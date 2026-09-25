//! `probe_modbus`, `read_modbus`, `write_modbus`: Modbus RTU (serial) or TCP
//! through the image's `mbpoll`, always in the container (the host doesn't
//! ship it; the privileged container sees the host's `/dev` and network).
//!
//! mbpoll is always run with every link setting explicit (it defaults to
//! 19200 baud, EVEN parity), `-0` so registers are 0-based PDU addresses,
//! and `-1 -q` for one quiet poll. It prints read values as `[N]:\t<value>`
//! (holding values over 32767 as `65535 (-1)`), a write as `Written 1
//! references.`, and failures on stderr as `Read ... failed: <reason>` or
//! `mbpoll: Connection failed: <reason>.`, exiting 1.

use std::path::Path;
use std::time::Duration;

use doover::rpc::RpcError;
use serde::Serialize;
use serde_json::{json, Value};

use crate::diag::{invalid, Place, Runner, Step};
use crate::executor::CommandResult;
use crate::parse::Params;

pub const MAX_PROBE_UNITS: usize = 32;
pub const MAX_PROBE_COUNT: i64 = 16;
/// mbpoll's (and Modbus's) most registers in one read.
pub const MAX_READ_COUNT: i64 = 125;
pub const MAX_TIMEOUT: f64 = 5.0;
const BAUDS: &[i64] = &[
    1200, 2400, 4800, 9600, 14400, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

impl Parity {
    fn flag(self) -> &'static str {
        match self {
            Parity::None => "none",
            Parity::Even => "even",
            Parity::Odd => "odd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Rtu {
        port: String,
        baud: u32,
        parity: Parity,
        stop_bits: u8,
        data_bits: u8,
    },
    Tcp {
        host: String,
        port: u16,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub transport: Transport,
    /// Seconds mbpoll waits for a reply.
    pub timeout: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Holding,
    Input,
    Coil,
    Discrete,
}

impl Kind {
    fn parse(s: &str) -> Self {
        match s {
            "input" => Kind::Input,
            "coil" => Kind::Coil,
            "discrete" => Kind::Discrete,
            _ => Kind::Holding,
        }
    }

    /// mbpoll `-t`.
    fn code(self) -> &'static str {
        match self {
            Kind::Holding => "4",
            Kind::Input => "3",
            Kind::Coil => "0",
            Kind::Discrete => "1",
        }
    }

    fn is_bit(self) -> bool {
        matches!(self, Kind::Coil | Kind::Discrete)
    }
}

const LINK_KEYS: &[&str] = &[
    "transport",
    "port",
    "host",
    "tcp_port",
    "baud",
    "parity",
    "stop_bits",
    "data_bits",
    "timeout",
];

/// A serial device path: `/dev/tty` and letters/digits.
pub fn valid_serial_port(s: &str) -> bool {
    s.strip_prefix("/dev/tty")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// A TCP host: an IPv4 address or a hostname.
pub fn valid_host(s: &str) -> bool {
    (1..=253).contains(&s.len())
        && !s.starts_with(['-', '.'])
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
}

impl Link {
    /// The link parameters. Settings for the other transport are ignored.
    fn parse(p: &Params) -> Result<Self, String> {
        let transport = match p
            .choice("transport", &["rtu", "tcp"])?
            .ok_or("'transport' (\"rtu\" or \"tcp\") is required")?
            .as_str()
        {
            "rtu" => {
                let port = p.required_str("port")?;
                if !valid_serial_port(port) {
                    return Err(format!(
                        "'port' {port:?} is not a serial device like /dev/ttyUSB0"
                    ));
                }
                let baud = p.int("baud", 1, 921600)?.unwrap_or(9600);
                if !BAUDS.contains(&baud) {
                    return Err(format!("'baud' {baud} is not a standard rate"));
                }
                // The control backend sends N/E/O.
                let parity = match p
                    .choice("parity", &["none", "even", "odd", "n", "e", "o"])?
                    .as_deref()
                {
                    None | Some("none") | Some("n") => Parity::None,
                    Some("even") | Some("e") => Parity::Even,
                    _ => Parity::Odd,
                };
                let stop_bits = p.int("stop_bits", 1, 2)?.unwrap_or(1) as u8;
                let data_bits = p.int("data_bits", 7, 8)?.unwrap_or(8) as u8;
                Transport::Rtu {
                    port: port.to_string(),
                    baud: baud as u32,
                    parity,
                    stop_bits,
                    data_bits,
                }
            }
            _ => {
                let host = p.required_str("host")?;
                if !valid_host(host) {
                    return Err(format!("'host' {host:?} is not an IP address or hostname"));
                }
                Transport::Tcp {
                    host: host.to_string(),
                    port: p.int("tcp_port", 1, 65535)?.unwrap_or(502) as u16,
                }
            }
        };
        let timeout = match p.number("timeout")? {
            None => 1.0,
            Some(t) if t.is_finite() && t >= 0.01 => t.min(MAX_TIMEOUT),
            Some(_) => return Err("'timeout' must be a number of seconds, at least 0.01".into()),
        };
        Ok(Self { transport, timeout })
    }

    /// Unit ids this transport can address: 0 is broadcast, which never
    /// answers over serial.
    fn min_unit(&self) -> i64 {
        match self.transport {
            Transport::Rtu { .. } => 1,
            Transport::Tcp { .. } => 0,
        }
    }

    /// The serial port has to be there (the privileged container shares the
    /// host's `/dev`).
    fn check_port(&self) -> Result<(), RpcError> {
        match &self.transport {
            Transport::Rtu { port, .. } if !Path::new(port).exists() => Err(RpcError::new(
                "PORT_NOT_FOUND",
                format!("no serial port {port} on the device"),
            )),
            _ => Ok(()),
        }
    }

    /// mbpoll's options for the link, before the target.
    fn options(&self) -> Vec<String> {
        let mut argv: Vec<String> = Vec::new();
        match &self.transport {
            Transport::Rtu {
                baud,
                parity,
                stop_bits,
                data_bits,
                ..
            } => argv.extend([
                "-m".into(),
                "rtu".into(),
                "-b".into(),
                baud.to_string(),
                "-P".into(),
                parity.flag().into(),
                "-s".into(),
                stop_bits.to_string(),
                "-d".into(),
                data_bits.to_string(),
            ]),
            Transport::Tcp { port, .. } => {
                argv.extend(["-m".into(), "tcp".into(), "-p".into(), port.to_string()])
            }
        }
        argv
    }

    fn target(&self) -> &str {
        match &self.transport {
            Transport::Rtu { port, .. } => port,
            Transport::Tcp { host, .. } => host,
        }
    }

    /// Bound on one mbpoll run: its reply timeout plus connection setup.
    fn run_timeout(&self) -> Duration {
        Duration::from_secs_f64(self.timeout + 5.0)
    }
}

/// `mbpoll ... -a <unit> -0 -r <register> -c <count> -t <kind> -o <t> -1 -q <target>`.
pub fn read_argv(link: &Link, unit: u8, register: u16, count: u16, kind: Kind) -> Vec<String> {
    let mut argv = vec!["mbpoll".to_string()];
    argv.extend(link.options());
    argv.extend([
        "-a".into(),
        unit.to_string(),
        "-0".into(),
        "-r".into(),
        register.to_string(),
        "-c".into(),
        count.to_string(),
        "-t".into(),
        kind.code().into(),
        "-o".into(),
        link.timeout.to_string(),
        "-1".into(),
        "-q".into(),
        link.target().into(),
    ]);
    argv
}

/// The single-value write: no `-c` (mbpoll refuses it with a value), the
/// value last.
pub fn write_argv(link: &Link, unit: u8, register: u16, kind: Kind, value: u16) -> Vec<String> {
    let mut argv = vec!["mbpoll".to_string()];
    argv.extend(link.options());
    argv.extend([
        "-a".into(),
        unit.to_string(),
        "-0".into(),
        "-r".into(),
        register.to_string(),
        "-t".into(),
        kind.code().into(),
        "-o".into(),
        link.timeout.to_string(),
        "-1".into(),
        "-q".into(),
        link.target().into(),
        value.to_string(),
    ]);
    argv
}

/// The `[N]:\t<value>` lines of a read, as numbers (registers) or booleans
/// (coils, discrete inputs). A register over 32767 prints as
/// `65535 (-1)`; the unsigned value is the one kept.
pub fn parse_values(out: &str, kind: Kind) -> Result<Vec<Value>, String> {
    let mut values = Vec::new();
    for line in out.lines() {
        let Some(rest) = line.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some((index, value)) = rest.split_once("]:") else {
            continue;
        };
        if index.parse::<u32>().is_err() {
            continue;
        }
        let raw = value.split_whitespace().next().unwrap_or("");
        let n: u16 = raw
            .parse()
            .map_err(|_| format!("mbpoll printed an unexpected value {raw:?}"))?;
        values.push(if kind.is_bit() {
            json!(n != 0)
        } else {
            json!(n)
        });
    }
    Ok(values)
}

/// Whether a write's output confirms it.
pub fn parse_written(out: &str) -> bool {
    out.lines().any(|l| {
        l.trim()
            .strip_prefix("Written ")
            .and_then(|r| r.strip_suffix(" references."))
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n >= 1)
    })
}

/// Why mbpoll failed: its stderr's first line, without the `mbpoll: ` prefix
/// or a trailing full stop.
pub fn failure(r: &CommandResult) -> String {
    if r.timed_out {
        return format!("mbpoll: timed out after {:.0}s", r.duration);
    }
    if r.cancelled {
        return "mbpoll: cancelled".into();
    }
    let line = r
        .stderr
        .lines()
        .chain(r.stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("-- Polling"))
        .unwrap_or("");
    let line = line.strip_prefix("mbpoll: ").unwrap_or(line);
    let line = line.strip_suffix('.').unwrap_or(line);
    if line.is_empty() {
        format!("mbpoll: exit {}", r.exit_code.map_or(-1, i64::from))
    } else {
        line.to_string()
    }
}

/// A failure that isn't about one unit: the port or host can't be reached
/// at all, so probing further unit ids is pointless.
pub fn link_failure(reason: &str) -> bool {
    reason.starts_with("Connection failed")
}

/// Modbus exception responses (libmodbus's texts): the unit is there and
/// answered, just not with data.
pub fn is_exception(reason: &str) -> bool {
    let reason = reason.rsplit(": ").next().unwrap_or(reason);
    [
        "Illegal function",
        "Illegal data address",
        "Illegal data value",
        "Slave device or server failure",
        "Acknowledge",
        "Slave device or server is busy",
        "Negative acknowledge",
        "Memory parity error",
        "Gateway path unavailable",
    ]
    .contains(&reason)
}

// -- params ------------------------------------------------------------------

fn kind(p: &Params, allowed: &[&str]) -> Result<Kind, String> {
    Ok(Kind::parse(
        &p.choice("kind", allowed)?
            .unwrap_or_else(|| "holding".into()),
    ))
}

const READ_KINDS: &[&str] = &["holding", "input", "coil", "discrete"];

fn register_and_count(p: &Params, max_count: i64) -> Result<(u16, u16), String> {
    let register = p.int("register", 0, 65535)?.unwrap_or(0);
    let count = p.int("count", 1, max_count)?.unwrap_or(1);
    if register + count > 65536 {
        return Err("'register' + 'count' runs past 65535".into());
    }
    Ok((register as u16, count as u16))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeParams {
    pub link: Link,
    pub unit_ids: Vec<u8>,
    pub register: u16,
    pub count: u16,
    pub kind: Kind,
}

impl ProbeParams {
    pub fn parse(payload: &Value) -> Result<Self, RpcError> {
        let parsed = Self::parse_inner(payload).map_err(invalid)?;
        parsed.link.check_port()?;
        Ok(parsed)
    }

    fn parse_inner(payload: &Value) -> Result<Self, String> {
        let p = Params::new(payload)?;
        p.only(&[LINK_KEYS, &["unit_ids", "register", "count", "kind"]].concat())?;
        let link = Link::parse(&p)?;
        let min = link.min_unit();
        let unit_ids = match p.get("unit_ids") {
            None => vec![1],
            Some(Value::Array(ids)) if (1..=MAX_PROBE_UNITS).contains(&ids.len()) => {
                let mut out: Vec<u8> = Vec::new();
                for id in ids {
                    match id.as_i64() {
                        Some(n) if (min..=247).contains(&n) => {
                            if !out.contains(&(n as u8)) {
                                out.push(n as u8);
                            }
                        }
                        _ => return Err(format!("'unit_ids' entries must be {min} to 247")),
                    }
                }
                out
            }
            Some(_) => {
                return Err(format!(
                    "'unit_ids' must be a list of 1 to {MAX_PROBE_UNITS} unit ids"
                ))
            }
        };
        let (register, count) = register_and_count(&p, MAX_PROBE_COUNT)?;
        Ok(Self {
            link,
            unit_ids,
            register,
            count,
            kind: kind(&p, READ_KINDS)?,
        })
    }
}

fn unit_id(p: &Params, link: &Link) -> Result<u8, String> {
    let min = link.min_unit();
    Ok(p.int("unit_id", min, 247)?.ok_or("'unit_id' is required")? as u8)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadParams {
    pub link: Link,
    pub unit_id: u8,
    pub register: u16,
    pub count: u16,
    pub kind: Kind,
}

impl ReadParams {
    pub fn parse(payload: &Value) -> Result<Self, RpcError> {
        let parsed = Self::parse_inner(payload).map_err(invalid)?;
        parsed.link.check_port()?;
        Ok(parsed)
    }

    fn parse_inner(payload: &Value) -> Result<Self, String> {
        let p = Params::new(payload)?;
        p.only(&[LINK_KEYS, &["unit_id", "register", "count", "kind"]].concat())?;
        let link = Link::parse(&p)?;
        let unit_id = unit_id(&p, &link)?;
        let (register, count) = register_and_count(&p, MAX_READ_COUNT)?;
        Ok(Self {
            unit_id,
            register,
            count,
            kind: kind(&p, READ_KINDS)?,
            link,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WriteParams {
    pub link: Link,
    pub unit_id: u8,
    pub register: u16,
    pub kind: Kind,
    /// 0-65535 for a holding register, 0/1 for a coil.
    pub value: u16,
}

impl WriteParams {
    pub fn parse(payload: &Value) -> Result<Self, RpcError> {
        let parsed = Self::parse_inner(payload).map_err(invalid)?;
        parsed.link.check_port()?;
        Ok(parsed)
    }

    fn parse_inner(payload: &Value) -> Result<Self, String> {
        let p = Params::new(payload)?;
        // `count` is accepted only as 1: this writes one register or coil.
        p.only(
            &[
                LINK_KEYS,
                &["unit_id", "register", "kind", "value", "count"],
            ]
            .concat(),
        )?;
        if p.int("count", 1, 1).is_err() {
            return Err("write_modbus writes one register or coil; 'count' must be 1".into());
        }
        let link = Link::parse(&p)?;
        let unit_id = unit_id(&p, &link)?;
        let register = p
            .int("register", 0, 65535)?
            .ok_or("'register' is required")? as u16;
        let kind = kind(&p, &["holding", "coil"])?;
        let value = match (kind, p.get("value")) {
            (_, None) => return Err("'value' is required".into()),
            (_, Some(Value::Array(_))) => {
                return Err(
                    "write_modbus writes one value; multi-register writes are refused".into(),
                )
            }
            (Kind::Coil, Some(Value::Bool(b))) => u16::from(*b),
            (Kind::Coil, Some(_)) => p
                .int("value", 0, 1)
                .map_err(|_| "a coil 'value' must be true or false".to_string())?
                .unwrap_or(0) as u16,
            (_, Some(Value::Bool(_))) => {
                return Err("a holding register 'value' must be an integer 0 to 65535".into())
            }
            (_, Some(_)) => p
                .int("value", 0, 65535)
                .map_err(|_| {
                    "a holding register 'value' must be an integer 0 to 65535".to_string()
                })?
                .unwrap_or(0) as u16,
        };
        Ok(Self {
            link,
            unit_id,
            register,
            kind,
            value,
        })
    }
}

// -- methods -----------------------------------------------------------------

async fn mbpoll(runner: &Runner, link: &Link, argv: &[String]) -> Result<CommandResult, RpcError> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    runner
        .run(Place::Container, &argv, None, link.run_timeout())
        .await
        .map_err(|e| RpcError::new("MODBUS_FAILED", e))
}

/// One read; `Err` is mbpoll's reason.
async fn read(
    runner: &Runner,
    link: &Link,
    unit: u8,
    register: u16,
    count: u16,
    kind: Kind,
) -> Result<Result<Vec<Value>, String>, RpcError> {
    let r = mbpoll(runner, link, &read_argv(link, unit, register, count, kind)).await?;
    if r.exit_code != Some(0) {
        return Ok(Err(failure(&r)));
    }
    Ok(match parse_values(&r.stdout, kind) {
        Ok(v) if v.len() == count as usize => Ok(v),
        Ok(v) => Err(format!(
            "expected {count} values, mbpoll printed {}",
            v.len()
        )),
        Err(e) => Err(e),
    })
}

#[derive(Debug, Serialize)]
struct ProbeResult {
    unit_id: u8,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The unit answered with a Modbus exception: it's there, the register
    /// or function isn't.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    exception: bool,
}

/// Which unit ids answer. A link-level failure (port or host unreachable)
/// stops the probe and goes in `errors`.
pub async fn probe_modbus(
    runner: &Runner,
    step: &Step,
    params: ProbeParams,
) -> Result<Value, RpcError> {
    let total = params.unit_ids.len();
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for (i, &unit) in params.unit_ids.iter().enumerate() {
        if runner.cancelled() {
            break;
        }
        step.set(format!("Probing unit {}/{total}", i + 1));
        let outcome = read(
            runner,
            &params.link,
            unit,
            params.register,
            params.count,
            params.kind,
        )
        .await?;
        match outcome {
            Ok(values) => results.push(ProbeResult {
                unit_id: unit,
                ok: true,
                values: Some(values),
                error: None,
                exception: false,
            }),
            Err(reason) => {
                let stop = link_failure(&reason);
                if stop {
                    errors.push(format!("{}: {reason}", params.link.target()));
                }
                results.push(ProbeResult {
                    unit_id: unit,
                    ok: false,
                    values: None,
                    exception: is_exception(&reason),
                    error: Some(reason),
                });
                if stop {
                    break;
                }
            }
        }
    }
    let answered = results.iter().filter(|r| r.ok || r.exception).count();
    Ok(json!({
        "results": results,
        "attempted": results.len(),
        "answered": answered,
        "errors": errors,
    }))
}

pub async fn read_modbus(
    runner: &Runner,
    step: &Step,
    params: ReadParams,
) -> Result<Value, RpcError> {
    step.set(format!(
        "Reading {} {} on unit {}",
        params.count, params.register, params.unit_id
    ));
    let values = read(
        runner,
        &params.link,
        params.unit_id,
        params.register,
        params.count,
        params.kind,
    )
    .await?
    .map_err(|e| RpcError::new("MODBUS_FAILED", e))?;
    Ok(json!({
        "unit_id": params.unit_id,
        "register": params.register,
        "kind": params.kind,
        "values": values,
    }))
}

/// Modicon-style number for progress: holding 0 is 40001, coil 0 is 1.
fn reference(kind: Kind, register: u16) -> String {
    match kind {
        Kind::Holding if register < 9999 => format!("register {}", 40001 + u32::from(register)),
        Kind::Holding => format!("register {}", 400001 + u32::from(register)),
        _ => format!("coil {}", u32::from(register) + 1),
    }
}

pub async fn write_modbus(
    runner: &Runner,
    step: &Step,
    params: WriteParams,
) -> Result<Value, RpcError> {
    let WriteParams {
        link,
        unit_id,
        register,
        kind,
        value,
    } = params;
    step.set(format!(
        "Writing {} on unit {unit_id}",
        reference(kind, register)
    ));
    let r = mbpoll(
        runner,
        &link,
        &write_argv(&link, unit_id, register, kind, value),
    )
    .await?;
    if r.exit_code != Some(0) || !parse_written(&r.stdout) {
        return Err(RpcError::new("MODBUS_FAILED", failure(&r)));
    }
    let as_json = |v: u16| {
        if kind.is_bit() {
            json!(v != 0)
        } else {
            json!(v)
        }
    };
    let mut out = json!({
        "ok": true,
        "unit_id": unit_id,
        "register": register,
        "kind": kind,
        "value": as_json(value),
    });
    step.set(format!("Reading back {}", reference(kind, register)));
    match read(runner, &link, unit_id, register, 1, kind).await? {
        Ok(values) => out["readback"] = values[0].clone(),
        Err(e) => out["readback_error"] = json!(e),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtu() -> Link {
        Link {
            transport: Transport::Rtu {
                port: "/dev/ttyUSB0".into(),
                baud: 9600,
                parity: Parity::None,
                stop_bits: 1,
                data_bits: 8,
            },
            timeout: 1.0,
        }
    }

    fn tcp() -> Link {
        Link {
            transport: Transport::Tcp {
                host: "192.168.1.40".into(),
                port: 502,
            },
            timeout: 0.5,
        }
    }

    fn joined(argv: Vec<String>) -> String {
        argv.join(" ")
    }

    #[test]
    fn read_commands() {
        assert_eq!(
            joined(read_argv(&rtu(), 3, 0, 2, Kind::Holding)),
            "mbpoll -m rtu -b 9600 -P none -s 1 -d 8 -a 3 -0 -r 0 -c 2 -t 4 -o 1 -1 -q /dev/ttyUSB0"
        );
        assert_eq!(
            joined(read_argv(&tcp(), 1, 100, 1, Kind::Input)),
            "mbpoll -m tcp -p 502 -a 1 -0 -r 100 -c 1 -t 3 -o 0.5 -1 -q 192.168.1.40"
        );
        assert!(joined(read_argv(&tcp(), 1, 0, 1, Kind::Coil)).contains(" -t 0 "));
        assert!(joined(read_argv(&tcp(), 1, 0, 1, Kind::Discrete)).contains(" -t 1 "));
    }

    #[test]
    fn write_commands() {
        assert_eq!(
            joined(write_argv(&rtu(), 1, 5, Kind::Holding, 1234)),
            "mbpoll -m rtu -b 9600 -P none -s 1 -d 8 -a 1 -0 -r 5 -t 4 -o 1 -1 -q /dev/ttyUSB0 1234"
        );
        assert_eq!(
            joined(write_argv(&tcp(), 1, 2, Kind::Coil, 1)),
            "mbpoll -m tcp -p 502 -a 1 -0 -r 2 -t 0 -o 0.5 -1 -q 192.168.1.40 1"
        );
    }

    // Fixtures captured from mbpoll 1.5-4 (the image's) against a pymodbus
    // TCP server.
    const HOLDING: &str =
        "-- Polling slave 1...\n[0]: \t200\n[1]: \t65535 (-1)\n[2]: \t4\n[3]: \t0\n\n";
    const COILS: &str = "-- Polling slave 1...\n[0]: \t1\n[1]: \t1\n[2]: \t0\n[3]: \t0\n\n";
    const OFFSET: &str = "-- Polling slave 1...\n[3]: \t65535 (-1)\n\n";

    #[test]
    fn values() {
        assert_eq!(
            parse_values(HOLDING, Kind::Holding).unwrap(),
            [json!(200), json!(65535), json!(4), json!(0)]
        );
        assert_eq!(
            parse_values(COILS, Kind::Coil).unwrap(),
            [json!(true), json!(true), json!(false), json!(false)]
        );
        assert_eq!(parse_values(OFFSET, Kind::Input).unwrap(), [json!(65535)]);
        assert_eq!(
            parse_values("-- Polling slave 2...\n\n", Kind::Holding).unwrap(),
            Vec::<Value>::new()
        );
        assert!(parse_values("[0]: \tnan\n", Kind::Holding).is_err());
    }

    #[test]
    fn written() {
        assert!(parse_written("Written 1 references.\n\n"));
        assert!(!parse_written("-- Polling slave 1...\n"));
        assert!(!parse_written(""));
    }

    fn result(exit: i32, stdout: &str, stderr: &str) -> CommandResult {
        CommandResult {
            exit_code: Some(exit),
            stdout: stdout.into(),
            stderr: stderr.into(),
            duration: 0.1,
            timed_out: false,
            cancelled: false,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn failures() {
        let timeout = result(
            1,
            "-- Polling slave 2...\n\n",
            "Read output (holding) register failed: Operation timed out\n",
        );
        assert_eq!(
            failure(&timeout),
            "Read output (holding) register failed: Operation timed out"
        );
        assert!(!is_exception(&failure(&timeout)));
        assert!(!link_failure(&failure(&timeout)));

        let illegal = result(
            1,
            "-- Polling slave 1...\n\n",
            "Read output (holding) register failed: Illegal data address\n",
        );
        assert!(is_exception(&failure(&illegal)));

        let refused = result(1, "", "mbpoll: Connection failed: Connection refused.\n");
        assert_eq!(failure(&refused), "Connection failed: Connection refused");
        assert!(link_failure(&failure(&refused)));

        let no_port = result(
            1,
            "",
            "mbpoll: Connection failed: No such file or directory.\n",
        );
        assert!(link_failure(&failure(&no_port)));

        let write = result(
            1,
            "\n",
            "Write output (holding) register failed: Illegal data address\n",
        );
        assert_eq!(
            failure(&write),
            "Write output (holding) register failed: Illegal data address"
        );
        assert_eq!(failure(&result(3, "", "")), "mbpoll: exit 3");
    }

    #[test]
    fn references() {
        assert_eq!(reference(Kind::Holding, 0), "register 40001");
        assert_eq!(reference(Kind::Holding, 9999), "register 410000");
        assert_eq!(reference(Kind::Coil, 4), "coil 5");
    }

    #[test]
    fn link_params() {
        let link = |v: Value| Link::parse(&Params::new(&v).unwrap());
        assert_eq!(
            link(json!({"transport": "rtu", "port": "/dev/ttyUSB0"})).unwrap(),
            rtu()
        );
        // The backend's parity letters.
        let l = link(
            json!({"transport": "rtu", "port": "/dev/ttyS1", "baud": 19200,
                            "parity": "E", "stop_bits": 2, "data_bits": 7, "timeout": 9}),
        )
        .unwrap();
        assert_eq!(
            l.transport,
            Transport::Rtu {
                port: "/dev/ttyS1".into(),
                baud: 19200,
                parity: Parity::Even,
                stop_bits: 2,
                data_bits: 7
            }
        );
        assert_eq!(l.timeout, MAX_TIMEOUT);
        assert_eq!(
            link(json!({"transport": "tcp", "host": "plc.local"}))
                .unwrap()
                .transport,
            Transport::Tcp {
                host: "plc.local".into(),
                port: 502
            }
        );
        for bad in [
            json!({}),
            json!({"transport": "ascii"}),
            json!({"transport": "rtu"}),
            json!({"transport": "rtu", "port": "/dev/sda"}),
            json!({"transport": "rtu", "port": "/dev/ttyUSB0; reboot"}),
            json!({"transport": "rtu", "port": "/dev/tty"}),
            json!({"transport": "rtu", "port": "/dev/ttyUSB0", "baud": 1234}),
            json!({"transport": "rtu", "port": "/dev/ttyUSB0", "parity": "mark"}),
            json!({"transport": "rtu", "port": "/dev/ttyUSB0", "stop_bits": 3}),
            json!({"transport": "rtu", "port": "/dev/ttyUSB0", "data_bits": 5}),
            json!({"transport": "rtu", "port": "/dev/ttyUSB0", "timeout": 0}),
            json!({"transport": "tcp"}),
            json!({"transport": "tcp", "host": "-oProxy"}),
            json!({"transport": "tcp", "host": "a b"}),
            json!({"transport": "tcp", "host": "h", "tcp_port": 0}),
        ] {
            assert!(link(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn probe_params() {
        let p = ProbeParams::parse_inner(&json!({"transport": "tcp", "host": "10.0.0.5",
            "unit_ids": [1, 2, 2, 0], "register": 10, "count": 2, "kind": "input"}))
        .unwrap();
        assert_eq!(p.unit_ids, [1, 2, 0]);
        assert_eq!((p.register, p.count, p.kind), (10, 2, Kind::Input));
        let d = ProbeParams::parse_inner(&json!({"transport": "tcp", "host": "h"})).unwrap();
        assert_eq!(
            (d.unit_ids.as_slice(), d.register, d.count, d.kind),
            (&[1u8][..], 0, 1, Kind::Holding)
        );
        for bad in [
            json!({"transport": "tcp", "host": "h", "unit_ids": []}),
            json!({"transport": "tcp", "host": "h", "unit_ids": (1..=33).collect::<Vec<_>>()}),
            json!({"transport": "tcp", "host": "h", "unit_ids": [248]}),
            json!({"transport": "tcp", "host": "h", "unit_ids": 1}),
            json!({"transport": "rtu", "port": "/dev/ttyUSB0", "unit_ids": [0]}),
            json!({"transport": "tcp", "host": "h", "count": 17}),
            json!({"transport": "tcp", "host": "h", "register": 65535, "count": 2}),
            json!({"transport": "tcp", "host": "h", "kind": "string"}),
            json!({"transport": "tcp", "host": "h", "unit_id": 1}),
        ] {
            assert!(ProbeParams::parse_inner(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn read_params() {
        let p = ReadParams::parse_inner(
            &json!({"transport": "tcp", "host": "h", "unit_id": 3, "register": 7, "count": 125}),
        )
        .unwrap();
        assert_eq!((p.unit_id, p.register, p.count), (3, 7, 125));
        for bad in [
            json!({"transport": "tcp", "host": "h"}),
            json!({"transport": "tcp", "host": "h", "unit_id": 1, "count": 126}),
            json!({"transport": "tcp", "host": "h", "unit_ids": [1]}),
        ] {
            assert!(ReadParams::parse_inner(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn write_params() {
        let w = |v: Value| WriteParams::parse_inner(&v);
        let base = json!({"transport": "tcp", "host": "h", "unit_id": 1, "register": 3});
        let with = |extra: Value| {
            let mut v = base.clone();
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            v
        };
        assert_eq!(w(with(json!({"value": 65535}))).unwrap().value, 65535);
        assert_eq!(
            w(with(json!({"value": true, "kind": "coil"})))
                .unwrap()
                .value,
            1
        );
        assert_eq!(
            w(with(json!({"value": 0, "kind": "coil", "count": 1})))
                .unwrap()
                .value,
            0
        );
        for bad in [
            with(json!({})),
            with(json!({"value": 65536})),
            with(json!({"value": -1})),
            with(json!({"value": 1.5})),
            with(json!({"value": "12"})),
            with(json!({"value": true})),
            with(json!({"value": [1, 2]})),
            with(json!({"value": 1, "count": 2})),
            with(json!({"value": 2, "kind": "coil"})),
            with(json!({"value": 1, "kind": "input"})),
            with(json!({"value": 1, "kind": "discrete"})),
            with(json!({"value": 1, "kind": "bogus"})),
            json!({"transport": "tcp", "host": "h", "unit_id": 1, "value": 1}),
        ] {
            assert!(w(bad.clone()).is_err(), "{bad}");
        }
    }
}
