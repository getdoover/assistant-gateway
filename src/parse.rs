//! Parsers for the diagnostic tools' output (nmcli terse mode, `ip -j`,
//! resolvectl, mmcli, arp-scan, nmap grepable) and validators for the typed
//! methods' parameters. Pure functions; nothing here runs anything.

use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;

use serde::Serialize;
use serde_json::Value;

// -- nmcli -------------------------------------------------------------------

/// Split one line of `nmcli -t` output. Terse mode separates fields with `:`
/// and escapes a literal `:` or `\` in a value with a backslash.
pub fn split_terse(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    fields.last_mut().unwrap().push(next);
                }
            }
            ':' => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

fn terse_rows(out: &str, min_fields: usize) -> impl Iterator<Item = Vec<String>> + '_ {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(split_terse)
        .filter(move |f| f.len() >= min_fields)
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s != "--").then(|| s.to_string())
}

/// A row of `nmcli -t -f DEVICE,TYPE,STATE,CONNECTION dev`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NmDevice {
    pub name: String,
    pub kind: String,
    pub state: String,
    pub connection: Option<String>,
}

pub fn nmcli_devices(out: &str) -> Vec<NmDevice> {
    terse_rows(out, 4)
        .map(|f| NmDevice {
            name: f[0].clone(),
            kind: f[1].clone(),
            state: f[2].clone(),
            connection: non_empty(&f[3]),
        })
        .collect()
}

/// A row of `nmcli -t -f NAME,UUID,TYPE,DEVICE con show --active`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NmConnection {
    pub name: String,
    pub uuid: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub device: Option<String>,
}

pub fn nmcli_connections(out: &str) -> Vec<NmConnection> {
    terse_rows(out, 4)
        .map(|f| NmConnection {
            name: f[0].clone(),
            uuid: f[1].clone(),
            kind: f[2].clone(),
            device: non_empty(&f[3]),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WifiLink {
    pub ssid: Option<String>,
    pub signal: Option<i64>,
}

/// The associated network per wifi device, from
/// `nmcli -t -f ACTIVE,SSID,SIGNAL,DEVICE dev wifi`.
pub fn nmcli_wifi_active(out: &str) -> HashMap<String, WifiLink> {
    terse_rows(out, 4)
        .filter(|f| f[0] == "yes")
        .map(|f| {
            (
                f[3].clone(),
                WifiLink {
                    ssid: non_empty(&f[1]),
                    signal: f[2].trim().parse().ok(),
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal: Option<i64>,
    /// nmcli's SECURITY, e.g. "WPA2" or "WPA1 WPA2"; "open" when it has none.
    pub security: String,
    /// MHz.
    pub freq: Option<i64>,
}

/// `nmcli -t -f SSID,SIGNAL,SECURITY,FREQ dev wifi list`: hidden (blank)
/// SSIDs dropped, one entry per SSID (its strongest access point), strongest
/// first.
pub fn nmcli_wifi_list(out: &str) -> Vec<WifiNetwork> {
    let mut best: Vec<WifiNetwork> = Vec::new();
    for f in terse_rows(out, 4) {
        let ssid = f[0].trim();
        if ssid.is_empty() || ssid == "--" {
            continue;
        }
        let network = WifiNetwork {
            ssid: ssid.to_string(),
            signal: f[1].trim().parse().ok(),
            security: non_empty(&f[2]).unwrap_or_else(|| "open".into()),
            freq: f[3].split_whitespace().next().and_then(|n| n.parse().ok()),
        };
        match best.iter_mut().find(|n| n.ssid == network.ssid) {
            Some(existing) if existing.signal < network.signal => *existing = network,
            Some(_) => {}
            None => best.push(network),
        }
    }
    best.sort_by(|a, b| b.signal.cmp(&a.signal));
    best
}

// -- ip -j -------------------------------------------------------------------

/// One interface from `ip -j addr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpLink {
    pub name: String,
    /// `link_type`: "ether", "loopback", "none" (tun/ppp), ...
    pub link_type: String,
    /// `operstate`: "UP", "DOWN", "UNKNOWN", ...
    pub state: String,
    pub mac: Option<String>,
    /// IPv4 addresses with their prefix length.
    pub ip4: Vec<(Ipv4Addr, u8)>,
    /// IPv6 addresses as `addr/prefix`.
    pub ip6: Vec<String>,
}

impl IpLink {
    pub fn ip4_cidrs(&self) -> Vec<String> {
        self.ip4.iter().map(|(a, p)| format!("{a}/{p}")).collect()
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).and_then(non_empty)
}

pub fn ip_addr(json: &str) -> Result<Vec<IpLink>, String> {
    let links: Vec<Value> =
        serde_json::from_str(json).map_err(|e| format!("ip -j addr: bad JSON: {e}"))?;
    Ok(links
        .iter()
        .filter_map(|link| {
            let name = str_field(link, "ifname")?;
            let mut ip4 = Vec::new();
            let mut ip6 = Vec::new();
            for info in link
                .get("addr_info")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let (Some(local), Some(prefix)) = (
                    info.get("local").and_then(Value::as_str),
                    info.get("prefixlen").and_then(Value::as_u64),
                ) else {
                    continue;
                };
                match info.get("family").and_then(Value::as_str) {
                    Some("inet") => {
                        if let Ok(addr) = local.parse() {
                            ip4.push((addr, prefix.min(32) as u8));
                        }
                    }
                    Some("inet6") => ip6.push(format!("{local}/{prefix}")),
                    _ => {}
                }
            }
            let link_type = str_field(link, "link_type").unwrap_or_default();
            let mac = str_field(link, "address")
                .filter(|m| link_type == "ether" && m != "00:00:00:00:00:00");
            Some(IpLink {
                name,
                link_type,
                state: str_field(link, "operstate").unwrap_or_default(),
                mac,
                ip4,
                ip6,
            })
        })
        .collect())
}

/// One route from `ip -j route`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    /// "default" or a CIDR.
    pub dst: String,
    pub gateway: Option<String>,
    pub dev: Option<String>,
    pub protocol: Option<String>,
    pub metric: Option<i64>,
    pub prefsrc: Option<String>,
}

pub fn ip_route(json: &str) -> Result<Vec<Route>, String> {
    let routes: Vec<Value> =
        serde_json::from_str(json).map_err(|e| format!("ip -j route: bad JSON: {e}"))?;
    Ok(routes
        .iter()
        .filter_map(|r| {
            Some(Route {
                dst: str_field(r, "dst")?,
                gateway: str_field(r, "gateway"),
                dev: str_field(r, "dev"),
                protocol: str_field(r, "protocol"),
                metric: r.get("metric").and_then(Value::as_i64),
                prefsrc: str_field(r, "prefsrc"),
            })
        })
        .collect())
}

/// The default route the kernel prefers: lowest metric (a missing metric is
/// 0).
pub fn default_route(routes: &[Route]) -> Option<&Route> {
    routes
        .iter()
        .filter(|r| r.dst == "default")
        .min_by_key(|r| r.metric.unwrap_or(0))
}

// -- DNS ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedDns {
    pub global: Vec<String>,
    /// Per link name, in the order resolvectl lists them.
    pub links: Vec<(String, Vec<String>)>,
}

impl ResolvedDns {
    /// Every server, global first, without duplicates.
    pub fn all(&self) -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        for s in self
            .global
            .iter()
            .chain(self.links.iter().flat_map(|(_, s)| s))
        {
            if !all.contains(s) {
                all.push(s.clone());
            }
        }
        all
    }

    pub fn for_link(&self, name: &str) -> Vec<String> {
        self.links
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
    }
}

/// `resolvectl status`: the "DNS Servers" of the Global section and of each
/// `Link N (name)` section. Values may wrap onto indented continuation lines,
/// and DoT servers carry a `#name` suffix, which is dropped.
pub fn resolvectl_status(out: &str) -> ResolvedDns {
    let mut dns = ResolvedDns::default();
    // None = the Global section.
    let mut section: Option<usize> = None;
    let mut in_servers = false;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_servers = false;
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            in_servers = false;
            if trimmed == "Global" {
                section = None;
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("Link ") {
                let name = rest
                    .split_once('(')
                    .and_then(|(_, n)| n.strip_suffix(')'))
                    .unwrap_or(rest)
                    .to_string();
                dns.links.push((name, Vec::new()));
                section = Some(dns.links.len() - 1);
                continue;
            }
        }
        // "Key: value"; IPv6 addresses have colons but never ": ".
        let values = match trimmed.split_once(": ") {
            Some((key, value)) => {
                in_servers = key == "DNS Servers";
                if !in_servers {
                    continue;
                }
                value
            }
            None if trimmed.ends_with(':') => {
                in_servers = trimmed == "DNS Servers:";
                continue;
            }
            None if in_servers => trimmed,
            None => continue,
        };
        let target = match section {
            None => &mut dns.global,
            Some(i) => &mut dns.links[i].1,
        };
        for server in values.split_whitespace() {
            let server = server.split('#').next().unwrap_or(server).to_string();
            if !server.is_empty() && !target.contains(&server) {
                target.push(server);
            }
        }
    }
    dns
}

/// The `nameserver` lines of `/etc/resolv.conf`.
pub fn resolv_conf(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            (words.next() == Some("nameserver"))
                .then(|| words.next())
                .flatten()
                .map(str::to_string)
        })
        .collect()
}

// -- mmcli -------------------------------------------------------------------

fn path_index(path: &str) -> Option<String> {
    let idx = path.rsplit('/').next()?;
    (!idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit())).then(|| idx.to_string())
}

/// Modem indices from `mmcli -J -L`.
pub fn mmcli_modem_list(json: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("mmcli -L: bad JSON: {e}"))?;
    Ok(v.get("modem-list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(path_index)
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Modem {
    pub state: Option<String>,
    /// Signal quality, percent.
    pub signal: Option<i64>,
    pub operator: Option<String>,
    pub apn: Option<String>,
}

fn pointer_str(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(non_empty)
}

/// `mmcli -J -m <idx>`, plus the index of its first bearer, whose APN
/// (`mmcli -J -b <idx>`) is the one in use when the initial EPS bearer
/// doesn't say.
pub fn mmcli_modem(json: &str) -> Result<(Modem, Option<String>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("mmcli -m: bad JSON: {e}"))?;
    let signal = v
        .pointer("/modem/generic/signal-quality/value")
        .and_then(|s| match s {
            Value::String(s) => s.trim().parse().ok(),
            other => other.as_i64(),
        });
    let bearer = v
        .pointer("/modem/generic/bearers")
        .and_then(Value::as_array)
        .and_then(|b| b.iter().filter_map(Value::as_str).find_map(path_index));
    Ok((
        Modem {
            state: pointer_str(&v, "/modem/generic/state"),
            signal,
            operator: pointer_str(&v, "/modem/3gpp/operator-name"),
            apn: pointer_str(&v, "/modem/3gpp/eps/initial-bearer/settings/apn"),
        },
        bearer,
    ))
}

/// The APN from `mmcli -J -b <idx>`.
pub fn mmcli_bearer_apn(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    pointer_str(&v, "/bearer/properties/apn")
}

// -- arp-scan / nmap ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArpHost {
    pub ip: Ipv4Addr,
    pub mac: String,
    pub vendor: Option<String>,
}

fn is_mac(s: &str) -> bool {
    let parts: Vec<_> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// arp-scan's `ip<TAB>mac<TAB>vendor` lines; header, footer and duplicate
/// replies are skipped.
pub fn arp_scan(out: &str) -> Vec<ArpHost> {
    let mut hosts: Vec<ArpHost> = Vec::new();
    for line in out.lines() {
        let mut fields = line.split('\t');
        let (Some(ip), Some(mac)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(ip), true) = (ip.trim().parse::<Ipv4Addr>(), is_mac(mac.trim())) else {
            continue;
        };
        if hosts.iter().any(|h| h.ip == ip) {
            continue;
        }
        let vendor = fields
            .next()
            .map(|v| {
                // Duplicate replies are tagged "(DUP: n)".
                match v.find(" (DUP:") {
                    Some(i) => &v[..i],
                    None => v,
                }
                .trim()
            })
            .filter(|v| !v.is_empty() && !v.starts_with("(Unknown"))
            .map(str::to_string);
        hosts.push(ArpHost {
            ip,
            mac: mac.trim().to_ascii_lowercase(),
            vendor,
        });
    }
    hosts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NmapHost {
    pub ip: Ipv4Addr,
    pub hostname: Option<String>,
    pub open_ports: Vec<u16>,
}

/// `nmap -oG -`: hosts reported up (by a `Status: Up` line or by having
/// ports), with their reverse-DNS name and open TCP ports.
pub fn nmap_grepable(out: &str) -> Vec<NmapHost> {
    let mut hosts: Vec<NmapHost> = Vec::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("Host: ") else {
            continue;
        };
        let mut sections = rest.split('\t');
        let head = sections.next().unwrap_or_default();
        let (ip, name) = match head.split_once(' ') {
            Some((ip, name)) => (
                ip,
                name.trim().trim_start_matches('(').trim_end_matches(')'),
            ),
            None => (head, ""),
        };
        let Ok(ip) = ip.parse::<Ipv4Addr>() else {
            continue;
        };
        let mut up = false;
        let mut ports = Vec::new();
        for section in sections {
            if let Some(status) = section.strip_prefix("Status: ") {
                up = status.trim() == "Up";
            } else if let Some(list) = section.strip_prefix("Ports: ") {
                up = true;
                for entry in list.split(", ") {
                    // port/state/protocol/owner/service/rpc/version/
                    let mut f = entry.trim().split('/');
                    let (Some(port), Some("open")) = (f.next(), f.next()) else {
                        continue;
                    };
                    if let Ok(port) = port.parse() {
                        ports.push(port);
                    }
                }
            }
        }
        if !up {
            continue;
        }
        let host = match hosts.iter_mut().find(|h| h.ip == ip) {
            Some(h) => h,
            None => {
                hosts.push(NmapHost {
                    ip,
                    hostname: None,
                    open_ports: Vec::new(),
                });
                hosts.last_mut().unwrap()
            }
        };
        if host.hostname.is_none() {
            host.hostname = non_empty(name);
        }
        for port in ports {
            if !host.open_ports.contains(&port) {
                host.open_ports.push(port);
            }
        }
        host.open_ports.sort_unstable();
    }
    hosts
}

// -- parameters --------------------------------------------------------------

/// An IPv4 network, normalised to its network address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

/// The largest network `scan_network` will sweep: a /22, 1024 addresses.
pub const MAX_SCAN_PREFIX: u8 = 22;
/// Below this, the device's own network is too big to guess a scan range from.
pub const MIN_GUESS_PREFIX: u8 = 16;

impl Subnet {
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Self {
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix as u32)
        };
        Self {
            network: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        }
    }

    pub fn contains(&self, addr: Ipv4Addr) -> bool {
        Subnet::new(addr, self.prefix).network == self.network
    }

    pub fn size(&self) -> u64 {
        1u64 << (32 - self.prefix as u32)
    }
}

impl fmt::Display for Subnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// A requested scan range: `a.b.c.d/p` (or a bare address, /32), no larger
/// than a /22.
pub fn parse_subnet(s: &str) -> Result<Subnet, String> {
    let s = s.trim();
    let (addr, prefix) = match s.split_once('/') {
        Some((a, p)) => (
            a,
            p.parse::<u8>()
                .map_err(|_| format!("bad prefix in {s:?}"))?,
        ),
        None => (s, 32),
    };
    let addr: Ipv4Addr = addr
        .parse()
        .map_err(|_| format!("{s:?} is not an IPv4 address or CIDR"))?;
    if prefix > 32 {
        return Err(format!("bad prefix in {s:?}"));
    }
    if prefix < MAX_SCAN_PREFIX {
        return Err(format!(
            "{s} is larger than a /{MAX_SCAN_PREFIX} ({} addresses max)",
            1u64 << (32 - MAX_SCAN_PREFIX as u32)
        ));
    }
    Ok(Subnet::new(addr, prefix))
}

/// The scan range for the device's own address: its network, narrowed to the
/// /22 around it when the network is bigger than that. Refused below a /16.
pub fn default_subnet(addr: Ipv4Addr, prefix: u8) -> Result<Subnet, String> {
    if prefix < MIN_GUESS_PREFIX {
        return Err(format!(
            "{addr}/{prefix} is too large to scan; pass a `subnet` (/{MAX_SCAN_PREFIX} or smaller)"
        ));
    }
    Ok(Subnet::new(addr, prefix.max(MAX_SCAN_PREFIX)))
}

/// A port list for `nmap -p`: comma-separated ports and `lo-hi` ranges, as a
/// string or a JSON array of ports / range strings. Every port is 1-65535.
/// Returns the normalised list.
pub fn parse_ports(v: &Value) -> Result<String, String> {
    let items: Vec<String> = match v {
        Value::String(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
        Value::Number(n) => vec![n.to_string()],
        Value::Array(a) => a
            .iter()
            .map(|p| match p {
                Value::Number(n) => Ok(n.to_string()),
                Value::String(s) => Ok(s.trim().to_string()),
                _ => Err("'ports' entries must be numbers or strings".to_string()),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("'ports' must be a string like \"22,80,500-510\" or an array".into()),
    };
    if items.is_empty() || items.iter().all(|i| i.is_empty()) {
        return Err("'ports' is empty".into());
    }
    let port = |s: &str| -> Result<u16, String> {
        match s.trim().parse::<u32>() {
            Ok(p @ 1..=65535) => Ok(p as u16),
            _ => Err(format!("bad port {s:?}: ports are 1-65535")),
        }
    };
    let mut out = Vec::new();
    for item in &items {
        match item.split_once('-') {
            Some((lo, hi)) => {
                let (lo, hi) = (port(lo)?, port(hi)?);
                if lo > hi {
                    return Err(format!("bad port range {item:?}"));
                }
                out.push(format!("{lo}-{hi}"));
            }
            None => out.push(port(item)?.to_string()),
        }
    }
    Ok(out.join(","))
}

/// Linux interface names: 1-15 of `[A-Za-z0-9_.:@-]`, not `.`/`..`, and never
/// starting with `-` (they go on a command line).
pub fn valid_interface(s: &str) -> bool {
    (1..=15).contains(&s.len())
        && !s.starts_with('-')
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:@-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terse_escapes() {
        assert_eq!(split_terse(r"a:b\:c:d\\e:"), ["a", "b:c", r"d\e", ""]);
        assert_eq!(split_terse("x"), ["x"]);
    }

    #[test]
    fn nmcli_dev_rows() {
        let out = "eth0:ethernet:connected:Wired connection 1\n\
                   wlan0:wifi:connected:Cafe\\: Guest\n\
                   wwan0:gsm:disconnected:\n\
                   lo:loopback:connected (externally):lo\n";
        let devs = nmcli_devices(out);
        assert_eq!(devs.len(), 4);
        assert_eq!(devs[1].connection.as_deref(), Some("Cafe: Guest"));
        assert_eq!(devs[2].connection, None);
        assert_eq!(devs[3].state, "connected (externally)");
    }

    #[test]
    fn nmcli_con_rows() {
        let out = "Wired connection 1:2b0e7c1e-7b8a-3f2c-9d1c-4a0e8f6f1a11:802-3-ethernet:eth0\n";
        assert_eq!(
            nmcli_connections(out),
            [NmConnection {
                name: "Wired connection 1".into(),
                uuid: "2b0e7c1e-7b8a-3f2c-9d1c-4a0e8f6f1a11".into(),
                kind: "802-3-ethernet".into(),
                device: Some("eth0".into()),
            }]
        );
    }

    #[test]
    fn wifi_active() {
        let out = "no:Neighbour:40:wlan0\nyes:Farm Office:72:wlan0\nno:Other:90:wlan0\n";
        let active = nmcli_wifi_active(out);
        assert_eq!(
            active["wlan0"],
            WifiLink {
                ssid: Some("Farm Office".into()),
                signal: Some(72)
            }
        );
    }

    #[test]
    fn wifi_list_dedupes_and_sorts() {
        let out = "Farm:40:WPA2:2437 MHz\n\
                   :80:WPA2:2412 MHz\n\
                   Farm:65:WPA2:5180 MHz\n\
                   Guest\\:Free:30::2462 MHz\n\
                   Shed:90:WPA1 WPA2:2412 MHz\n";
        assert_eq!(
            nmcli_wifi_list(out),
            [
                WifiNetwork {
                    ssid: "Shed".into(),
                    signal: Some(90),
                    security: "WPA1 WPA2".into(),
                    freq: Some(2412)
                },
                WifiNetwork {
                    ssid: "Farm".into(),
                    signal: Some(65),
                    security: "WPA2".into(),
                    freq: Some(5180)
                },
                WifiNetwork {
                    ssid: "Guest:Free".into(),
                    signal: Some(30),
                    security: "open".into(),
                    freq: Some(2462)
                },
            ]
        );
    }

    const IP_ADDR: &str = r#"[
      {"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP"],"mtu":65536,"operstate":"UNKNOWN","link_type":"loopback","address":"00:00:00:00:00:00","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host"}]},
      {"ifindex":2,"ifname":"eth0","flags":["UP"],"mtu":1500,"operstate":"UP","link_type":"ether","address":"dc:a6:32:01:02:03","addr_info":[
        {"family":"inet","local":"192.168.1.23","prefixlen":24,"broadcast":"192.168.1.255","scope":"global","dynamic":true},
        {"family":"inet6","local":"fe80::dea6:32ff:fe01:203","prefixlen":64,"scope":"link"}]},
      {"ifindex":5,"ifname":"wwan0","flags":["POINTOPOINT"],"mtu":1500,"operstate":"UNKNOWN","link_type":"none","addr_info":[{"family":"inet","local":"10.64.12.9","prefixlen":30,"scope":"global"}]}
    ]"#;

    #[test]
    fn ip_addr_links() {
        let links = ip_addr(IP_ADDR).unwrap();
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].mac, None);
        let eth = &links[1];
        assert_eq!(eth.name, "eth0");
        assert_eq!(eth.state, "UP");
        assert_eq!(eth.mac.as_deref(), Some("dc:a6:32:01:02:03"));
        assert_eq!(eth.ip4_cidrs(), ["192.168.1.23/24"]);
        assert_eq!(eth.ip6, ["fe80::dea6:32ff:fe01:203/64"]);
        assert_eq!(links[2].mac, None);
        assert!(ip_addr("not json").is_err());
    }

    #[test]
    fn ip_route_default() {
        let out = r#"[
          {"dst":"default","gateway":"10.64.12.10","dev":"wwan0","protocol":"static","metric":700,"flags":[]},
          {"dst":"default","gateway":"192.168.1.1","dev":"eth0","protocol":"dhcp","prefsrc":"192.168.1.23","metric":100,"flags":[]},
          {"dst":"192.168.1.0/24","dev":"eth0","protocol":"kernel","scope":"link","prefsrc":"192.168.1.23","metric":100,"flags":[]}
        ]"#;
        let routes = ip_route(out).unwrap();
        assert_eq!(routes.len(), 3);
        let default = default_route(&routes).unwrap();
        assert_eq!(default.gateway.as_deref(), Some("192.168.1.1"));
        assert_eq!(default.dev.as_deref(), Some("eth0"));
        assert_eq!(routes[2].gateway, None);
        assert_eq!(
            serde_json::to_value(&routes[2]).unwrap(),
            json!({"dst":"192.168.1.0/24","gateway":null,"dev":"eth0","protocol":"kernel","metric":100,"prefsrc":"192.168.1.23"})
        );
    }

    #[test]
    fn resolvectl() {
        let out = "Global
           Protocols: -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
    resolv.conf mode: stub
         DNS Servers: 9.9.9.9#dns.quad9.net
Fallback DNS Servers: 1.1.1.1 8.8.8.8

Link 2 (eth0)
    Current Scopes: DNS
         Protocols: +DefaultRoute -LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
Current DNS Server: 192.168.1.1
       DNS Servers: 192.168.1.1 fe80::1%eth0
                    8.8.4.4
        DNS Domain: lan

Link 3 (wlan0)
    Current Scopes: none
";
        let dns = resolvectl_status(out);
        assert_eq!(dns.global, ["9.9.9.9"]);
        assert_eq!(
            dns.for_link("eth0"),
            ["192.168.1.1", "fe80::1%eth0", "8.8.4.4"]
        );
        assert!(dns.for_link("wlan0").is_empty());
        assert_eq!(
            dns.all(),
            ["9.9.9.9", "192.168.1.1", "fe80::1%eth0", "8.8.4.4"]
        );
    }

    #[test]
    fn resolv_conf_nameservers() {
        let out = "# generated\nnameserver 127.0.0.53\noptions edns0\nnameserver  8.8.8.8 \n";
        assert_eq!(resolv_conf(out), ["127.0.0.53", "8.8.8.8"]);
    }

    #[test]
    fn mmcli() {
        assert_eq!(
            mmcli_modem_list(r#"{"modem-list":["/org/freedesktop/ModemManager1/Modem/0"]}"#)
                .unwrap(),
            ["0"]
        );
        assert!(mmcli_modem_list(r#"{"modem-list":[]}"#).unwrap().is_empty());
        let modem = r#"{"modem":{"3gpp":{"operator-name":"Telstra","eps":{"initial-bearer":{"settings":{"apn":"--"}}}},
            "generic":{"state":"connected","signal-quality":{"recent":"yes","value":"67"},
            "bearers":["/org/freedesktop/ModemManager1/Bearer/3"]}}}"#;
        let (m, bearer) = mmcli_modem(modem).unwrap();
        assert_eq!(
            m,
            Modem {
                state: Some("connected".into()),
                signal: Some(67),
                operator: Some("Telstra".into()),
                apn: None
            }
        );
        assert_eq!(bearer.as_deref(), Some("3"));
        assert_eq!(
            mmcli_bearer_apn(r#"{"bearer":{"properties":{"apn":"telstra.internet"}}}"#).as_deref(),
            Some("telstra.internet")
        );
    }

    #[test]
    fn arp_scan_lines() {
        let out = "Interface: eth0, type: EN10MB, MAC: dc:a6:32:01:02:03, IPv4: 192.168.1.23
Starting arp-scan 1.10.0 with 256 hosts (https://github.com/royhills/arp-scan)
192.168.1.1\t00:1A:2B:3C:4D:5E\tNETGEAR
192.168.1.40\t00:80:f4:11:22:33\tTelemecanique Electrique
192.168.1.40\t00:80:f4:11:22:33\tTelemecanique Electrique (DUP: 2)
192.168.1.77\t02:42:ac:11:00:02\t(Unknown: locally administered)

3 packets received by filter, 0 packets dropped by kernel
Ending arp-scan 1.10.0: 256 hosts scanned in 1.915 seconds (133.68 hosts/sec). 3 responded
";
        let hosts = arp_scan(out);
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].mac, "00:1a:2b:3c:4d:5e");
        assert_eq!(hosts[0].vendor.as_deref(), Some("NETGEAR"));
        assert_eq!(hosts[1].ip, Ipv4Addr::new(192, 168, 1, 40));
        assert_eq!(hosts[2].vendor, None);
    }

    #[test]
    fn nmap_grepable_hosts_and_ports() {
        let out = "# Nmap 7.95 scan initiated Thu Sep 25 10:00:00 2026 as: nmap -sn -oG - 192.168.1.0/24
Host: 192.168.1.1 (router.lan)\tStatus: Up
Host: 192.168.1.40 ()\tStatus: Up
Host: 192.168.1.9 ()\tStatus: Down
Host: 192.168.1.40 ()\tPorts: 80/open/tcp//http///, 502/open/tcp//mbap///, 443/closed/tcp//https///\tIgnored State: closed (997)
# Nmap done at Thu Sep 25 10:00:03 2026 -- 256 IP addresses (2 hosts up) scanned in 2.95 seconds
";
        let hosts = nmap_grepable(out);
        assert_eq!(
            hosts,
            [
                NmapHost {
                    ip: Ipv4Addr::new(192, 168, 1, 1),
                    hostname: Some("router.lan".into()),
                    open_ports: vec![]
                },
                NmapHost {
                    ip: Ipv4Addr::new(192, 168, 1, 40),
                    hostname: None,
                    open_ports: vec![80, 502]
                },
            ]
        );
    }

    #[test]
    fn subnets() {
        assert_eq!(
            parse_subnet("192.168.1.77/24").unwrap().to_string(),
            "192.168.1.0/24"
        );
        assert_eq!(
            parse_subnet("10.0.5.1/22").unwrap().to_string(),
            "10.0.4.0/22"
        );
        assert_eq!(parse_subnet("10.0.0.9").unwrap().to_string(), "10.0.0.9/32");
        for bad in [
            "10.0.0.0/21",
            "10.0.0.0/8",
            "10.0.0/24",
            "x",
            "10.0.0.0/33",
            "1.2.3.4/-1",
        ] {
            assert!(parse_subnet(bad).is_err(), "{bad}");
        }
        let s = parse_subnet("192.168.1.0/24").unwrap();
        assert!(s.contains(Ipv4Addr::new(192, 168, 1, 200)));
        assert!(!s.contains(Ipv4Addr::new(192, 168, 2, 1)));
        assert_eq!(s.size(), 256);

        let a = Ipv4Addr::new(172, 16, 9, 5);
        assert_eq!(default_subnet(a, 24).unwrap().to_string(), "172.16.9.0/24");
        assert_eq!(default_subnet(a, 16).unwrap().to_string(), "172.16.8.0/22");
        assert_eq!(default_subnet(a, 30).unwrap().to_string(), "172.16.9.4/30");
        assert!(default_subnet(a, 12).is_err());
    }

    #[test]
    fn ports() {
        assert_eq!(
            parse_ports(&json!("22, 80,500-510")).unwrap(),
            "22,80,500-510"
        );
        assert_eq!(
            parse_ports(&json!([502, "8000-8080"])).unwrap(),
            "502,8000-8080"
        );
        assert_eq!(parse_ports(&json!(502)).unwrap(), "502");
        for bad in [
            json!("0"),
            json!("65536"),
            json!("80-22"),
            json!("22,,80"),
            json!(""),
            json!([]),
            json!("-p 22"),
            json!("22;reboot"),
            json!([true]),
            json!({}),
        ] {
            assert!(parse_ports(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn interfaces() {
        for ok in ["eth0", "wlan0", "enp1s0", "br-1a2b", "eth0.100", "wwan0"] {
            assert!(valid_interface(ok), "{ok}");
        }
        for bad in ["", "-i", "eth0 ; ls", "a/b", "sixteen_chars_xx", ".."] {
            assert!(!valid_interface(bad), "{bad}");
        }
    }
}
