//! `net_apply`: one NetworkManager change (wifi, IPv4, DNS, interface, LTE,
//! connection) that can't strand the device.
//!
//! Before anything changes, a NetworkManager checkpoint is created over D-Bus
//! with a rollback timeout: NetworkManager itself restores every device's
//! configuration (and deletes connections made since, flag 2) when the timer
//! fires, even if this app dies. The change is applied with nmcli, then the
//! platform is polled; only once it answers is the checkpoint destroyed,
//! which keeps the change. If it never answers, the checkpoint is left for
//! NetworkManager's timer, and once that has fired the checkpoint is checked
//! gone (rolled back explicitly if it isn't). A failed apply rolls back
//! straight away.
//!
//! The pure parts (params, the nmcli / busctl / dbus-send argv, the output
//! parsers) are unit-tested here; the flow is tested through the RPC in
//! `tests/net_apply_rpc.rs`.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use doover::rpc::RpcError;
use serde_json::{json, Value};

use crate::diag::{self, check, invalid, label, Place, Runner, Step, TOOL_TIMEOUT};
use crate::parse::{self, Params};

pub const DEFAULT_ROLLBACK: u32 = 90;
pub const MIN_ROLLBACK: u32 = 30;
pub const MAX_ROLLBACK: u32 = 300;
/// Stop verifying this long before the rollback fires, so a late success
/// can't race NetworkManager's timer.
const VERIFY_MARGIN: f64 = 10.0;
const VERIFY_POLL: f64 = 3.0;
/// After the deadline, how long to give NetworkManager to finish rolling
/// back before reading the network state.
const ROLLBACK_GRACE: f64 = 2.0;
/// NetworkManager's `CheckpointCreate` flag DELETE_NEW_CONNECTIONS: a wifi or
/// LTE profile added after the checkpoint is removed on rollback.
const CHECKPOINT_FLAGS: &str = "2";

const NM_DEST: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const CHECKPOINT_PREFIX: &str = "/org/freedesktop/NetworkManager/Checkpoint/";

/// The unit every `net_apply` wait is counted in: one second, except in tests,
/// which shrink it so a 30 s rollback window passes in a fraction of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetTiming {
    pub second: Duration,
}

impl Default for NetTiming {
    fn default() -> Self {
        Self {
            second: Duration::from_secs(1),
        }
    }
}

impl NetTiming {
    fn secs(&self, n: f64) -> Duration {
        self.second.mul_f64(n)
    }
}

// -- params ------------------------------------------------------------------

/// A connection named directly, or the one active on an interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Connection(String),
    Interface(String),
}

impl Target {
    fn name(&self) -> &str {
        match self {
            Target::Connection(n) | Target::Interface(n) => n,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetOp {
    WifiConnect {
        ssid: String,
        password: Option<String>,
        interface: Option<String>,
    },
    Ipv4 {
        target: Target,
        /// `None` = DHCP ("auto"); otherwise the static address and prefix.
        manual: Option<(Ipv4Addr, u8)>,
        gateway: Option<Ipv4Addr>,
        /// `None` leaves the servers as they are; `Some([])` clears them.
        dns: Option<Vec<Ipv4Addr>>,
    },
    Dns {
        target: Target,
        servers: Vec<Ipv4Addr>,
    },
    Interface {
        name: String,
        up: bool,
    },
    Lte {
        apn: String,
        user: Option<String>,
        password: Option<String>,
        connection: Option<String>,
    },
    Connection {
        name: String,
        up: bool,
    },
}

impl NetOp {
    pub fn name(&self) -> &'static str {
        match self {
            NetOp::WifiConnect { .. } => "wifi_connect",
            NetOp::Ipv4 { .. } => "ipv4",
            NetOp::Dns { .. } => "dns",
            NetOp::Interface { .. } => "interface",
            NetOp::Lte { .. } => "lte",
            NetOp::Connection { .. } => "connection",
        }
    }

    /// Bringing something up can't cut the device off, so it may go ahead
    /// without a checkpoint when there's no D-Bus tool to make one.
    pub fn harmless(&self) -> bool {
        matches!(
            self,
            NetOp::Interface { up: true, .. } | NetOp::Connection { up: true, .. }
        )
    }

    /// What the change is to, for progress: "Applying ipv4 on eth0".
    pub fn target(&self) -> String {
        match self {
            NetOp::WifiConnect { ssid, .. } => ssid.clone(),
            NetOp::Ipv4 { target, .. } | NetOp::Dns { target, .. } => target.name().into(),
            NetOp::Interface { name, .. } | NetOp::Connection { name, .. } => name.clone(),
            NetOp::Lte {
                connection, apn, ..
            } => connection.clone().unwrap_or_else(|| apn.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetApplyParams {
    pub op: NetOp,
    /// Seconds, clamped to 30..=300.
    pub rollback_after: u32,
}

const OPS: &[&str] = &[
    "wifi_connect",
    "ipv4",
    "dns",
    "interface",
    "lte",
    "connection",
];

/// Interface names: `[A-Za-z0-9_.:-]{1,64}`, not starting with `-`.
pub fn valid_iface(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
}

/// Connection (profile) names: the interface characters plus spaces, since
/// NetworkManager's own defaults are "Wired connection 1" and the like.
/// Still no shell metacharacters, and no leading/trailing space.
pub fn valid_connection(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && !s.starts_with(['-', ' '])
        && !s.ends_with(' ')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:- ".contains(c))
}

/// `a.b.c.d/nn` (prefix 1-32), or a bare address with `prefix` given
/// separately (as the control backend sends it).
pub fn parse_cidr(address: &str, prefix: Option<i64>) -> Result<(Ipv4Addr, u8), String> {
    let (addr, p) = match (address.split_once('/'), prefix) {
        (Some(_), Some(_)) => {
            return Err("give the prefix in 'address' or in 'prefix', not both".into())
        }
        (Some((a, p)), None) => (
            a,
            p.parse::<i64>()
                .map_err(|_| format!("bad prefix in 'address' {address:?}"))?,
        ),
        (None, Some(p)) => (address, p),
        (None, None) => {
            return Err(format!(
                "'address' {address:?} needs a prefix: \"a.b.c.d/nn\""
            ))
        }
    };
    let addr: Ipv4Addr = addr
        .trim()
        .parse()
        .map_err(|_| format!("'address' {address:?} is not an IPv4 address"))?;
    if !(1..=32).contains(&p) {
        return Err(format!("prefix must be 1 to 32, not {p}"));
    }
    Ok((addr, p as u8))
}

fn ipv4(v: &Value, key: &str) -> Result<Ipv4Addr, String> {
    v.as_str()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| format!("'{key}' must be IPv4 addresses"))
}

fn ip_list(p: &Params, key: &str, min: usize) -> Result<Option<Vec<Ipv4Addr>>, String> {
    let Some(v) = p.get(key) else {
        return Ok(None);
    };
    let list = v
        .as_array()
        .ok_or_else(|| format!("'{key}' must be a list of IPv4 addresses"))?;
    if list.len() < min || list.len() > 4 {
        return Err(format!("'{key}' takes {min} to 4 addresses"));
    }
    list.iter()
        .map(|v| ipv4(v, key))
        .collect::<Result<_, _>>()
        .map(Some)
}

fn iface(p: &Params, key: &str) -> Result<Option<String>, String> {
    match p.str(key)? {
        None => Ok(None),
        Some(s) if valid_iface(s) => Ok(Some(s.to_string())),
        Some(s) => Err(format!("'{key}' {s:?} is not an interface name")),
    }
}

fn connection(p: &Params, key: &str) -> Result<Option<String>, String> {
    match p.str(key)? {
        None => Ok(None),
        Some(s) if valid_connection(s) => Ok(Some(s.to_string())),
        Some(s) => Err(format!(
            "'{key}' {s:?} is not a connection name (letters, digits, spaces, _ . : -)"
        )),
    }
}

/// `connection` or `interface`, exactly one.
fn target(p: &Params) -> Result<Target, String> {
    match (connection(p, "connection")?, iface(p, "interface")?) {
        (Some(c), None) => Ok(Target::Connection(c)),
        (None, Some(i)) => Ok(Target::Interface(i)),
        (Some(_), Some(_)) => Err("give 'connection' or 'interface', not both".into()),
        (None, None) => Err("'connection' or 'interface' is required".into()),
    }
}

fn up_down(p: &Params) -> Result<bool, String> {
    match p.choice("state", &["up", "down"])? {
        Some(s) => Ok(s == "up"),
        None => Err("'state' (\"up\" or \"down\") is required".into()),
    }
}

/// Free text (a password, a username): 1-100 characters, no control
/// characters, not starting with `-`.
fn secret(p: &Params, key: &str) -> Result<Option<String>, String> {
    match p.str(key)? {
        None | Some("") => Ok(None),
        Some(s) if s.chars().count() <= 100 && parse::safe_text(s) => Ok(Some(s.to_string())),
        Some(_) => Err(format!(
            "'{key}' must be at most 100 characters, without control characters or a leading '-'"
        )),
    }
}

impl NetApplyParams {
    pub fn parse(payload: &Value) -> Result<Self, RpcError> {
        Self::parse_inner(payload).map_err(invalid)
    }

    fn parse_inner(payload: &Value) -> Result<Self, String> {
        let p = Params::new(payload)?;
        let op = p
            .choice("op", OPS)?
            .ok_or_else(|| format!("'op' is required: one of {}", OPS.join(", ")))?;
        let rollback_after = match p.number("rollback_after")? {
            None => DEFAULT_ROLLBACK,
            Some(n) if n.is_finite() && n > 0.0 => {
                (n.round() as u32).clamp(MIN_ROLLBACK, MAX_ROLLBACK)
            }
            Some(_) => return Err("'rollback_after' must be a positive number".into()),
        };
        let keys = |extra: &[&str]| {
            let mut known = vec!["op", "rollback_after"];
            known.extend_from_slice(extra);
            p.only(&known)
        };
        let op = match op.as_str() {
            "wifi_connect" => {
                keys(&["ssid", "password", "interface"])?;
                let ssid = p.required_str("ssid")?;
                if ssid.len() > 32 || !parse::safe_text(ssid) {
                    return Err(
                        "'ssid' must be 1 to 32 bytes, without control characters or a leading '-'"
                            .into(),
                    );
                }
                let password = match p.str("password")? {
                    None | Some("") => None,
                    Some(pw) => {
                        let n = pw.chars().count();
                        if !(8..=64).contains(&n) || !parse::safe_text(pw) {
                            return Err("a wifi 'password' is 8 to 64 characters".into());
                        }
                        Some(pw.to_string())
                    }
                };
                NetOp::WifiConnect {
                    ssid: ssid.to_string(),
                    password,
                    interface: iface(&p, "interface")?,
                }
            }
            "ipv4" => {
                keys(&[
                    "connection",
                    "interface",
                    "method",
                    "address",
                    "prefix",
                    "gateway",
                    "dns",
                ])?;
                let target = target(&p)?;
                let method = p
                    .choice("method", &["auto", "manual"])?
                    .ok_or("'method' (\"auto\" or \"manual\") is required")?;
                let gateway = p.get("gateway").map(|g| ipv4(g, "gateway")).transpose()?;
                let manual = if method == "manual" {
                    let address = p.required_str("address")?;
                    Some(parse_cidr(address, p.int("prefix", i64::MIN, i64::MAX)?)?)
                } else {
                    if p.has("address") || p.has("prefix") || gateway.is_some() {
                        return Err(
                            "'address', 'prefix' and 'gateway' are for method \"manual\"".into(),
                        );
                    }
                    None
                };
                NetOp::Ipv4 {
                    target,
                    manual,
                    gateway,
                    dns: ip_list(&p, "dns", 0)?,
                }
            }
            "dns" => {
                keys(&["connection", "interface", "servers"])?;
                NetOp::Dns {
                    target: target(&p)?,
                    servers: ip_list(&p, "servers", 1)?
                        .ok_or("'servers' (1 to 4 IPv4 addresses) is required")?,
                }
            }
            "interface" => {
                keys(&["name", "state"])?;
                NetOp::Interface {
                    name: iface(&p, "name")?.ok_or("'name' is required")?,
                    up: up_down(&p)?,
                }
            }
            "lte" => {
                keys(&["apn", "user", "password", "connection"])?;
                let apn = p.required_str("apn")?;
                if apn.len() > 100
                    || apn.starts_with('-')
                    || !apn
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                {
                    return Err(format!(
                        "'apn' {apn:?} is not an APN (letters, digits, . _ -)"
                    ));
                }
                NetOp::Lte {
                    apn: apn.to_string(),
                    user: secret(&p, "user")?,
                    password: secret(&p, "password")?,
                    connection: connection(&p, "connection")?,
                }
            }
            "connection" => {
                keys(&["name", "state"])?;
                NetOp::Connection {
                    name: connection(&p, "name")?.ok_or("'name' is required")?,
                    up: up_down(&p)?,
                }
            }
            _ => unreachable!("choice() checked the op"),
        };
        Ok(Self { op, rollback_after })
    }
}

// -- nmcli -------------------------------------------------------------------

/// How long nmcli waits for an activation (`-w`), in seconds: long enough
/// for wifi association and DHCP, short enough to leave time to verify
/// inside the rollback window.
pub fn nmcli_wait(rollback_after: u32) -> u32 {
    rollback_after.saturating_sub(20).clamp(10, 45)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn nmcli_waiting(wait: u32, rest: &[&str]) -> Vec<String> {
    let mut argv = s(&["nmcli", "-w", &wait.to_string()]);
    argv.extend(s(rest));
    argv
}

fn dns_list(servers: &[Ipv4Addr]) -> String {
    servers
        .iter()
        .map(Ipv4Addr::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The connection an op changes, when it isn't given by name: the profile
/// active on the interface (`ipv4`, `dns`), or an existing gsm profile
/// (`lte`; `None` there means one is created).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resolved {
    pub connection: Option<String>,
}

/// Name of the gsm profile `lte` creates when there's none.
pub const LTE_CONNECTION: &str = "lte";

/// The nmcli commands for `op`, in order.
pub fn commands(op: &NetOp, resolved: &Resolved, wait: u32) -> Vec<Vec<String>> {
    let con = || {
        resolved
            .connection
            .clone()
            .expect("resolve() found the connection")
    };
    match op {
        NetOp::WifiConnect {
            ssid,
            password,
            interface,
        } => {
            let mut argv = nmcli_waiting(wait, &["dev", "wifi", "connect", ssid]);
            if let Some(pw) = password {
                argv.extend(s(&["password", pw]));
            }
            if let Some(i) = interface {
                argv.extend(s(&["ifname", i]));
            }
            vec![argv]
        }
        NetOp::Ipv4 {
            manual,
            gateway,
            dns,
            ..
        } => {
            let con = con();
            let mut modify = s(&["nmcli", "con", "mod", &con]);
            match manual {
                Some((addr, prefix)) => {
                    let gw = gateway.map(|g| g.to_string()).unwrap_or_default();
                    modify.extend(s(&[
                        "ipv4.method",
                        "manual",
                        "ipv4.addresses",
                        &format!("{addr}/{prefix}"),
                        "ipv4.gateway",
                        &gw,
                    ]));
                }
                // Back to DHCP: static addresses would otherwise stay on as
                // extra addresses.
                None => modify.extend(s(&[
                    "ipv4.method",
                    "auto",
                    "ipv4.addresses",
                    "",
                    "ipv4.gateway",
                    "",
                ])),
            }
            if let Some(dns) = dns {
                modify.extend(s(&["ipv4.dns", &dns_list(dns)]));
            }
            vec![modify, nmcli_waiting(wait, &["con", "up", &con])]
        }
        NetOp::Dns { servers, .. } => {
            let con = con();
            vec![
                s(&[
                    "nmcli",
                    "con",
                    "mod",
                    &con,
                    "ipv4.dns",
                    &dns_list(servers),
                    "ipv4.ignore-auto-dns",
                    "yes",
                ]),
                nmcli_waiting(wait, &["con", "up", &con]),
            ]
        }
        NetOp::Interface { name, up } => vec![nmcli_waiting(
            wait,
            &["dev", if *up { "connect" } else { "disconnect" }, name],
        )],
        NetOp::Connection { name, up } => vec![nmcli_waiting(
            wait,
            &["con", if *up { "up" } else { "down" }, name],
        )],
        NetOp::Lte {
            apn,
            user,
            password,
            connection,
        } => {
            let mut creds = Vec::new();
            if let Some(u) = user {
                creds.extend(s(&["gsm.username", u]));
            }
            if let Some(pw) = password {
                creds.extend(s(&["gsm.password", pw]));
            }
            let (name, first) = match &resolved.connection {
                Some(existing) => {
                    let mut argv = s(&["nmcli", "con", "mod", existing, "gsm.apn", apn]);
                    argv.extend(creds);
                    (existing.clone(), argv)
                }
                None => {
                    let name = connection.clone().unwrap_or_else(|| LTE_CONNECTION.into());
                    let mut argv = s(&[
                        "nmcli", "con", "add", "type", "gsm", "ifname", "*", "con-name", &name,
                        "gsm.apn", apn,
                    ]);
                    argv.extend(creds);
                    (name, argv)
                }
            };
            vec![first, nmcli_waiting(wait, &["con", "up", &name])]
        }
    }
}

/// `nmcli -t -f NAME,DEVICE con show --active`: the profile on `device`.
pub fn active_connection_on(out: &str, device: &str) -> Option<String> {
    out.lines()
        .map(parse::split_terse)
        .find(|f| f.len() >= 2 && f[1] == device)
        .map(|f| f[0].clone())
}

/// `nmcli -t -f NAME,TYPE con show`: the profile to change for `lte` -- the
/// one named, if it exists, else the first gsm profile.
pub fn lte_connection(out: &str, wanted: Option<&str>) -> Option<String> {
    let rows: Vec<Vec<String>> = out
        .lines()
        .map(parse::split_terse)
        .filter(|f| f.len() >= 2)
        .collect();
    match wanted {
        Some(name) => rows.iter().find(|f| f[0] == name).map(|f| f[0].clone()),
        None => rows.iter().find(|f| f[1] == "gsm").map(|f| f[0].clone()),
    }
}

// -- checkpoint --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbusTool {
    Busctl,
    DbusSend,
}

/// `CheckpointCreate(ao devices, u rollback_timeout, u flags) -> o`, for
/// every device (an empty list).
pub fn checkpoint_create_argv(tool: DbusTool, rollback_after: u32) -> Vec<String> {
    let t = rollback_after.to_string();
    match tool {
        DbusTool::Busctl => s(&[
            "busctl",
            "call",
            NM_DEST,
            NM_PATH,
            NM_DEST,
            "CheckpointCreate",
            "aouu",
            "0",
            &t,
            CHECKPOINT_FLAGS,
        ]),
        DbusTool::DbusSend => s(&[
            "dbus-send",
            "--system",
            "--print-reply",
            &format!("--dest={NM_DEST}"),
            NM_PATH,
            &format!("{NM_DEST}.CheckpointCreate"),
            "array:objpath:",
            &format!("uint32:{t}"),
            &format!("uint32:{CHECKPOINT_FLAGS}"),
        ]),
    }
}

/// `CheckpointDestroy(o)` / `CheckpointRollback(o)`.
pub fn checkpoint_argv(tool: DbusTool, method: &str, path: &str) -> Vec<String> {
    match tool {
        DbusTool::Busctl => s(&[
            "busctl", "call", NM_DEST, NM_PATH, NM_DEST, method, "o", path,
        ]),
        DbusTool::DbusSend => s(&[
            "dbus-send",
            "--system",
            "--print-reply",
            &format!("--dest={NM_DEST}"),
            NM_PATH,
            &format!("{NM_DEST}.{method}"),
            &format!("objpath:{path}"),
        ]),
    }
}

/// The checkpoint's object path from `CheckpointCreate`'s reply: busctl's
/// `o "/org/freedesktop/NetworkManager/Checkpoint/1"` or dbus-send's
/// `object path "/org/freedesktop/NetworkManager/Checkpoint/1"`.
pub fn parse_checkpoint_path(out: &str) -> Option<String> {
    let start = out.find(CHECKPOINT_PREFIX)?;
    let rest = &out[start + CHECKPOINT_PREFIX.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest[digits.len()..].chars().next();
    (!digits.is_empty() && matches!(after, None | Some('"') | Some(' ') | Some('\n')))
        .then(|| format!("{CHECKPOINT_PREFIX}{digits}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub tool: DbusTool,
    pub path: String,
}

enum CheckpointError {
    /// Neither busctl nor dbus-send can be run.
    Unavailable(String),
    /// One ran and NetworkManager refused (or the reply made no sense).
    Failed(String),
}

async fn create_checkpoint(
    runner: &Runner,
    rollback_after: u32,
) -> Result<Checkpoint, CheckpointError> {
    let mut missing = Vec::new();
    for tool in [DbusTool::Busctl, DbusTool::DbusSend] {
        let argv = checkpoint_create_argv(tool, rollback_after);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match runner
            .run(Place::HostThenContainer, &argv, None, TOOL_TIMEOUT)
            .await
        {
            Err(e) => missing.push(e),
            Ok(r) => {
                check(&label(&argv), &r).map_err(CheckpointError::Failed)?;
                return parse_checkpoint_path(&r.stdout)
                    .map(|path| Checkpoint { tool, path })
                    .ok_or_else(|| {
                        CheckpointError::Failed(format!(
                            "CheckpointCreate: unexpected reply {:?}",
                            r.stdout.trim()
                        ))
                    });
            }
        }
    }
    Err(CheckpointError::Unavailable(missing.join("; ")))
}

async fn checkpoint_call(runner: &Runner, cp: &Checkpoint, method: &str) -> Result<(), String> {
    let argv = checkpoint_argv(cp.tool, method, &cp.path);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let r = runner
        .run(Place::HostThenContainer, &argv, None, TOOL_TIMEOUT)
        .await?;
    check(method, &r)
}

/// NetworkManager's `Checkpoints` property (the pending checkpoints).
pub fn checkpoints_argv(tool: DbusTool) -> Vec<String> {
    match tool {
        DbusTool::Busctl => s(&[
            "busctl",
            "get-property",
            NM_DEST,
            NM_PATH,
            NM_DEST,
            "Checkpoints",
        ]),
        DbusTool::DbusSend => s(&[
            "dbus-send",
            "--system",
            "--print-reply",
            &format!("--dest={NM_DEST}"),
            NM_PATH,
            "org.freedesktop.DBus.Properties.Get",
            &format!("string:{NM_DEST}"),
            "string:Checkpoints",
        ]),
    }
}

/// Whether `path` is still among the pending checkpoints, from busctl's
/// `ao 1 "/org/.../Checkpoint/4"` or dbus-send's `variant array [ object
/// path "/org/.../Checkpoint/4" ]`.
pub fn checkpoint_listed(out: &str, path: &str) -> bool {
    out.contains(&format!("\"{path}\""))
}

/// Wait until NetworkManager's rollback has fired (`by`), then make sure it
/// did: a checkpoint still pending is rolled back explicitly. A note for the
/// reason when that was needed, or when it couldn't be confirmed.
async fn await_rollback(
    runner: &Runner,
    step: &Step,
    cp: &Checkpoint,
    by: Instant,
) -> Option<String> {
    step.set("Waiting for NetworkManager to roll back");
    runner
        .sleep(by.saturating_duration_since(Instant::now()))
        .await;
    if runner.cancelled() {
        return None;
    }
    let argv = checkpoints_argv(cp.tool);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let listed = runner
        .stdout(Place::HostThenContainer, &argv, TOOL_TIMEOUT)
        .await
        .map(|out| checkpoint_listed(&out, &cp.path));
    match listed {
        Ok(false) => None,
        Ok(true) => Some(match checkpoint_call(runner, cp, "CheckpointRollback").await {
            Ok(()) => "the checkpoint was still pending at the deadline, so it was rolled back explicitly".into(),
            Err(e) => format!("the checkpoint was still pending at the deadline and CheckpointRollback failed ({e})"),
        }),
        Err(e) => Some(format!("couldn't confirm the rollback ({e})")),
    }
}

fn with_note(reason: String, note: Option<String>) -> String {
    match note {
        Some(note) => format!("{reason}; {note}"),
        None => reason,
    }
}

// -- the flow ----------------------------------------------------------------

/// Find the connection an op needs by looking, before anything changes.
async fn resolve(runner: &Runner, op: &NetOp) -> Result<Resolved, RpcError> {
    let failed = |e: String| RpcError::new("APPLY_FAILED", e);
    let place = Place::HostThenContainer;
    match op {
        NetOp::Ipv4 { target, .. } | NetOp::Dns { target, .. } => match target {
            Target::Connection(c) => Ok(Resolved {
                connection: Some(c.clone()),
            }),
            Target::Interface(i) => {
                let out = runner
                    .stdout(
                        place,
                        &[
                            "nmcli",
                            "-t",
                            "-f",
                            "NAME,DEVICE",
                            "con",
                            "show",
                            "--active",
                        ],
                        TOOL_TIMEOUT,
                    )
                    .await
                    .map_err(failed)?;
                let connection = active_connection_on(&out, i).ok_or_else(|| {
                    RpcError::new(
                        "NO_CONNECTION",
                        format!("no active connection on {i}; pass 'connection' instead"),
                    )
                })?;
                Ok(Resolved {
                    connection: Some(connection),
                })
            }
        },
        NetOp::Lte { connection, .. } => {
            let out = runner
                .stdout(
                    place,
                    &["nmcli", "-t", "-f", "NAME,TYPE", "con", "show"],
                    TOOL_TIMEOUT,
                )
                .await
                .map_err(failed)?;
            Ok(Resolved {
                connection: lte_connection(&out, connection.as_deref()),
            })
        }
        _ => Ok(Resolved::default()),
    }
}

/// Run the commands in order; the first failure's reason.
async fn apply(runner: &Runner, commands: &[Vec<String>], wait: u32) -> Result<(), String> {
    for argv in commands {
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let r = runner
            .run(
                Place::HostThenContainer,
                &argv,
                None,
                Duration::from_secs(u64::from(wait) + 10),
            )
            .await?;
        check(&label(&argv), &r)?;
    }
    Ok(())
}

/// Poll the platform until it answers or `until`; the last failure if it
/// never did.
async fn verify(runner: &Runner, until: Instant, poll: Duration) -> Result<(), String> {
    // A slow apply can use up the window: a check now could run after the
    // rollback and "verify" the old configuration.
    if Instant::now() >= until {
        return Err("the apply took the whole verification window".into());
    }
    loop {
        let last = match diag::platform_https(runner).await {
            (Ok(()), _) => return Ok(()),
            (Err(e), _) => e,
        };
        if runner.cancelled() || Instant::now() + poll >= until {
            return Err(last);
        }
        runner.sleep(poll).await;
    }
}

fn cancelled() -> RpcError {
    RpcError::new(
        "CANCELLED",
        "cancelled; any checkpoint rolls back at its deadline",
    )
}

struct Outcome {
    applied: bool,
    verified: bool,
    rolled_back: bool,
    reason: Option<String>,
}

/// Apply one change under a checkpoint; see the module docs.
pub async fn net_apply(
    runner: &Runner,
    step: &Step,
    params: NetApplyParams,
    timing: NetTiming,
) -> Result<Value, RpcError> {
    let op = &params.op;
    let rollback = params.rollback_after;
    let wait = nmcli_wait(rollback);

    step.set("Reading connections");
    let resolved = resolve(runner, op).await?;
    let commands = commands(op, &resolved, wait);

    step.set("Creating checkpoint");
    let started = Instant::now();
    let checkpoint = match create_checkpoint(runner, rollback).await {
        Ok(cp) => {
            step.set(format!("Checkpoint created (rollback in {rollback}s)"));
            Some(cp)
        }
        Err(CheckpointError::Unavailable(why)) if op.harmless() => {
            tracing::warn!("net_apply {} without a checkpoint: {why}", op.name());
            None
        }
        Err(CheckpointError::Unavailable(why)) => {
            return Err(RpcError::new(
                "NO_CHECKPOINT",
                format!(
                    "can't create a NetworkManager checkpoint (neither busctl nor dbus-send \
                     runs: {why}), so no change was made"
                ),
            ))
        }
        Err(CheckpointError::Failed(why)) => {
            return Err(RpcError::new(
                "CHECKPOINT_FAILED",
                format!("NetworkManager refused the checkpoint ({why}), so no change was made"),
            ))
        }
    };
    // NetworkManager's timer started between `started` and now: verify
    // against the earliest it can fire, wait out the latest.
    let fires_by = Instant::now() + timing.secs(f64::from(rollback));
    let verify_until = started + timing.secs(f64::from(rollback) - VERIFY_MARGIN);
    let rollback_by = fires_by + timing.secs(ROLLBACK_GRACE);

    step.set(format!("Applying {} on {}", op.name(), op.target()));
    let outcome = match apply(runner, &commands, wait).await {
        Err(why) if runner.cancelled() => {
            tracing::warn!("net_apply cancelled while applying: {why}");
            return Err(cancelled());
        }
        Err(why) => match &checkpoint {
            None => Outcome {
                applied: false,
                verified: false,
                rolled_back: false,
                reason: Some(why),
            },
            Some(cp) => {
                step.set("Apply failed; rolling back");
                let reason = match checkpoint_call(runner, cp, "CheckpointRollback").await {
                    Ok(()) => why,
                    Err(e) => with_note(
                        format!("{why}; CheckpointRollback failed ({e}), so NetworkManager rolled back at the deadline"),
                        await_rollback(runner, step, cp, rollback_by).await,
                    ),
                };
                Outcome {
                    applied: false,
                    verified: false,
                    rolled_back: true,
                    reason: Some(reason),
                }
            }
        },
        Ok(()) => {
            step.set("Verifying platform reachability");
            let verified = verify(runner, verify_until, timing.secs(VERIFY_POLL)).await;
            if runner.cancelled() {
                return Err(cancelled());
            }
            match (verified, &checkpoint) {
                (Ok(()), None) => Outcome {
                    applied: true,
                    verified: true,
                    rolled_back: false,
                    reason: None,
                },
                (Err(e), None) => Outcome {
                    applied: true,
                    verified: false,
                    rolled_back: false,
                    reason: Some(format!(
                        "platform unreachable after apply ({e}); no checkpoint, so the change stays"
                    )),
                },
                (Ok(()), Some(cp)) => {
                    match checkpoint_call(runner, cp, "CheckpointDestroy").await {
                        Ok(()) => Outcome {
                            applied: true,
                            verified: true,
                            rolled_back: false,
                            reason: None,
                        },
                        Err(e) => {
                            let note = await_rollback(runner, step, cp, rollback_by).await;
                            Outcome {
                                applied: false,
                                verified: false,
                                rolled_back: true,
                                reason: Some(with_note(
                                    format!(
                                        "the platform answered, but keeping the change failed \
                                         (CheckpointDestroy: {e}), so NetworkManager rolled it back"
                                    ),
                                    note,
                                )),
                            }
                        }
                    }
                }
                (Err(e), Some(cp)) => {
                    let note = await_rollback(runner, step, cp, rollback_by).await;
                    Outcome {
                        applied: false,
                        verified: false,
                        rolled_back: true,
                        reason: Some(with_note(
                            format!("platform unreachable after apply ({e})"),
                            note,
                        )),
                    }
                }
            }
        }
    };
    if runner.cancelled() {
        return Err(cancelled());
    }

    let status = diag::net_status(runner, step).await;
    Ok(json!({
        "applied": outcome.applied,
        "verified": outcome.verified,
        "rolled_back": outcome.rolled_back,
        "checkpoint": checkpoint.is_some(),
        "reason": outcome.reason,
        "status": status,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Result<NetApplyParams, String> {
        NetApplyParams::parse_inner(&v)
    }

    fn op(v: Value) -> NetOp {
        parse(v).unwrap().op
    }

    fn cmds(v: Value, resolved: Option<&str>) -> Vec<Vec<String>> {
        let p = parse(v).unwrap();
        commands(
            &p.op,
            &Resolved {
                connection: resolved.map(String::from),
            },
            nmcli_wait(p.rollback_after),
        )
    }

    #[test]
    fn rollback_after_default_and_clamp() {
        let r = |v: Value| parse(v).unwrap().rollback_after;
        assert_eq!(
            r(json!({"op": "connection", "name": "a", "state": "up"})),
            90
        );
        assert_eq!(
            r(json!({"op": "connection", "name": "a", "state": "up", "rollback_after": 5})),
            30
        );
        assert_eq!(
            r(json!({"op": "connection", "name": "a", "state": "up", "rollback_after": 1000})),
            300
        );
        assert_eq!(
            r(json!({"op": "connection", "name": "a", "state": "up", "rollback_after": 120})),
            120
        );
        assert!(parse(
            json!({"op": "connection", "name": "a", "state": "up", "rollback_after": "x"})
        )
        .is_err());
        assert!(parse(
            json!({"op": "connection", "name": "a", "state": "up", "rollback_after": -1})
        )
        .is_err());
    }

    #[test]
    fn wait_leaves_room_to_verify() {
        assert_eq!(nmcli_wait(30), 10);
        assert_eq!(nmcli_wait(60), 40);
        assert_eq!(nmcli_wait(90), 45);
        assert_eq!(nmcli_wait(300), 45);
    }

    #[test]
    fn wifi_connect_commands() {
        assert_eq!(
            cmds(json!({"op": "wifi_connect", "ssid": "Farm Office"}), None),
            [["nmcli", "-w", "45", "dev", "wifi", "connect", "Farm Office"]]
        );
        assert_eq!(
            cmds(
                json!({"op": "wifi_connect", "ssid": "Tom's", "password": "hunter2hunter2",
                       "interface": "wlan0", "rollback_after": 30}),
                None
            ),
            [[
                "nmcli",
                "-w",
                "10",
                "dev",
                "wifi",
                "connect",
                "Tom's",
                "password",
                "hunter2hunter2",
                "ifname",
                "wlan0"
            ]]
        );
        // Quoting keeps a hostile-looking SSID one argument.
        let argv = &cmds(json!({"op": "wifi_connect", "ssid": "a$(reboot)"}), None)[0];
        assert_eq!(
            crate::executor::shell_join(argv),
            "nmcli -w 45 dev wifi connect 'a$(reboot)'"
        );
    }

    #[test]
    fn ipv4_commands() {
        assert_eq!(
            cmds(
                json!({"op": "ipv4", "interface": "eth0", "method": "manual",
                       "address": "192.168.1.50/24", "gateway": "192.168.1.1",
                       "dns": ["1.1.1.1", "8.8.8.8"]}),
                Some("Wired connection 1")
            ),
            [
                vec![
                    "nmcli",
                    "con",
                    "mod",
                    "Wired connection 1",
                    "ipv4.method",
                    "manual",
                    "ipv4.addresses",
                    "192.168.1.50/24",
                    "ipv4.gateway",
                    "192.168.1.1",
                    "ipv4.dns",
                    "1.1.1.1 8.8.8.8"
                ],
                vec!["nmcli", "-w", "45", "con", "up", "Wired connection 1"],
            ]
        );
        // The backend's split address + prefix; no gateway clears it.
        assert_eq!(
            cmds(
                json!({"op": "ipv4", "connection": "lan", "method": "manual",
                       "address": "10.0.0.2", "prefix": 8}),
                Some("lan")
            )[0],
            [
                "nmcli",
                "con",
                "mod",
                "lan",
                "ipv4.method",
                "manual",
                "ipv4.addresses",
                "10.0.0.2/8",
                "ipv4.gateway",
                ""
            ]
        );
        assert_eq!(
            cmds(
                json!({"op": "ipv4", "connection": "lan", "method": "auto"}),
                Some("lan")
            )[0],
            [
                "nmcli",
                "con",
                "mod",
                "lan",
                "ipv4.method",
                "auto",
                "ipv4.addresses",
                "",
                "ipv4.gateway",
                ""
            ]
        );
    }

    #[test]
    fn dns_commands() {
        assert_eq!(
            cmds(
                json!({"op": "dns", "interface": "eth0", "servers": ["1.1.1.1"]}),
                Some("Wired connection 1")
            ),
            [
                vec![
                    "nmcli",
                    "con",
                    "mod",
                    "Wired connection 1",
                    "ipv4.dns",
                    "1.1.1.1",
                    "ipv4.ignore-auto-dns",
                    "yes"
                ],
                vec!["nmcli", "-w", "45", "con", "up", "Wired connection 1"],
            ]
        );
    }

    #[test]
    fn interface_and_connection_commands() {
        assert_eq!(
            cmds(
                json!({"op": "interface", "name": "wlan0", "state": "down"}),
                None
            ),
            [["nmcli", "-w", "45", "dev", "disconnect", "wlan0"]]
        );
        assert_eq!(
            cmds(
                json!({"op": "interface", "name": "wlan0", "state": "UP"}),
                None
            ),
            [["nmcli", "-w", "45", "dev", "connect", "wlan0"]]
        );
        assert_eq!(
            cmds(
                json!({"op": "connection", "name": "Farm Office", "state": "down"}),
                None
            ),
            [["nmcli", "-w", "45", "con", "down", "Farm Office"]]
        );
        assert_eq!(
            cmds(
                json!({"op": "connection", "name": "lte", "state": "up"}),
                None
            ),
            [["nmcli", "-w", "45", "con", "up", "lte"]]
        );
    }

    #[test]
    fn lte_commands() {
        // No gsm profile: create one.
        assert_eq!(
            cmds(json!({"op": "lte", "apn": "telstra.internet"}), None),
            [
                vec![
                    "nmcli",
                    "con",
                    "add",
                    "type",
                    "gsm",
                    "ifname",
                    "*",
                    "con-name",
                    "lte",
                    "gsm.apn",
                    "telstra.internet"
                ],
                vec!["nmcli", "-w", "45", "con", "up", "lte"],
            ]
        );
        // An existing one: modify it.
        assert_eq!(
            cmds(
                json!({"op": "lte", "apn": "m2m.example", "user": "u", "password": "p"}),
                Some("Telstra")
            ),
            [
                vec![
                    "nmcli",
                    "con",
                    "mod",
                    "Telstra",
                    "gsm.apn",
                    "m2m.example",
                    "gsm.username",
                    "u",
                    "gsm.password",
                    "p"
                ],
                vec!["nmcli", "-w", "45", "con", "up", "Telstra"],
            ]
        );
        assert_eq!(
            cmds(json!({"op": "lte", "apn": "x", "connection": "cell"}), None)[0][8],
            "cell"
        );
        assert_eq!(
            crate::executor::shell_join(&cmds(json!({"op": "lte", "apn": "x"}), None)[0]),
            "nmcli con add type gsm ifname '*' con-name lte gsm.apn x"
        );
    }

    #[test]
    fn harmless_ops() {
        assert!(op(json!({"op": "interface", "name": "eth0", "state": "up"})).harmless());
        assert!(op(json!({"op": "connection", "name": "a", "state": "up"})).harmless());
        assert!(!op(json!({"op": "interface", "name": "eth0", "state": "down"})).harmless());
        assert!(!op(json!({"op": "wifi_connect", "ssid": "a"})).harmless());
        assert!(!op(json!({"op": "lte", "apn": "a"})).harmless());
    }

    #[test]
    fn bad_params() {
        for bad in [
            json!("x"),
            json!({}),
            json!({"op": "reboot"}),
            json!({"op": "wifi_connect"}),
            json!({"op": "wifi_connect", "ssid": "x".repeat(33)}),
            json!({"op": "wifi_connect", "ssid": "a\nb"}),
            json!({"op": "wifi_connect", "ssid": "-rf"}),
            json!({"op": "wifi_connect", "ssid": "a", "password": "short"}),
            json!({"op": "wifi_connect", "ssid": "a", "interface": "wlan0; reboot"}),
            json!({"op": "wifi_connect", "ssid": "a", "bssid": "x"}),
            json!({"op": "ipv4", "method": "auto"}),
            json!({"op": "ipv4", "interface": "eth0"}),
            json!({"op": "ipv4", "interface": "eth0", "connection": "x", "method": "auto"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "static"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "manual"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "manual", "address": "10.0.0.1"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "manual", "address": "10.0.0.1/0"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "manual", "address": "10.0.0.1/33"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "manual", "address": "10.0.0.1/8", "prefix": 8}),
            json!({"op": "ipv4", "interface": "eth0", "method": "manual", "address": "10.0.0.300/8"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "manual", "address": "10.0.0.1/8", "gateway": "x"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "auto", "address": "10.0.0.1/8"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "auto", "dns": "1.1.1.1"}),
            json!({"op": "ipv4", "interface": "eth0", "method": "auto", "dns": ["1.1.1.1", "1.1.1.2", "1.1.1.3", "1.1.1.4", "1.1.1.5"]}),
            json!({"op": "ipv4", "interface": "eth0", "method": "auto", "dns": ["2606:4700::1111"]}),
            json!({"op": "ipv4", "connection": "a;b", "method": "auto"}),
            json!({"op": "ipv4", "connection": "x".repeat(65), "method": "auto"}),
            json!({"op": "dns", "interface": "eth0"}),
            json!({"op": "dns", "interface": "eth0", "servers": []}),
            json!({"op": "dns", "interface": "eth0", "servers": ["1.1.1.1 8.8.8.8"]}),
            json!({"op": "interface", "name": "eth0"}),
            json!({"op": "interface", "name": "eth0", "state": "sideways"}),
            json!({"op": "interface", "name": "eth 0", "state": "up"}),
            json!({"op": "interface", "name": "-eth0", "state": "up"}),
            json!({"op": "lte"}),
            json!({"op": "lte", "apn": "a b"}),
            json!({"op": "lte", "apn": "a", "password": "p\n"}),
            json!({"op": "connection", "name": "$(x)", "state": "up"}),
            json!({"op": "connection", "name": " lead", "state": "up"}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn good_params() {
        assert_eq!(
            op(
                json!({"op": "ipv4", "interface": "eth0", "method": "manual",
                      "address": "192.168.1.50/24", "dns": []})
            ),
            NetOp::Ipv4 {
                target: Target::Interface("eth0".into()),
                manual: Some(("192.168.1.50".parse().unwrap(), 24)),
                gateway: None,
                dns: Some(vec![]),
            }
        );
        assert_eq!(
            op(json!({"op": "wifi_connect", "ssid": "Farm", "password": ""})),
            NetOp::WifiConnect {
                ssid: "Farm".into(),
                password: None,
                interface: None
            }
        );
        assert!(valid_connection("Wired connection 1"));
        assert!(valid_connection("Farm: Office"));
        assert!(!valid_connection("Tom's"));
        assert!(valid_iface("wwan0.1:x_y-z"));
    }

    #[test]
    fn active_connection_lookup() {
        let out = "Wired connection 1:eth0\nFarm\\: Office:wlan0\nlo:lo\n";
        assert_eq!(
            active_connection_on(out, "eth0").as_deref(),
            Some("Wired connection 1")
        );
        assert_eq!(
            active_connection_on(out, "wlan0").as_deref(),
            Some("Farm: Office")
        );
        assert_eq!(active_connection_on(out, "wwan0"), None);
    }

    #[test]
    fn lte_lookup() {
        let out = "Wired connection 1:802-3-ethernet\nTelstra:gsm\nother:gsm\n";
        assert_eq!(lte_connection(out, None).as_deref(), Some("Telstra"));
        assert_eq!(lte_connection(out, Some("other")).as_deref(), Some("other"));
        assert_eq!(lte_connection(out, Some("cell")), None);
        assert_eq!(lte_connection("eth:802-3-ethernet\n", None), None);
    }

    #[test]
    fn checkpoint_commands() {
        assert_eq!(
            crate::executor::shell_join(&checkpoint_create_argv(DbusTool::Busctl, 90)),
            "busctl call org.freedesktop.NetworkManager /org/freedesktop/NetworkManager \
             org.freedesktop.NetworkManager CheckpointCreate aouu 0 90 2"
        );
        assert_eq!(
            crate::executor::shell_join(&checkpoint_create_argv(DbusTool::DbusSend, 30)),
            "dbus-send --system --print-reply --dest=org.freedesktop.NetworkManager \
             /org/freedesktop/NetworkManager org.freedesktop.NetworkManager.CheckpointCreate \
             array:objpath: uint32:30 uint32:2"
        );
        let path = "/org/freedesktop/NetworkManager/Checkpoint/7";
        assert_eq!(
            checkpoint_argv(DbusTool::Busctl, "CheckpointDestroy", path),
            [
                "busctl",
                "call",
                NM_DEST,
                NM_PATH,
                NM_DEST,
                "CheckpointDestroy",
                "o",
                path
            ]
        );
        assert_eq!(
            checkpoint_argv(DbusTool::DbusSend, "CheckpointRollback", path)[5..],
            [
                "org.freedesktop.NetworkManager.CheckpointRollback",
                "objpath:/org/freedesktop/NetworkManager/Checkpoint/7"
            ]
        );
    }

    #[test]
    fn pending_checkpoints() {
        let path = "/org/freedesktop/NetworkManager/Checkpoint/4";
        assert_eq!(
            crate::executor::shell_join(&checkpoints_argv(DbusTool::Busctl)),
            "busctl get-property org.freedesktop.NetworkManager /org/freedesktop/NetworkManager \
             org.freedesktop.NetworkManager Checkpoints"
        );
        assert!(checkpoint_listed(
            "ao 2 \"/org/freedesktop/NetworkManager/Checkpoint/3\" \"/org/freedesktop/NetworkManager/Checkpoint/4\"\n",
            path
        ));
        assert!(!checkpoint_listed("ao 0\n", path));
        assert!(!checkpoint_listed(
            "ao 1 \"/org/freedesktop/NetworkManager/Checkpoint/41\"\n",
            path
        ));
        let dbus_send = "method return time=1727241600.3 sender=:1.7 -> destination=:1.9 serial=7 reply_serial=2\n   variant       array [\n         object path \"/org/freedesktop/NetworkManager/Checkpoint/4\"\n      ]\n";
        assert!(checkpoint_listed(dbus_send, path));
        assert!(!checkpoint_listed(
            "method return time=1 sender=:1.7 -> destination=:1.9 serial=7 reply_serial=2\n   variant       array [\n      ]\n",
            path
        ));
    }

    #[test]
    fn checkpoint_replies() {
        assert_eq!(
            parse_checkpoint_path("o \"/org/freedesktop/NetworkManager/Checkpoint/1\"\n")
                .as_deref(),
            Some("/org/freedesktop/NetworkManager/Checkpoint/1")
        );
        let dbus_send = "method return time=1727241600.123456 sender=:1.7 -> destination=:1.212 serial=4711 reply_serial=2\n   object path \"/org/freedesktop/NetworkManager/Checkpoint/12\"\n";
        assert_eq!(
            parse_checkpoint_path(dbus_send).as_deref(),
            Some("/org/freedesktop/NetworkManager/Checkpoint/12")
        );
        for bad in [
            "",
            "o \"/org/freedesktop/NetworkManager/Checkpoint/\"",
            "o \"/org/freedesktop/NetworkManager/Checkpoint/1x\"",
            "o \"/org/freedesktop/NetworkManager/Devices/3\"",
            "Call failed: Access denied",
        ] {
            assert_eq!(parse_checkpoint_path(bad), None, "{bad}");
        }
    }
}
