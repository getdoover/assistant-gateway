//! The typed diagnostic methods: `net_status`, `net_wifi_scan` and
//! `scan_network`. Each runs fixed tools through [`run_command`] -- the same
//! spawn path as `exec`, on the host or in the container -- and parses their
//! output into JSON with [`crate::parse`].

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use doover::rpc::{RpcContext, RpcError};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::executor::{run_command, shell_join, CommandResult, CommandSpec, LiveOutput};
use crate::parse::{self, IpLink, Route, Subnet};

/// Output captured per tool. Internal only (it's parsed, not returned), so
/// generous: an nmap sweep of a /22 is tens of KB.
const TOOL_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
const WIFI_SCAN_TIMEOUT: Duration = Duration::from_secs(30);
/// The whole of `scan_network`: discovery plus the optional port scan.
pub const SCAN_BUDGET: Duration = Duration::from_secs(60);
/// Checked by `net_status`.
const PLATFORM_HOST: &str = "api.doover.com";
const PLATFORM_URL: &str = "https://api.doover.com/";
const INTERNET_PING: &str = "1.1.1.1";

/// Where a tool runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// The host's namespaces, the host's binaries.
    Host,
    /// This container (Alpine, with the tools the image installs).
    Container,
    /// The host, falling back to the container when the host lacks the tool
    /// or can't be entered. The container shares the host's network
    /// (`network_mode: host`), so network tools see the same thing.
    HostThenContainer,
}

impl Place {
    fn attempts(self) -> &'static [bool] {
        match self {
            Place::Host => &[true],
            Place::Container => &[false],
            Place::HostThenContainer => &[true, false],
        }
    }
}

fn place_name(on_host: bool) -> &'static str {
    if on_host {
        "host"
    } else {
        "container"
    }
}

/// The current step of a running method, for its progress updates.
#[derive(Clone, Default)]
pub struct Step(Arc<Mutex<(String, u64)>>);

impl Step {
    pub fn new(text: &str) -> Self {
        Self(Arc::new(Mutex::new((text.to_string(), 0))))
    }

    pub fn set(&self, text: impl Into<String>) {
        let mut step = self.0.lock().unwrap();
        step.0 = text.into();
        step.1 += 1;
    }

    /// The text and a version that changes with every `set`.
    pub fn get(&self) -> (String, u64) {
        self.0.lock().unwrap().clone()
    }
}

/// Runs tools for one request, stopping when it's cancelled.
pub struct Runner {
    ctx: RpcContext,
    /// `run_on_host`: when false, nothing enters the host's namespaces and
    /// [`Place::HostThenContainer`] means the container.
    allow_host: bool,
}

impl Runner {
    pub fn new(ctx: RpcContext, allow_host: bool) -> Self {
        Self { ctx, allow_host }
    }

    pub fn cancelled(&self) -> bool {
        self.ctx.is_cancelled()
    }

    /// Run `argv` at `place`. `Err` is a one-line reason ("mmcli: not found
    /// on host") for the method's `errors`; a non-zero exit is still `Ok`.
    pub async fn run(
        &self,
        place: Place,
        argv: &[&str],
        stdin: Option<String>,
        timeout: Duration,
    ) -> Result<CommandResult, String> {
        let tool = label(argv);
        let mut reasons = Vec::new();
        if !self.allow_host && place == Place::Host {
            return Err(format!(
                "{tool}: host execution is off (run_on_host: false)"
            ));
        }
        for &on_host in place.attempts() {
            if on_host && !self.allow_host {
                continue;
            }
            if self.cancelled() {
                return Err(format!("{tool}: cancelled"));
            }
            let spec = CommandSpec {
                command: shell_join(argv),
                stdin: stdin.clone(),
                timeout,
                run_on_host: on_host,
                ..Default::default()
            };
            let live = LiveOutput::new(TOOL_OUTPUT_LIMIT);
            let ctx = self.ctx.clone();
            match run_command(spec, live, async move { ctx.wait_cancelled().await }).await {
                Err(e) => {
                    reasons.push(format!("{tool}: can't run on {}: {e}", place_name(on_host)))
                }
                // sh's "command not found".
                Ok(r) if r.exit_code == Some(127) => {
                    reasons.push(format!("{tool}: not found on {}", place_name(on_host)))
                }
                Ok(r) => return Ok(r),
            }
        }
        Err(reasons.join("; "))
    }

    /// [`run`](Self::run), requiring exit 0; returns stdout.
    pub async fn stdout(
        &self,
        place: Place,
        argv: &[&str],
        timeout: Duration,
    ) -> Result<String, String> {
        let r = self.run(place, argv, None, timeout).await?;
        check(&label(argv), &r)?;
        Ok(r.stdout)
    }

    /// The route to the internet, per `ip -j route get 1.1.1.1`: what the
    /// kernel actually uses, policy routing included, for when the main table
    /// has no default route.
    pub async fn internet_route(&self, place: Place) -> Option<Route> {
        let out = self
            .stdout(
                place,
                &["ip", "-j", "route", "get", INTERNET_PING],
                TOOL_TIMEOUT,
            )
            .await
            .ok()?;
        parse::ip_route(&out).ok()?.into_iter().next()
    }
}

/// A short name for a tool invocation in `errors`: the program and its
/// subcommand words, e.g. "nmcli con show" or "ip addr".
fn label(argv: &[&str]) -> String {
    let mut words = vec![argv[0]];
    words.extend(
        argv[1..]
            .iter()
            .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase()))
            .take(2),
    );
    words.join(" ")
}

/// `Err` describing a run that didn't exit 0.
fn check(tool: &str, r: &CommandResult) -> Result<(), String> {
    if r.timed_out {
        return Err(format!("{tool}: timed out after {:.0}s", r.duration));
    }
    if r.cancelled {
        return Err(format!("{tool}: cancelled"));
    }
    if r.exit_code == Some(0) {
        return Ok(());
    }
    let detail = r
        .stderr
        .lines()
        .chain(r.stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let code = r.exit_code.map_or("?".into(), |c| c.to_string());
    Err(format!("{tool}: exit {code}: {detail}")
        .trim_end_matches(": ")
        .to_string())
}

// -- net_status --------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Interface {
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    state: Option<String>,
    ip4: Vec<String>,
    ip6: Vec<String>,
    gateway: Option<String>,
    dns: Vec<String>,
    mac: Option<String>,
    connection: Option<String>,
    wifi: Option<parse::WifiLink>,
}

/// Network state of the host, and whether it can reach the platform. Never
/// fails: whatever can't be read is an entry in `errors`.
pub async fn net_status(runner: &Runner, step: &Step) -> Value {
    let mut errors: Vec<String> = Vec::new();
    let host = Place::HostThenContainer;

    step.set("Reading interfaces");
    let devices = note(
        &mut errors,
        runner
            .stdout(
                host,
                &["nmcli", "-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev"],
                TOOL_TIMEOUT,
            )
            .await,
    )
    .map(|o| parse::nmcli_devices(&o))
    .unwrap_or_default();
    let connections = note(
        &mut errors,
        runner
            .stdout(
                host,
                &[
                    "nmcli",
                    "-t",
                    "-f",
                    "NAME,UUID,TYPE,DEVICE",
                    "con",
                    "show",
                    "--active",
                ],
                TOOL_TIMEOUT,
            )
            .await,
    )
    .map(|o| parse::nmcli_connections(&o))
    .unwrap_or_default();
    let links: Vec<IpLink> = note(
        &mut errors,
        runner
            .stdout(host, &["ip", "-j", "addr"], TOOL_TIMEOUT)
            .await,
    )
    .and_then(|o| note(&mut errors, parse::ip_addr(&o)))
    .unwrap_or_default();
    let routes: Vec<Route> = note(
        &mut errors,
        runner
            .stdout(host, &["ip", "-j", "route"], TOOL_TIMEOUT)
            .await,
    )
    .and_then(|o| note(&mut errors, parse::ip_route(&o)))
    .unwrap_or_default();

    step.set("Reading DNS");
    let resolved = match runner
        .stdout(Place::Host, &["resolvectl", "status"], TOOL_TIMEOUT)
        .await
    {
        Ok(out) => Some(parse::resolvectl_status(&out)),
        // No systemd-resolved (or not running): resolv.conf is the truth.
        Err(_) => None,
    };
    let dns = match &resolved {
        Some(r) if !r.all().is_empty() => r.all(),
        _ => note(
            &mut errors,
            runner
                .stdout(host, &["cat", "/etc/resolv.conf"], TOOL_TIMEOUT)
                .await,
        )
        .map(|o| parse::resolv_conf(&o))
        .unwrap_or_default(),
    };

    let wifi = if devices.iter().any(|d| d.kind == "wifi") {
        step.set("Reading wifi");
        note(
            &mut errors,
            runner
                .stdout(
                    host,
                    &[
                        "nmcli",
                        "-t",
                        "-f",
                        "ACTIVE,SSID,SIGNAL,DEVICE",
                        "dev",
                        "wifi",
                        "list",
                        "--rescan",
                        "no",
                    ],
                    TOOL_TIMEOUT,
                )
                .await,
        )
        .map(|o| parse::nmcli_wifi_active(&o))
        .unwrap_or_default()
    } else {
        Default::default()
    };

    step.set("Reading modem");
    let modem = modem(runner, &mut errors).await;

    // Interfaces: NetworkManager's view, then anything only `ip` knows of.
    // Loopback is left out.
    let gateway_for = |name: &str| {
        routes
            .iter()
            .filter(|r| r.dst == "default" && r.dev.as_deref() == Some(name))
            .min_by_key(|r| r.metric.unwrap_or(0))
            .and_then(|r| r.gateway.clone())
    };
    let mut interfaces: Vec<Interface> = Vec::new();
    let mut names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
    for link in &links {
        if !names.contains(&link.name.as_str()) {
            names.push(&link.name);
        }
    }
    for name in names {
        let device = devices.iter().find(|d| d.name == name);
        let link = links.iter().find(|l| l.name == name);
        let kind = device
            .map(|d| d.kind.clone())
            .or_else(|| link.map(|l| l.link_type.clone()));
        if matches!(kind.as_deref(), Some("loopback")) {
            continue;
        }
        interfaces.push(Interface {
            name: name.to_string(),
            kind,
            state: device
                .map(|d| d.state.clone())
                .or_else(|| link.map(|l| l.state.to_lowercase())),
            ip4: link.map(IpLink::ip4_cidrs).unwrap_or_default(),
            ip6: link.map(|l| l.ip6.clone()).unwrap_or_default(),
            gateway: gateway_for(name),
            dns: resolved
                .as_ref()
                .map(|r| r.for_link(name))
                .unwrap_or_default(),
            mac: link.and_then(|l| l.mac.clone()),
            connection: device.and_then(|d| d.connection.clone()),
            wifi: wifi.get(name).cloned(),
        });
    }

    step.set("Checking connectivity");
    let gateway = match parse::default_route(&routes).and_then(|r| r.gateway.clone()) {
        Some(gw) => Some(gw),
        None => runner.internet_route(host).await.and_then(|r| r.gateway),
    };
    let ping = |target: String| async move {
        runner
            .stdout(
                host,
                &["ping", "-c", "1", "-W", "2", &target],
                Duration::from_secs(5),
            )
            .await
    };
    let (gw_ping, net_ping, resolve, https) = tokio::join!(
        async {
            match &gateway {
                Some(gw) => Some(ping(gw.clone()).await),
                None => None,
            }
        },
        ping(INTERNET_PING.to_string()),
        runner.stdout(
            host,
            &["getent", "hosts", PLATFORM_HOST],
            Duration::from_secs(8)
        ),
        runner.run(
            host,
            &[
                "curl",
                "-sS",
                "-o",
                "/dev/null",
                "-m",
                "5",
                "-w",
                "%{http_code}",
                PLATFORM_URL
            ],
            None,
            Duration::from_secs(8),
        ),
    );
    let gateway_ping = match gw_ping {
        Some(r) => passed(&mut errors, r, "gateway ping"),
        None => {
            errors.push("gateway ping: no default gateway".into());
            false
        }
    };
    let internet_ping = passed(&mut errors, net_ping, "internet ping");
    let dns_resolve = passed(&mut errors, resolve, "dns resolve");
    let (platform_https, platform_status) = match https {
        Ok(r) => {
            let status = r.stdout.trim().parse::<i64>().ok().filter(|s| *s > 0);
            match check("curl", &r) {
                Ok(()) => (true, status),
                Err(e) => {
                    errors.push(format!("platform https: {e}"));
                    (false, status)
                }
            }
        }
        Err(e) => {
            errors.push(format!("platform https: {e}"));
            (false, None)
        }
    };

    json!({
        "interfaces": interfaces,
        "routes": routes,
        "dns": dns,
        "connections": connections,
        "modem": modem,
        "checks": {
            "gateway_ping": gateway_ping,
            "internet_ping": internet_ping,
            "dns_resolve": dns_resolve,
            "platform_https": platform_https,
            "platform_status": platform_status,
        },
        "errors": errors,
    })
}

fn note<T>(errors: &mut Vec<String>, r: Result<T, String>) -> Option<T> {
    r.map_err(|e| errors.push(e)).ok()
}

/// Whether a check passed, noting why when it didn't.
fn passed(errors: &mut Vec<String>, r: Result<String, String>, what: &str) -> bool {
    match r {
        Ok(_) => true,
        Err(e) => {
            errors.push(format!("{what}: {e}"));
            false
        }
    }
}

/// The first modem ModemManager knows of; `None` when there's none (or no
/// ModemManager, which goes in `errors`).
async fn modem(runner: &Runner, errors: &mut Vec<String>) -> Option<parse::Modem> {
    let host = Place::HostThenContainer;
    let list = runner
        .stdout(host, &["mmcli", "-J", "-L"], TOOL_TIMEOUT)
        .await
        .and_then(|o| parse::mmcli_modem_list(&o))
        .map_err(|e| errors.push(e))
        .ok()?;
    let index = list.first()?;
    let (mut modem, bearer) = runner
        .stdout(host, &["mmcli", "-J", "-m", index], TOOL_TIMEOUT)
        .await
        .and_then(|o| parse::mmcli_modem(&o))
        .map_err(|e| errors.push(e))
        .ok()?;
    if modem.apn.is_none() {
        if let Some(bearer) = bearer {
            modem.apn = runner
                .stdout(host, &["mmcli", "-J", "-b", &bearer], TOOL_TIMEOUT)
                .await
                .ok()
                .and_then(|o| parse::mmcli_bearer_apn(&o));
        }
    }
    Some(modem)
}

// -- net_wifi_scan -----------------------------------------------------------

/// Visible wifi networks, strongest first, one per SSID.
pub async fn net_wifi_scan(runner: &Runner, step: &Step) -> Result<Value, RpcError> {
    step.set("Scanning wifi");
    let out = runner
        .stdout(
            Place::HostThenContainer,
            &[
                "nmcli",
                "-t",
                "-f",
                "SSID,SIGNAL,SECURITY,FREQ",
                "dev",
                "wifi",
                "list",
                "--rescan",
                "yes",
            ],
            WIFI_SCAN_TIMEOUT,
        )
        .await
        .map_err(|e| RpcError::new("WIFI_SCAN_FAILED", e))?;
    Ok(json!({"networks": parse::nmcli_wifi_list(&out)}))
}

// -- scan_network ------------------------------------------------------------

/// Validated `scan_network` parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanParams {
    pub subnet: Option<Subnet>,
    pub interface: Option<String>,
    /// Normalised `nmap -p` list.
    pub ports: Option<String>,
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new("INVALID_PARAMS", message)
}

impl ScanParams {
    pub fn parse(payload: &Value) -> Result<Self, RpcError> {
        let empty = Map::new();
        let payload = match payload {
            Value::Object(p) => p,
            Value::Null => &empty,
            _ => return Err(invalid("payload must be an object")),
        };
        let string = |key: &str| match payload.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
            Some(_) => Err(invalid(format!("'{key}' must be a string"))),
        };
        let subnet = string("subnet")?
            .map(|s| parse::parse_subnet(&s))
            .transpose()
            .map_err(invalid)?;
        let interface = string("interface")?;
        if let Some(i) = &interface {
            if !parse::valid_interface(i) {
                return Err(invalid(format!(
                    "'interface' {i:?} is not an interface name"
                )));
            }
        }
        let ports = match payload.get("ports") {
            None | Some(Value::Null) => None,
            Some(p) => Some(parse::parse_ports(p).map_err(invalid)?),
        };
        Ok(Self {
            subnet,
            interface,
            ports,
        })
    }
}

#[derive(Debug, Serialize)]
struct Host {
    ip: Ipv4Addr,
    mac: Option<String>,
    vendor: Option<String>,
    hostname: Option<String>,
    open_ports: Vec<u16>,
}

/// Pick the interface and subnet to scan: the ones asked for, else the
/// default route's interface and its network.
async fn scan_target(
    runner: &Runner,
    params: &ScanParams,
) -> Result<(Subnet, Option<String>), RpcError> {
    let place = Place::HostThenContainer;
    let no_target = |e: String| RpcError::new("NO_SUBNET", e);
    let links = runner
        .stdout(place, &["ip", "-j", "addr"], TOOL_TIMEOUT)
        .await
        .and_then(|o| parse::ip_addr(&o));

    if let Some(subnet) = params.subnet {
        // arp-scan needs an interface; use the one on that network if any.
        let interface = params.interface.clone().or_else(|| {
            links.ok()?.into_iter().find_map(|l| {
                l.ip4
                    .iter()
                    .any(|(a, p)| Subnet::new(*a, *p).contains(subnet.network))
                    .then_some(l.name)
            })
        });
        return Ok((subnet, interface));
    }

    let links = links.map_err(no_target)?;
    let interface = match &params.interface {
        Some(i) => i.clone(),
        None => {
            let routes = runner
                .stdout(place, &["ip", "-j", "route"], TOOL_TIMEOUT)
                .await
                .and_then(|o| parse::ip_route(&o))
                .map_err(no_target)?;
            match parse::default_route(&routes).and_then(|r| r.dev.clone()) {
                Some(dev) => dev,
                None => runner
                    .internet_route(place)
                    .await
                    .and_then(|r| r.dev)
                    .ok_or_else(|| {
                        no_target("no default route; pass `subnet` or `interface`".into())
                    })?,
            }
        }
    };
    let link = links
        .iter()
        .find(|l| l.name == interface)
        .ok_or_else(|| no_target(format!("no interface {interface:?}")))?;
    let (addr, prefix) = *link
        .ip4
        .first()
        .ok_or_else(|| no_target(format!("{interface} has no IPv4 address")))?;
    let subnet = parse::default_subnet(addr, prefix).map_err(no_target)?;
    Ok((subnet, Some(interface)))
}

/// Hosts on the local network (ARP + ping sweep), optionally with open TCP
/// ports. Runs in the container, with its nmap and arp-scan.
pub async fn scan_network(
    runner: &Runner,
    step: &Step,
    params: ScanParams,
) -> Result<Value, RpcError> {
    let started = Instant::now();
    let remaining = || {
        SCAN_BUDGET
            .saturating_sub(started.elapsed())
            .max(Duration::from_secs(1))
    };
    let mut errors: Vec<String> = Vec::new();

    step.set("Finding the local network");
    let (subnet, interface) = scan_target(runner, &params).await?;
    let target = subnet.to_string();
    step.set(format!("Scanning {target}"));

    let mut arp_argv = vec!["arp-scan", "--retry=2"];
    if let Some(i) = &interface {
        arp_argv.extend(["-I", i]);
    }
    arp_argv.push(&target);
    let mut nmap_argv = vec!["nmap", "-sn", "-T4", "-oG", "-"];
    if let Some(i) = &interface {
        nmap_argv.extend(["-e", i]);
    }
    nmap_argv.push(&target);
    let discovery = remaining().min(Duration::from_secs(40));
    let (arp, nmap) = tokio::join!(
        runner.run(Place::Container, &arp_argv, None, discovery),
        runner.run(Place::Container, &nmap_argv, None, discovery),
    );
    let arp = output(arp, "arp-scan", &mut errors);
    let nmap = output(nmap, "nmap", &mut errors);
    if arp.is_none() && nmap.is_none() {
        return Err(RpcError::new("SCAN_FAILED", errors.join("; ")));
    }

    let mut hosts: BTreeMap<u32, Host> = BTreeMap::new();
    for a in parse::arp_scan(arp.as_deref().unwrap_or_default()) {
        let host = hosts
            .entry(u32::from(a.ip))
            .or_insert_with(|| empty_host(a.ip));
        host.mac = Some(a.mac);
        host.vendor = a.vendor;
    }
    for n in parse::nmap_grepable(nmap.as_deref().unwrap_or_default()) {
        let host = hosts
            .entry(u32::from(n.ip))
            .or_insert_with(|| empty_host(n.ip));
        host.hostname = n.hostname;
    }

    if let Some(ports) = &params.ports {
        if !hosts.is_empty() && !runner.cancelled() {
            step.set(format!("Scanning ports on {} hosts", hosts.len()));
            let list: String = hosts.values().map(|h| format!("{}\n", h.ip)).collect();
            let mut argv = vec![
                "nmap", "-Pn", "-T4", "-p", ports, "--open", "-oG", "-", "-iL", "-",
            ];
            if let Some(i) = &interface {
                argv.extend(["-e", i]);
            }
            let r = runner
                .run(Place::Container, &argv, Some(list), remaining())
                .await;
            if let Some(out) = output(r, "nmap", &mut errors) {
                for n in parse::nmap_grepable(&out) {
                    if let Some(host) = hosts.get_mut(&u32::from(n.ip)) {
                        host.open_ports = n.open_ports;
                        if host.hostname.is_none() {
                            host.hostname = n.hostname;
                        }
                    }
                }
            }
        }
    }

    Ok(json!({
        "subnet": target,
        "interface": interface,
        "hosts": hosts.into_values().collect::<Vec<_>>(),
        "errors": errors,
    }))
}

/// A tool's stdout, noting any failure. Partial output from a timed-out run
/// is still worth parsing.
fn output(
    r: Result<CommandResult, String>,
    tool: &str,
    errors: &mut Vec<String>,
) -> Option<String> {
    match r {
        Ok(r) => {
            if let Err(e) = check(tool, &r) {
                errors.push(e);
            }
            Some(r.stdout)
        }
        Err(e) => {
            errors.push(e);
            None
        }
    }
}

fn empty_host(ip: Ipv4Addr) -> Host {
    Host {
        ip,
        mac: None,
        vendor: None,
        hostname: None,
        open_ports: Vec::new(),
    }
}
