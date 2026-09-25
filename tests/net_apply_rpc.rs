//! `net_apply` through the real `RpcManager`, against fake `busctl` /
//! `dbus-send` / `nmcli` / `curl` (and the tools `net_status` reads) in a
//! directory put first on the tools' PATH with `Gateway::with_tool_env`.
//! Each test gets its own `$FAKE_DIR` (same env hook): the fakes append their
//! argv to `$FAKE_DIR/log` (`|`-separated, so argument boundaries show) and
//! read flag files there for how to behave.
//!
//! Waits are shrunk with `NetTiming`: one "second" is 50 ms, so the 30 s
//! minimum rollback window passes in 1.5 s.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use assistant_gateway::app::Gateway;
use assistant_gateway::config::AssistantGatewayConfig;
use assistant_gateway::netapply::NetTiming;
use assistant_gateway::tags::AssistantGatewayTags;
use doover::rpc::RpcManager;
use doover::tags::TagsCollection;
use doover::testing::MockBackend;
use doover::{ChannelBackend, Event};
use serde_json::{json, Value};

const CHANNEL: &str = "dv-assistant-gateway";
const SECOND: Duration = Duration::from_millis(50);

const LOG: &str = r#"{ printf '%s' "${0##*/}"; printf '|%s' "$@"; echo; } >> "$FAKE_DIR/log""#;

const FAKES: &[(&str, &str)] = &[
    (
        "busctl",
        r#"[ -e "$FAKE_DIR/no_busctl" ] && exit 127
case "$*" in
  *CheckpointCreate*)
    [ -e "$FAKE_DIR/checkpoint_refused" ] && { echo 'Call failed: Access denied' >&2; exit 1; }
    echo 'o "/org/freedesktop/NetworkManager/Checkpoint/4"' ;;
  *CheckpointRollback*) echo 'a{su} 1 "/org/freedesktop/NetworkManager/Devices/2" 0' ;;
  *CheckpointDestroy*) ;;
  *get-property*Checkpoints*)
    if [ -e "$FAKE_DIR/checkpoint_stuck" ]; then
      echo 'ao 1 "/org/freedesktop/NetworkManager/Checkpoint/4"'
    else
      echo 'ao 0'
    fi ;;
  *) exit 1 ;;
esac"#,
    ),
    (
        "dbus-send",
        r#"[ -e "$FAKE_DIR/dbus_send" ] || exit 127
case "$*" in
  *CheckpointCreate*) printf 'method return time=1727241600.1 sender=:1.7 -> destination=:1.9 serial=5 reply_serial=2\n   object path "/org/freedesktop/NetworkManager/Checkpoint/9"\n' ;;
  *) printf 'method return time=1727241600.2 sender=:1.7 -> destination=:1.9 serial=6 reply_serial=2\n' ;;
esac"#,
    ),
    (
        "nmcli",
        r#"case "$*" in
  *NAME,DEVICE*) printf 'Wired connection 1:eth0\n' ;;
  *NAME,TYPE*) printf 'Wired connection 1:802-3-ethernet\n' ;;
  *DEVICE,TYPE,STATE,CONNECTION*) printf 'eth0:ethernet:connected:Wired connection 1\n' ;;
  *NAME,UUID,TYPE,DEVICE*) printf 'Wired connection 1:1111-2222:802-3-ethernet:eth0\n' ;;
  *"con mod"*) sleep 0.15 ;;
  *"con up"*|*"wifi connect"*|*"dev connect"*|*"dev disconnect"*|*"con down"*|*"con add"*)
    if [ -e "$FAKE_DIR/fail_apply" ]; then
      echo 'Error: Connection activation failed: IP configuration could not be reserved' >&2
      exit 4
    fi
    touch "$FAKE_DIR/applied" ;;
  *) exit 2 ;;
esac"#,
    ),
    (
        "curl",
        r#"if [ -e "$FAKE_DIR/applied" ] && [ -e "$FAKE_DIR/unreachable_after_apply" ]; then
  echo 'curl: (6) Could not resolve host: api.doover.com' >&2; exit 6
fi
printf 200"#,
    ),
    ("ip", "echo '[]'"),
    ("mmcli", "exit 127"),
    ("ping", "exit 0"),
    ("getent", "echo '203.0.113.9 api.doover.com'"),
];

struct Fakes {
    /// This test's state: flag files and the log.
    dir: PathBuf,
}

/// The fake scripts, written once per test binary and run once each: the
/// first exec of a freshly written script can be slow (macOS scans new
/// executables), which would eat the shrunk rollback window.
fn scripts() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ag-net-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = dir.join("warmup");
        std::fs::create_dir_all(&scratch).unwrap();
        for (name, body) in FAKES {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{LOG}\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::process::Command::new(&path)
                .env("FAKE_DIR", &scratch)
                .output();
        }
        dir
    })
}

impl Fakes {
    fn new(flags: &[&str]) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ag-net-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for flag in flags {
            std::fs::write(dir.join(flag), "").unwrap();
        }
        Self { dir }
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "PATH".into(),
                format!(
                    "{}:{}",
                    scripts().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
            ("FAKE_DIR".into(), self.dir.display().to_string()),
        ]
    }

    /// Every fake call, in order, as `tool|arg|arg...`.
    fn log(&self) -> Vec<String> {
        read_lines(&self.dir.join("log"))
    }

    fn calls(&self, needle: &str) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| l.contains(needle))
            .collect()
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

struct Harness {
    backend: Arc<MockBackend>,
    rpc: Arc<RpcManager>,
    fakes: Fakes,
}

fn harness(flags: &[&str]) -> Harness {
    let fakes = Fakes::new(flags);
    let backend = Arc::new(MockBackend::new());
    let rpc = Arc::new(RpcManager::new(
        backend.clone() as Arc<dyn ChannelBackend>,
        None,
    ));
    Arc::new(
        Gateway::new(
            AssistantGatewayConfig {
                rpc_channel: CHANNEL.into(),
                run_on_host: false,
                default_timeout: 5.0,
                max_timeout: 10.0,
                stream_interval: 0.02,
                max_output_bytes: 1024,
            },
            AssistantGatewayTags::detached(),
        )
        .with_net_timing(NetTiming { second: SECOND })
        .with_tool_env(fakes.env()),
    )
    .register(&rpc, CHANNEL);
    Harness {
        backend,
        rpc,
        fakes,
    }
}

fn event(id: u64, request: Value) -> Event {
    Event {
        event_name: "MessageCreate".into(),
        channel: CHANNEL.into(),
        payload: json!({"id": id, "data": {
            "type": "rpc", "method": "net_apply", "request": request,
            "status": {"code": "sent"}, "response": {}}}),
    }
}

impl Harness {
    async fn call(&self, request: Value) -> Value {
        self.rpc.handle_event(&event(1, request)).await;
        let terminal: Vec<_> = self
            .updates()
            .into_iter()
            .filter(|u| matches!(u["status"]["code"].as_str(), Some("success" | "error")))
            .collect();
        assert_eq!(terminal.len(), 1, "{:?}", self.updates());
        terminal.into_iter().next().unwrap()
    }

    fn updates(&self) -> Vec<Value> {
        self.backend
            .message_updates
            .lock()
            .unwrap()
            .iter()
            .map(|u| u.data.clone())
            .collect()
    }

    fn steps(&self) -> Vec<String> {
        self.updates()
            .iter()
            .filter(|u| u["status"]["code"] == "pending")
            .filter_map(|u| u["status"]["message"]["step"].as_str().map(String::from))
            .collect()
    }
}

const IPV4: fn() -> Value = || {
    json!({"op": "ipv4", "interface": "eth0", "method": "manual",
           "address": "192.168.1.50/24", "gateway": "192.168.1.1",
           "dns": ["1.1.1.1"], "rollback_after": 30})
};

#[tokio::test]
async fn applies_verifies_and_keeps_the_change() {
    let h = harness(&[]);
    let outcome = h.call(IPV4()).await;
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    let r = &outcome["response"];
    assert_eq!(r["applied"], true, "{r}");
    assert_eq!(r["verified"], true);
    assert_eq!(r["rolled_back"], false);
    assert_eq!(r["checkpoint"], true);
    assert_eq!(r["reason"], Value::Null);
    assert_eq!(r["status"]["checks"]["platform_https"], true);

    let busctl = h.fakes.calls("busctl|");
    assert_eq!(
        busctl,
        [
            "busctl|call|org.freedesktop.NetworkManager|/org/freedesktop/NetworkManager|org.freedesktop.NetworkManager|CheckpointCreate|aouu|0|30|2",
            "busctl|call|org.freedesktop.NetworkManager|/org/freedesktop/NetworkManager|org.freedesktop.NetworkManager|CheckpointDestroy|o|/org/freedesktop/NetworkManager/Checkpoint/4",
        ]
    );
    // The interface was resolved to its connection before the checkpoint,
    // and the change came after it.
    let log = h.fakes.log();
    let pos = |needle: &str| log.iter().position(|l| l.contains(needle)).unwrap();
    assert!(pos("NAME,DEVICE") < pos("CheckpointCreate"));
    assert!(pos("CheckpointCreate") < pos("con|mod"));
    assert!(pos("con|up") < pos("CheckpointDestroy"));
    assert_eq!(
        h.fakes.calls("con|mod"),
        ["nmcli|con|mod|Wired connection 1|ipv4.method|manual|ipv4.addresses|192.168.1.50/24|ipv4.gateway|192.168.1.1|ipv4.dns|1.1.1.1"]
    );
    assert_eq!(
        h.fakes.calls("con|up"),
        ["nmcli|-w|10|con|up|Wired connection 1"]
    );
    assert!(
        h.steps().iter().any(|s| s == "Applying ipv4 on eth0"),
        "{:?}",
        h.steps()
    );
}

#[tokio::test]
async fn unreachable_after_apply_waits_for_the_rollback() {
    let h = harness(&["unreachable_after_apply"]);
    let started = Instant::now();
    let outcome = h.call(IPV4()).await;
    let took = started.elapsed();
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    let r = &outcome["response"];
    assert_eq!(r["applied"], false, "{r}");
    assert_eq!(r["verified"], false);
    assert_eq!(r["rolled_back"], true);
    assert_eq!(r["checkpoint"], true);
    let reason = r["reason"].as_str().unwrap();
    assert!(
        reason.starts_with(
            "platform unreachable after apply (curl: exit 6: curl: (6) Could not resolve host"
        ),
        "{reason}"
    );
    assert!(r["status"].is_object());

    // Not destroyed, not rolled back by us: NetworkManager's timer does it,
    // and the checkpoint is confirmed gone afterwards.
    assert!(h.fakes.calls("CheckpointDestroy").is_empty());
    assert!(h.fakes.calls("CheckpointRollback").is_empty());
    let log = h.fakes.log();
    let pos = |needle: &str| log.iter().position(|l| l.contains(needle)).unwrap();
    assert!(pos("con|up") < pos("get-property|"));
    // Polled (every 3 "s", for 30 - 10 "s": how many fit depends on how
    // fast processes spawn here), then waited out the 30 + 2.
    let polls = h.fakes.calls("curl|").len();
    assert!(
        polls >= 3,
        "{polls} polls (two or more verify polls + net_status)"
    );
    assert!(took >= SECOND * 32, "{took:?}");
    let steps = h.steps();
    assert!(
        steps.iter().any(|s| s == "Verifying platform reachability"),
        "{steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|s| s == "Waiting for NetworkManager to roll back"),
        "{steps:?}"
    );
}

#[tokio::test]
async fn a_checkpoint_still_pending_after_the_deadline_is_rolled_back() {
    let h = harness(&["unreachable_after_apply", "checkpoint_stuck"]);
    let outcome = h.call(IPV4()).await;
    let r = &outcome["response"];
    assert_eq!(r["rolled_back"], true, "{outcome}");
    let reason = r["reason"].as_str().unwrap();
    assert!(
        reason.ends_with(
            "; the checkpoint was still pending at the deadline, so it was rolled back explicitly"
        ),
        "{reason}"
    );
    assert_eq!(h.fakes.calls("CheckpointRollback").len(), 1);
}

#[tokio::test]
async fn failed_apply_rolls_back_at_once() {
    let h = harness(&["fail_apply"]);
    let outcome = h.call(IPV4()).await;
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    let r = &outcome["response"];
    assert_eq!(r["applied"], false, "{r}");
    assert_eq!(r["verified"], false);
    assert_eq!(r["rolled_back"], true);
    assert_eq!(
        r["reason"],
        "nmcli con up: exit 4: Error: Connection activation failed: IP configuration could not be reserved"
    );
    assert_eq!(
        h.fakes.calls("CheckpointRollback"),
        ["busctl|call|org.freedesktop.NetworkManager|/org/freedesktop/NetworkManager|org.freedesktop.NetworkManager|CheckpointRollback|o|/org/freedesktop/NetworkManager/Checkpoint/4"]
    );
    assert!(h.fakes.calls("CheckpointDestroy").is_empty());
    // No waiting out the timer, and no verification (the one curl is
    // net_status's check).
    assert!(h
        .steps()
        .iter()
        .all(|s| s != "Waiting for NetworkManager to roll back"
            && s != "Verifying platform reachability"));
    assert_eq!(h.fakes.calls("curl|").len(), 1, "{:?}", h.fakes.log());
}

#[tokio::test]
async fn refuses_without_checkpoint_tooling() {
    let h = harness(&["no_busctl"]);
    for request in [
        IPV4(),
        json!({"op": "wifi_connect", "ssid": "Farm", "password": "hunter2hunter2"}),
        json!({"op": "dns", "interface": "eth0", "servers": ["1.1.1.1"]}),
        json!({"op": "lte", "apn": "telstra.internet"}),
        json!({"op": "interface", "name": "eth0", "state": "down"}),
        json!({"op": "connection", "name": "Wired connection 1", "state": "down"}),
    ] {
        h.backend.message_updates.lock().unwrap().clear();
        let outcome = h.call(request.clone()).await;
        assert_eq!(
            outcome["status"]["message"]["code"], "NO_CHECKPOINT",
            "{request}: {outcome}"
        );
    }
    let log = h.fakes.log();
    assert!(
        log.iter().all(|l| !l.contains("|mod|")
            && !l.contains("|up|")
            && !l.contains("|down|")
            && !l.contains("|connect|")
            && !l.contains("|disconnect|")
            && !l.contains("|add|")),
        "{log:?}"
    );
    // busctl, then dbus-send, both missing.
    assert!(log.iter().any(|l| l.starts_with("dbus-send|")), "{log:?}");
}

#[tokio::test]
async fn bringing_things_up_needs_no_checkpoint() {
    let h = harness(&["no_busctl"]);
    let outcome = h
        .call(json!({"op": "connection", "name": "Wired connection 1", "state": "up"}))
        .await;
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    let r = &outcome["response"];
    assert_eq!(r["applied"], true, "{r}");
    assert_eq!(r["verified"], true);
    assert_eq!(r["checkpoint"], false);
    assert_eq!(r["rolled_back"], false);
    assert_eq!(
        h.fakes.calls("con|up"),
        ["nmcli|-w|45|con|up|Wired connection 1"]
    );
}

#[tokio::test]
async fn falls_back_to_dbus_send() {
    let h = harness(&["no_busctl", "dbus_send"]);
    let outcome = h
        .call(json!({"op": "wifi_connect", "ssid": "Farm Office", "password": "hunter2hunter2"}))
        .await;
    assert_eq!(outcome["response"]["verified"], true, "{outcome}");
    assert_eq!(
        h.fakes.calls("dbus-send|"),
        [
            "dbus-send|--system|--print-reply|--dest=org.freedesktop.NetworkManager|/org/freedesktop/NetworkManager|org.freedesktop.NetworkManager.CheckpointCreate|array:objpath:|uint32:90|uint32:2",
            "dbus-send|--system|--print-reply|--dest=org.freedesktop.NetworkManager|/org/freedesktop/NetworkManager|org.freedesktop.NetworkManager.CheckpointDestroy|objpath:/org/freedesktop/NetworkManager/Checkpoint/9",
        ]
    );
    assert_eq!(
        h.fakes.calls("wifi"),
        ["nmcli|-w|45|dev|wifi|connect|Farm Office|password|hunter2hunter2"]
    );
}

#[tokio::test]
async fn refused_checkpoint_changes_nothing() {
    let h = harness(&["checkpoint_refused"]);
    let outcome = h.call(IPV4()).await;
    assert_eq!(
        outcome["status"]["message"]["code"], "CHECKPOINT_FAILED",
        "{outcome}"
    );
    assert!(h.fakes.calls("|mod|").is_empty());
}

#[tokio::test]
async fn lte_creates_a_profile_when_there_is_none() {
    let h = harness(&[]);
    let outcome = h
        .call(json!({"op": "lte", "apn": "telstra.internet", "user": "u", "password": "p"}))
        .await;
    assert_eq!(outcome["response"]["verified"], true, "{outcome}");
    assert_eq!(
        h.fakes.calls("con|add"),
        ["nmcli|con|add|type|gsm|ifname|*|con-name|lte|gsm.apn|telstra.internet|gsm.username|u|gsm.password|p"]
    );
    assert_eq!(h.fakes.calls("con|up"), ["nmcli|-w|45|con|up|lte"]);
}

#[tokio::test]
async fn unknown_interface_has_no_connection() {
    let h = harness(&[]);
    let outcome = h
        .call(json!({"op": "dns", "interface": "wlan0", "servers": ["1.1.1.1"]}))
        .await;
    assert_eq!(
        outcome["status"]["message"]["code"], "NO_CONNECTION",
        "{outcome}"
    );
    assert!(h.fakes.calls("Checkpoint").is_empty());
}

#[tokio::test]
async fn one_change_at_a_time() {
    let h = harness(&["unreachable_after_apply"]);
    let first = event(1, IPV4());
    tokio::join!(h.rpc.handle_event(&first), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.rpc.handle_event(&event(2, IPV4())).await;
    });
    let updates = h.backend.message_updates.lock().unwrap();
    let terminal = |id: u64| {
        updates
            .iter()
            .filter(|u| u.message_id == id)
            .map(|u| u.data.clone())
            .find(|d| matches!(d["status"]["code"].as_str(), Some("success" | "error")))
            .unwrap()
    };
    assert_eq!(terminal(1)["response"]["rolled_back"], true);
    assert_eq!(terminal(2)["status"]["message"]["code"], "BUSY");
}

#[tokio::test]
async fn bad_params_are_rejected_before_acknowledging() {
    let h = harness(&[]);
    for request in [
        json!({"op": "reboot"}),
        json!({"op": "ipv4", "interface": "eth0; reboot", "method": "auto"}),
        json!({"op": "ipv4", "interface": "eth0", "method": "manual", "address": "10.0.0.1/33"}),
        json!({"op": "wifi_connect", "ssid": "Farm", "password": "short"}),
        json!({"op": "dns", "interface": "eth0", "servers": ["x"]}),
    ] {
        h.backend.message_updates.lock().unwrap().clear();
        let outcome = h.call(request.clone()).await;
        assert_eq!(
            outcome["status"]["message"]["code"], "INVALID_PARAMS",
            "{request}: {outcome}"
        );
        assert!(h
            .updates()
            .iter()
            .all(|u| u["status"]["code"] != "acknowledged"));
    }
    assert!(h.fakes.log().is_empty());
}
