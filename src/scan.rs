//! `scan_modbus`: find a Modbus RTU device on the Doovit's serial ports.
//!
//! Unlike `probe_modbus` (one mbpoll process per unit id, the caller picks the
//! ids), this always covers the whole address range and is native: each port
//! is opened once with raw termios and every unit is asked for holding
//! register 0 with a short reply timeout, so a full sweep is ~0.3 s per
//! unit instead of ~1 s. The order is deterministic:
//!
//! - ports: RS-485 (`ttyAMA*`, `ttySC*`) first, then USB serial adapters;
//!   cellular modems, MicroPython boards and the console are never touched;
//! - serial settings: a fixed list of common ones (9600 8N1 first);
//! - unit ids: 1, 247, 2, 246, 3, 245 … 124.
//!
//! Hints (from documentation or other context) only move candidates to the
//! front; they never shrink the scan. Ports are scanned in parallel (each is
//! its own bus), units on one port in sequence (RS-485 is half duplex: one
//! request on the wire at a time). The scan stops at the first unit that
//! answers, with data or a Modbus exception (both prove a device is there).

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use doover::rpc::RpcError;
use serde::Serialize;
use serde_json::{json, Value};

use crate::diag::{invalid, Step};
use crate::modbus::Parity;
use crate::parse::Params;

pub const MAX_UNIT: u8 = 247;
pub const DEFAULT_TIMEOUT: f64 = 0.3;
pub const MAX_HINT_UNITS: usize = 8;

/// Common settings, most likely first. Always 8 data bits.
pub const SETTINGS: &[Settings] = &[
    Settings::new(9600, Parity::None, 1),
    Settings::new(9600, Parity::Even, 1),
    Settings::new(19200, Parity::None, 1),
    Settings::new(19200, Parity::Even, 1),
    Settings::new(38400, Parity::None, 1),
    Settings::new(115200, Parity::None, 1),
];

/// USB serial devices that are never a Modbus bus (matched case-insensitively
/// against the USB manufacturer and product strings).
const NEVER_SCAN: &[(&str, &str)] = &[
    ("simtech", "cellular modem"),
    ("quectel", "cellular modem"),
    ("sierra", "cellular modem"),
    ("telit", "cellular modem"),
    ("u-blox", "cellular modem"),
    ("huawei", "cellular modem"),
    ("fibocom", "cellular modem"),
    ("zte", "cellular modem"),
    ("micropython", "MicroPython board"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Settings {
    pub baud: u32,
    #[serde(serialize_with = "parity_name")]
    pub parity: Parity,
    pub stop_bits: u8,
}

fn parity_name<S: serde::Serializer>(p: &Parity, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match p {
        Parity::None => "none",
        Parity::Even => "even",
        Parity::Odd => "odd",
    })
}

impl Settings {
    pub const fn new(baud: u32, parity: Parity, stop_bits: u8) -> Self {
        Self {
            baud,
            parity,
            stop_bits,
        }
    }

    /// "9600 8N1".
    pub fn label(&self) -> String {
        let p = match self.parity {
            Parity::None => 'N',
            Parity::Even => 'E',
            Parity::Odd => 'O',
        };
        format!("{} 8{p}{}", self.baud, self.stop_bits)
    }
}

// -- order -------------------------------------------------------------------

/// Every unit id 1..=247 exactly once: hints first, then from both ends
/// inwards (1, 247, 2, 246 …).
pub fn unit_order(hints: &[u8]) -> Vec<u8> {
    let mut seen = HashSet::new();
    let mut order = Vec::with_capacity(MAX_UNIT as usize);
    for &u in hints {
        if (1..=MAX_UNIT).contains(&u) && seen.insert(u) {
            order.push(u);
        }
    }
    let (mut low, mut high) = (1u8, MAX_UNIT);
    while low <= high {
        for u in [low, high] {
            if seen.insert(u) {
                order.push(u);
            }
        }
        low += 1;
        high -= 1;
    }
    order
}

/// The settings list with hinted settings moved to the front.
pub fn settings_order(hints: &[Settings]) -> Vec<Settings> {
    let mut order: Vec<Settings> = Vec::new();
    for s in hints.iter().chain(SETTINGS) {
        if !order.contains(s) {
            order.push(*s);
        }
    }
    order
}

// -- frames ------------------------------------------------------------------

/// Modbus RTU CRC-16 (poly 0xA001, init 0xFFFF), sent low byte first.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Read holding registers (0x03), register 0, count 1.
pub fn request(unit: u8) -> [u8; 8] {
    let mut f = [unit, 0x03, 0x00, 0x00, 0x00, 0x01, 0, 0];
    let crc = crc16(&f[..6]);
    f[6] = (crc & 0xFF) as u8;
    f[7] = (crc >> 8) as u8;
    f
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A normal response from `unit`.
    Data,
    /// A Modbus exception from `unit` (code): it is there.
    Exception(u8),
    /// A valid frame from a different unit (another device or crosstalk).
    OtherUnit(u8),
    /// Bytes that aren't a valid reply (noise, wrong baud, another master).
    Garbage,
}

/// Classify the bytes read after asking `unit`. `None`: nothing arrived.
pub fn classify(unit: u8, buf: &[u8]) -> Option<Reply> {
    if buf.is_empty() {
        return None;
    }
    let frame_ok = |len: usize| {
        buf.len() >= len && {
            let crc = crc16(&buf[..len - 2]);
            buf[len - 2] == (crc & 0xFF) as u8 && buf[len - 1] == (crc >> 8) as u8
        }
    };
    let reply = if buf.len() >= 5 && buf[1] == 0x83 && frame_ok(5) {
        Some((buf[0], Reply::Exception(buf[2])))
    } else if buf.len() >= 3 && buf[1] == 0x03 && frame_ok(3 + buf[2] as usize + 2) {
        Some((buf[0], Reply::Data))
    } else {
        None
    };
    Some(match reply {
        Some((from, r)) if from == unit => r,
        Some((from, _)) => Reply::OtherUnit(from),
        None => Reply::Garbage,
    })
}

/// Bytes a reply needs, once its header has arrived.
fn expected_len(buf: &[u8]) -> usize {
    match buf {
        [_, f, ..] if *f & 0x80 != 0 => 5,
        [_, _, n, ..] => 3 + *n as usize + 2,
        _ => 5,
    }
}

// -- serial ------------------------------------------------------------------

fn baud_flag(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        1200 => libc::B1200,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        230400 => libc::B230400,
        _ => return None,
    })
}

/// An open serial port in raw mode.
pub struct Port {
    fd: OwnedFd,
}

impl Port {
    pub fn open(path: &Path) -> io::Result<Self> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad path"))?;
        // SAFETY: c is a valid C string; the fd is owned from here on.
        let fd =
            unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn configure(&self, s: &Settings) -> io::Result<()> {
        let speed = baud_flag(s.baud)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unsupported baud"))?;
        let fd = self.fd.as_raw_fd();
        // SAFETY: termios is plain data filled by tcgetattr on a valid fd.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut t);
            t.c_cflag &=
                !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS);
            t.c_cflag |= libc::CS8 | libc::CLOCAL | libc::CREAD;
            match s.parity {
                Parity::None => {}
                Parity::Even => t.c_cflag |= libc::PARENB,
                Parity::Odd => t.c_cflag |= libc::PARENB | libc::PARODD,
            }
            if s.stop_bits == 2 {
                t.c_cflag |= libc::CSTOPB;
            }
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 0;
            libc::cfsetispeed(&mut t, speed);
            libc::cfsetospeed(&mut t, speed);
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::tcflush(fd, libc::TCIOFLUSH);
        }
        Ok(())
    }

    /// Send `frame` and collect a reply until it is complete or `timeout`
    /// passes. A silent gap after the first byte also ends the reply.
    pub fn transact(&self, frame: &[u8], timeout: Duration, gap: Duration) -> io::Result<Vec<u8>> {
        let fd = self.fd.as_raw_fd();
        unsafe {
            libc::tcflush(fd, libc::TCIOFLUSH);
            let n = libc::write(fd, frame.as_ptr().cast(), frame.len());
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::tcdrain(fd);
        }
        let deadline = Instant::now() + timeout;
        let mut buf = Vec::new();
        loop {
            let now = Instant::now();
            if now >= deadline || (!buf.is_empty() && buf.len() >= expected_len(&buf)) {
                return Ok(buf);
            }
            let wait = if buf.is_empty() {
                deadline - now
            } else {
                gap.min(deadline - now)
            };
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = wait.as_millis().clamp(1, i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
            if ready < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ready == 0 {
                if !buf.is_empty() {
                    return Ok(buf); // inter-frame silence: the reply is over
                }
                continue;
            }
            let mut chunk = [0u8; 256];
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                buf.extend_from_slice(&chunk[..n as usize]);
            } else if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::WouldBlock {
                    return Err(e);
                }
            }
        }
    }
}

/// 3.5 character times at `baud` (11 bits a character), at least 2 ms: the
/// silence RTU needs between frames.
fn frame_gap(baud: u32) -> Duration {
    Duration::from_secs_f64((3.5 * 11.0 / f64::from(baud)).max(0.002))
}

// -- ports -------------------------------------------------------------------

/// Where to look; tests point these at temporary directories.
#[derive(Debug, Clone)]
pub struct Roots {
    pub dev: PathBuf,
    pub sys_tty: PathBuf,
    pub proc: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Self {
            dev: "/dev".into(),
            sys_tty: "/sys/class/tty".into(),
            proc: "/proc".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub port: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// Processes holding the port open (another Modbus master on the bus
    /// garbles replies).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub in_use_by: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb: Option<String>,
}

fn rank(name: &str) -> Option<u8> {
    if name.starts_with("ttyAMA") || name.starts_with("ttySC") {
        Some(0)
    } else if name.starts_with("ttyUSB") || name.starts_with("ttyACM") {
        Some(1)
    } else {
        None
    }
}

/// The USB "manufacturer product" above a tty in sysfs, if it's USB.
fn usb_identity(sys_tty: &Path, name: &str) -> Option<String> {
    let mut dir = fs::canonicalize(sys_tty.join(name).join("device")).ok()?;
    for _ in 0..6 {
        let manufacturer = fs::read_to_string(dir.join("manufacturer")).ok();
        let product = fs::read_to_string(dir.join("product")).ok();
        if manufacturer.is_some() || product.is_some() {
            let text = format!(
                "{} {}",
                manufacturer.unwrap_or_default().trim(),
                product.unwrap_or_default().trim()
            );
            return Some(text.trim().to_string());
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

fn console_ports(proc: &Path) -> Vec<String> {
    fs::read_to_string(proc.join("cmdline"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|w| w.strip_prefix("console="))
        .map(|c| c.split(',').next().unwrap_or("").to_string())
        .collect()
}

/// Names of processes with `port` open (reads `/proc/*/fd`; the gateway
/// runs with `pid: host`, so this sees the other apps).
fn port_users(proc: &Path, port: &Path, own_pid: u32) -> Vec<String> {
    let mut users = Vec::new();
    let Ok(entries) = fs::read_dir(proc) else {
        return users;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        if fds
            .flatten()
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|t| t == port))
        {
            let comm = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            users.push(format!("{} (pid {pid})", comm.trim()));
        }
    }
    users.sort();
    users.dedup();
    users
}

/// Every serial port worth scanning, RS-485 first, plus the ones skipped and
/// why. `hint_ports` move to the front.
pub fn candidates(roots: &Roots, hint_ports: &[String]) -> Vec<Candidate> {
    let consoles = console_ports(&roots.proc);
    let mut names: Vec<String> = fs::read_dir(&roots.sys_tty)
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| rank(n).is_some())
                .filter(|n| roots.dev.join(n).exists())
                .collect()
        })
        .unwrap_or_default();
    names.sort_by_key(|n| {
        let hinted = hint_ports
            .iter()
            .any(|h| h.trim_start_matches("/dev/") == n);
        (!hinted, rank(n).unwrap_or(9), n.len(), n.clone())
    });
    let own = std::process::id();
    names
        .into_iter()
        .map(|name| {
            let path = roots.dev.join(&name);
            let usb = usb_identity(&roots.sys_tty, &name);
            let lower = usb.clone().unwrap_or_default().to_lowercase();
            let skipped = if consoles.iter().any(|c| c == &name) {
                Some("system console".to_string())
            } else {
                NEVER_SCAN
                    .iter()
                    .find(|(needle, _)| lower.contains(needle))
                    .map(|(_, why)| why.to_string())
            };
            let in_use_by = if skipped.is_none() {
                port_users(&roots.proc, &path, own)
            } else {
                Vec::new()
            };
            Candidate {
                port: path.display().to_string(),
                skipped,
                in_use_by,
                usb,
            }
        })
        .collect()
}

// -- params ------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ModbusScanParams {
    pub hint_unit_ids: Vec<u8>,
    pub hint_ports: Vec<String>,
    pub hint_settings: Vec<Settings>,
    pub timeout: f64,
}

impl ModbusScanParams {
    /// Only hints and the reply timeout: there is deliberately no way to
    /// name a partial range.
    pub fn parse(payload: &Value) -> Result<Self, RpcError> {
        Self::parse_inner(payload).map_err(invalid)
    }

    fn parse_inner(payload: &Value) -> Result<Self, String> {
        let p = Params::new(payload)?;
        p.only(&["hint_unit_ids", "hint_ports", "hint_settings", "timeout"])?;
        let mut hint_unit_ids = Vec::new();
        if let Some(v) = p.get("hint_unit_ids") {
            let ids = v
                .as_array()
                .ok_or_else(|| String::from("'hint_unit_ids' must be a list"))?;
            if ids.len() > MAX_HINT_UNITS {
                return Err(format!("at most {MAX_HINT_UNITS} hint_unit_ids"));
            }
            for id in ids {
                match id.as_u64() {
                    Some(u) if (1..=u64::from(MAX_UNIT)).contains(&u) => {
                        hint_unit_ids.push(u as u8)
                    }
                    _ => return Err(String::from("'hint_unit_ids' entries must be 1 to 247")),
                }
            }
        }
        let mut hint_ports = Vec::new();
        if let Some(v) = p.get("hint_ports") {
            for port in v
                .as_array()
                .ok_or_else(|| String::from("'hint_ports' must be a list"))?
            {
                match port.as_str() {
                    Some(s) if crate::modbus::valid_serial_port(s) => {
                        hint_ports.push(s.to_string())
                    }
                    _ => return Err(String::from("'hint_ports' entries must be /dev/tty… paths")),
                }
            }
        }
        let mut hint_settings = Vec::new();
        if let Some(v) = p.get("hint_settings") {
            for s in v
                .as_array()
                .ok_or_else(|| String::from("'hint_settings' must be a list"))?
            {
                let baud = s.get("baud").and_then(Value::as_u64).unwrap_or(9600) as u32;
                if baud_flag(baud).is_none() {
                    return Err(format!("unsupported baud {baud}"));
                }
                let parity = match s.get("parity").and_then(Value::as_str).unwrap_or("none") {
                    "none" | "N" => Parity::None,
                    "even" | "E" => Parity::Even,
                    "odd" | "O" => Parity::Odd,
                    other => return Err(format!("unknown parity {other:?}")),
                };
                let stop_bits = match s.get("stop_bits").and_then(Value::as_u64).unwrap_or(1) {
                    1 => 1,
                    2 => 2,
                    _ => return Err(String::from("'stop_bits' must be 1 or 2")),
                };
                hint_settings.push(Settings::new(baud, parity, stop_bits));
            }
        }
        let timeout = match p.number("timeout")? {
            None => DEFAULT_TIMEOUT,
            Some(t) if (0.05..=1.0).contains(&t) => t,
            Some(_) => return Err(String::from("'timeout' must be 0.05 to 1 seconds")),
        };
        Ok(Self {
            hint_unit_ids,
            hint_ports,
            hint_settings,
            timeout,
        })
    }
}

// -- the scan ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub port: String,
    #[serde(flatten)]
    pub settings: Settings,
    pub data_bits: u8,
    pub unit_id: u8,
    /// "data", or "exception" with its code.
    pub reply: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_code: Option<u8>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct PortReport {
    pub port: String,
    pub settings_tried: Vec<String>,
    pub units_tried: usize,
    /// Replies that were not a valid answer from the asked unit.
    pub noise: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_units_seen: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Scan one port: every setting, every unit, until `stop` is set or a unit
/// answers. Blocking; run it on a blocking thread.
pub fn scan_port(
    path: &Path,
    settings: &[Settings],
    units: &[u8],
    timeout: Duration,
    stop: &AtomicBool,
    step: &Step,
) -> (PortReport, Option<Found>) {
    let mut report = PortReport {
        port: path.display().to_string(),
        ..Default::default()
    };
    let port = match Port::open(path) {
        Ok(p) => p,
        Err(e) => {
            report.error = Some(format!("could not open: {e}"));
            return (report, None);
        }
    };
    let total = units.len();
    for s in settings {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Err(e) = port.configure(s) {
            report.error = Some(format!("could not set {}: {e}", s.label()));
            continue;
        }
        report.settings_tried.push(s.label());
        let gap = frame_gap(s.baud);
        for (i, &unit) in units.iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                return (report, None);
            }
            step.set(format!(
                "Scanning {} · {} · unit {}/{total}",
                report.port,
                s.label(),
                i + 1
            ));
            report.units_tried += 1;
            let reply = match port.transact(&request(unit), timeout, gap) {
                Ok(buf) => classify(unit, &buf),
                Err(e) => {
                    report.error = Some(format!("read/write failed: {e}"));
                    return (report, None);
                }
            };
            std::thread::sleep(gap);
            match reply {
                None => {}
                Some(Reply::Data) | Some(Reply::Exception(_)) => {
                    let code = match reply {
                        Some(Reply::Exception(c)) => Some(c),
                        _ => None,
                    };
                    return (
                        report.clone(),
                        Some(Found {
                            port: report.port,
                            settings: *s,
                            data_bits: 8,
                            unit_id: unit,
                            reply: if code.is_some() { "exception" } else { "data" },
                            exception_code: code,
                        }),
                    );
                }
                Some(Reply::OtherUnit(u)) => {
                    report.noise += 1;
                    if !report.other_units_seen.contains(&u) {
                        report.other_units_seen.push(u);
                    }
                }
                Some(Reply::Garbage) => report.noise += 1,
            }
        }
    }
    (report, None)
}

/// The whole scan: every candidate port in parallel, stopping everywhere at
/// the first answer. `cancelled` is polled between units.
pub async fn scan_modbus(
    roots: &Roots,
    step: &Step,
    params: ModbusScanParams,
    cancelled: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<Value, RpcError> {
    let started = Instant::now();
    let ports = candidates(roots, &params.hint_ports);
    let to_scan: Vec<&Candidate> = ports.iter().filter(|c| c.skipped.is_none()).collect();
    if to_scan.is_empty() {
        return Ok(json!({
            "found": null,
            "stopped": "no_ports",
            "ports": ports,
            "scanned": [],
            "duration": 0.0,
        }));
    }
    let units = Arc::new(unit_order(&params.hint_unit_ids));
    let settings = Arc::new(settings_order(&params.hint_settings));
    let timeout = Duration::from_secs_f64(params.timeout);
    let stop = Arc::new(AtomicBool::new(false));
    step.set(format!(
        "Scanning {} port(s) for Modbus devices",
        to_scan.len()
    ));
    let mut tasks = tokio::task::JoinSet::new();
    for c in &to_scan {
        let (path, units, settings, stop, step) = (
            PathBuf::from(&c.port),
            units.clone(),
            settings.clone(),
            stop.clone(),
            step.clone(),
        );
        tasks.spawn_blocking(move || scan_port(&path, &settings, &units, timeout, &stop, &step));
    }
    let watcher = {
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                if cancelled() {
                    stop.store(true, Ordering::Relaxed);
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        })
    };
    let mut found: Option<Found> = None;
    let mut reports = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (report, hit) = joined.map_err(|e| RpcError::new("SCAN_FAILED", e.to_string()))?;
        reports.push(report);
        if found.is_none() {
            if let Some(hit) = hit {
                found = Some(hit);
                stop.store(true, Ordering::Relaxed);
            }
        }
    }
    let was_cancelled = !watcher.is_finished() || watcher.await.unwrap_or(false);
    stop.store(true, Ordering::Relaxed);
    reports.sort_by(|a, b| a.port.cmp(&b.port));
    let stopped = if found.is_some() {
        "found"
    } else if was_cancelled && cancelled_now(&stop) {
        "cancelled"
    } else {
        "complete"
    };
    Ok(json!({
        "found": found,
        "stopped": stopped,
        "order": "hinted ids first, then 1, 247, 2, 246 … (all 247)",
        "timeout": params.timeout,
        "ports": ports,
        "scanned": reports,
        "duration": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    }))
}

fn cancelled_now(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_modbus_reference_frame() {
        // 01 03 00 00 00 01 -> CRC 84 0A (sent 0x84, 0x0A).
        assert_eq!(request(1), [0x01, 0x03, 0x00, 0x00, 0x00, 0x01, 0x84, 0x0A]);
    }

    #[test]
    fn order_covers_every_unit_once_from_both_ends() {
        let order = unit_order(&[]);
        assert_eq!(&order[..6], &[1, 247, 2, 246, 3, 245]);
        assert_eq!(order.len(), 247);
        assert_eq!(*order.last().unwrap(), 124);
        let unique: HashSet<_> = order.iter().collect();
        assert_eq!(unique.len(), 247);
    }

    #[test]
    fn hints_move_to_the_front_but_never_shrink_the_scan() {
        let order = unit_order(&[246, 0, 248, 246, 10]);
        assert_eq!(&order[..4], &[246, 10, 1, 247]);
        assert_eq!(order.len(), 247);
        let s = settings_order(&[Settings::new(19200, Parity::Even, 1)]);
        assert_eq!(s[0].label(), "19200 8E1");
        assert_eq!(s[1].label(), "9600 8N1");
        assert_eq!(s.len(), SETTINGS.len());
    }

    fn frame(bytes: &[u8]) -> Vec<u8> {
        let crc = crc16(bytes);
        let mut f = bytes.to_vec();
        f.extend([(crc & 0xFF) as u8, (crc >> 8) as u8]);
        f
    }

    #[test]
    fn classifies_data_exceptions_other_units_and_noise() {
        assert_eq!(
            classify(246, &frame(&[246, 0x03, 2, 0, 7])),
            Some(Reply::Data)
        );
        assert_eq!(
            classify(246, &frame(&[246, 0x83, 2])),
            Some(Reply::Exception(2))
        );
        assert_eq!(
            classify(5, &frame(&[9, 0x83, 2])),
            Some(Reply::OtherUnit(9))
        );
        assert_eq!(classify(5, &[0xFF, 0x00, 0x12]), Some(Reply::Garbage));
        let mut bad = frame(&[246, 0x03, 2, 0, 7]);
        bad[3] ^= 1;
        assert_eq!(classify(246, &bad), Some(Reply::Garbage));
        assert_eq!(classify(246, &[]), None);
    }

    #[test]
    fn params_accept_only_hints_and_timeout() {
        let ok = ModbusScanParams::parse(&json!({
            "hint_unit_ids": [246],
            "hint_ports": ["/dev/ttyAMA0"],
            "hint_settings": [{"baud": 9600, "parity": "none"}],
        }))
        .unwrap();
        assert_eq!(ok.hint_unit_ids, vec![246]);
        assert_eq!(ok.timeout, DEFAULT_TIMEOUT);
        assert!(ModbusScanParams::parse(&json!({"unit_ids": [1, 2]})).is_err());
        assert!(ModbusScanParams::parse(&json!({"hint_unit_ids": [300]})).is_err());
        assert!(ModbusScanParams::parse(&json!({"timeout": 5})).is_err());
    }
}
