//! scan_modbus against a fake Modbus device on a pseudo-terminal: the real
//! termios/RTU code, end to end.

use std::fs;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use assistant_gateway::diag::Step;
use assistant_gateway::scan::{self, crc16, ModbusScanParams, Roots};
use serde_json::json;

/// A pty pair: (master, slave, slave path). The slave stays open for the
/// test: with no slave open, reads on the master fail and the pty goes away.
fn pty() -> (OwnedFd, OwnedFd, String) {
    let (mut master, mut slave) = (0, 0);
    let mut name = [0 as libc::c_char; 128];
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            name.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        (
            OwnedFd::from_raw_fd(master),
            OwnedFd::from_raw_fd(slave),
            path,
        )
    }
}

/// A device at `unit` answering with an exception; counts every request.
fn fake_device(master: OwnedFd, unit: u8, requests: Arc<AtomicUsize>) {
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut file = fs::File::from(master);
        let mut frame = [0u8; 8];
        loop {
            if file.read_exact(&mut frame).is_err() {
                return;
            }
            requests.fetch_add(1, Ordering::SeqCst);
            if frame[0] == unit {
                let mut reply = vec![unit, 0x83, 0x02];
                let crc = crc16(&reply);
                reply.extend([(crc & 0xFF) as u8, (crc >> 8) as u8]);
                let _ = file.write_all(&reply);
            }
        }
    });
}

/// A /dev and /sys/class/tty with `ttyAMA0` (the pty), a modem on
/// `ttyUSB0`, and the system console on `ttyAMA1`.
fn roots(dir: &Path, pty_path: &str) -> Roots {
    let (dev, sys, proc) = (dir.join("dev"), dir.join("sys"), dir.join("proc"));
    for d in [&dev, &sys, &proc] {
        fs::create_dir_all(d).unwrap();
    }
    symlink(pty_path, dev.join("ttyAMA0")).unwrap();
    fs::create_dir_all(sys.join("ttyAMA0")).unwrap();
    fs::write(dev.join("ttyUSB0"), "").unwrap();
    let usb = dir.join("usbdev");
    fs::create_dir_all(usb.join("iface")).unwrap();
    fs::write(usb.join("manufacturer"), "SimTech, Incorporated\n").unwrap();
    fs::write(usb.join("product"), "SimTech, Incorporated\n").unwrap();
    fs::create_dir_all(sys.join("ttyUSB0")).unwrap();
    symlink(usb.join("iface"), sys.join("ttyUSB0").join("device")).unwrap();
    fs::write(dev.join("ttyAMA1"), "").unwrap();
    fs::create_dir_all(sys.join("ttyAMA1")).unwrap();
    fs::write(
        proc.join("cmdline"),
        "console=ttyAMA1,115200 root=/dev/mmcblk0p2",
    )
    .unwrap();
    Roots {
        dev,
        sys_tty: sys,
        proc,
    }
}

async fn run(hints: serde_json::Value) -> (serde_json::Value, usize) {
    let dir = std::env::temp_dir()
        .join(format!("scan-{}", std::process::id()))
        .join(format!("{}", hints.to_string().len()));
    let _ = fs::remove_dir_all(&dir);
    let (master, _slave, path) = pty();
    let requests = Arc::new(AtomicUsize::new(0));
    fake_device(master, 246, requests.clone());
    let roots = roots(&dir, &path);
    let mut params = json!({"timeout": 0.05});
    params
        .as_object_mut()
        .unwrap()
        .extend(hints.as_object().unwrap().clone());
    let params = ModbusScanParams::parse(&params).unwrap();
    let result = scan::scan_modbus(&roots, &Step::new("Starting"), params, || false)
        .await
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    (result, requests.load(Ordering::SeqCst))
}

#[tokio::test(flavor = "multi_thread")]
async fn finds_the_device_scanning_from_both_ends_and_stops() {
    let (result, requests) = run(json!({})).await;
    let found = &result["found"];
    assert_eq!(result["stopped"], "found", "{result}");
    assert_eq!(found["unit_id"], 246);
    assert_eq!(found["reply"], "exception");
    assert_eq!(found["baud"], 9600);
    assert_eq!(found["parity"], "none");
    // 1, 247, 2, then 246: it stops at the first answer.
    assert_eq!(requests, 4);
    // The modem and the console are reported, never scanned.
    let ports = result["ports"].as_array().unwrap();
    let skipped: Vec<_> = ports
        .iter()
        .filter_map(|p| {
            p["skipped"]
                .as_str()
                .map(|w| (p["port"].as_str().unwrap(), w))
        })
        .collect();
    assert!(
        skipped
            .iter()
            .any(|(p, w)| p.ends_with("ttyUSB0") && *w == "cellular modem"),
        "{ports:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|(p, w)| p.ends_with("ttyAMA1") && *w == "system console"),
        "{ports:?}"
    );
    assert_eq!(result["scanned"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hinted_id_is_asked_first() {
    let (result, requests) = run(json!({"hint_unit_ids": [246]})).await;
    assert_eq!(result["found"]["unit_id"], 246);
    assert_eq!(requests, 1);
}
