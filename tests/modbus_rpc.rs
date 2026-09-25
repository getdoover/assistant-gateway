//! The Modbus methods through the real `RpcManager`, against a fake `mbpoll`
//! (put first on the tools' PATH with `Gateway::with_tool_env`) that speaks
//! the real one's output format -- see the fixtures in `src/modbus.rs`.
//!
//! The fake's TCP "devices": host 10.0.0.5 has unit 1 and 4 answering, unit
//! 2 silent (timeout) and unit 3 answering with an exception; 10.0.0.9
//! refuses connections. Registers read as `register * 100 + unit` (register
//! 9 as 65535), coils as `register` even, until written; writes land in
//! `$FAKE_DIR` and register 100 and up refuse them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use assistant_gateway::app::Gateway;
use assistant_gateway::config::AssistantGatewayConfig;
use assistant_gateway::tags::AssistantGatewayTags;
use doover::rpc::RpcManager;
use doover::tags::TagsCollection;
use doover::testing::MockBackend;
use doover::{ChannelBackend, Event};
use serde_json::{json, Value};

const CHANNEL: &str = "dv-assistant-gateway";

const MBPOLL: &str = r#"{ printf 'mbpoll'; printf '|%s' "$@"; echo; } >> "$FAKE_DIR/log"
unit=1 reg=1 count=1 kind=4 target= value=
while [ $# -gt 0 ]; do
  case $1 in
    -a) unit=$2; shift 2 ;;
    -r) reg=$2; shift 2 ;;
    -c) count=$2; shift 2 ;;
    -t) kind=$2; shift 2 ;;
    -m|-b|-P|-s|-d|-o|-p) shift 2 ;;
    -0|-1|-q) shift ;;
    *) if [ -z "$target" ]; then target=$1; else value=$1; fi; shift ;;
  esac
done
sleep 0.05
case $target in
  10.0.0.9) echo 'mbpoll: Connection failed: Connection refused.' >&2; exit 1 ;;
esac
if [ -n "$value" ]; then
  if [ "$reg" -ge 100 ]; then
    echo; echo 'Write output (holding) register failed: Illegal data address' >&2; exit 1
  fi
  echo "$value" > "$FAKE_DIR/reg_${kind}_$reg"
  echo 'Written 1 references.'; echo; exit 0
fi
echo "-- Polling slave $unit..."
case $unit in
  2) echo; echo 'Read output (holding) register failed: Operation timed out' >&2; exit 1 ;;
  3) echo; echo 'Read output (holding) register failed: Illegal data address' >&2; exit 1 ;;
esac
i=0
while [ $i -lt $count ]; do
  r=$((reg + i)); f="$FAKE_DIR/reg_${kind}_$r"
  if [ -e "$f" ]; then v=$(cat "$f")
  elif [ "$kind" = 0 ] || [ "$kind" = 1 ]; then v=$(( (r + 1) % 2 ))
  elif [ $r = 9 ]; then v='65535 (-1)'
  else v=$((r * 100 + unit)); fi
  printf '[%d]: \t%s\n' "$r" "$v"
  i=$((i + 1))
done
echo"#;

fn scripts() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ag-mb-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mbpoll");
        std::fs::write(&path, format!("#!/bin/sh\n{MBPOLL}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    })
}

struct Harness {
    backend: Arc<MockBackend>,
    rpc: Arc<RpcManager>,
    dir: PathBuf,
}

fn harness_with_path(path: Option<String>) -> Harness {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ag-mb-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = path.unwrap_or_else(|| {
        format!(
            "{}:{}",
            scripts().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    });
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
        .with_tool_env(vec![
            ("PATH".into(), path),
            ("FAKE_DIR".into(), dir.display().to_string()),
        ]),
    )
    .register(&rpc, CHANNEL);
    Harness { backend, rpc, dir }
}

fn harness() -> Harness {
    harness_with_path(None)
}

impl Harness {
    async fn call(&self, method: &str, request: Value) -> Value {
        self.backend.message_updates.lock().unwrap().clear();
        self.rpc
            .handle_event(&Event {
                event_name: "MessageCreate".into(),
                channel: CHANNEL.into(),
                payload: json!({"id": 1, "data": {
                    "type": "rpc", "method": method, "request": request,
                    "status": {"code": "sent"}, "response": {}}}),
            })
            .await;
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

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

fn tcp(extra: Value) -> Value {
    let mut v = json!({"transport": "tcp", "host": "10.0.0.5"});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}

#[tokio::test]
async fn probe_reports_each_unit() {
    let h = harness();
    let outcome = h
        .call(
            "probe_modbus",
            tcp(json!({"unit_ids": [1, 2, 3, 4], "count": 2})),
        )
        .await;
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    assert_eq!(
        outcome["response"],
        json!({
            "results": [
                {"unit_id": 1, "ok": true, "values": [1, 101]},
                {"unit_id": 2, "ok": false,
                 "error": "Read output (holding) register failed: Operation timed out"},
                {"unit_id": 3, "ok": false,
                 "error": "Read output (holding) register failed: Illegal data address",
                 "exception": true},
                {"unit_id": 4, "ok": true, "values": [4, 104]},
            ],
            "attempted": 4,
            "answered": 3,
            "errors": [],
        })
    );
    assert_eq!(
        h.log()[0],
        "mbpoll|-m|tcp|-p|502|-a|1|-0|-r|0|-c|2|-t|4|-o|1|-1|-q|10.0.0.5"
    );
    let steps = h.steps();
    assert!(
        steps
            .iter()
            .any(|s| s.starts_with("Probing unit ") && s.ends_with("/4")),
        "{steps:?}"
    );
}

#[tokio::test]
async fn probe_stops_when_the_link_is_down() {
    let h = harness();
    let outcome = h
        .call(
            "probe_modbus",
            json!({"transport": "tcp", "host": "10.0.0.9", "unit_ids": [1, 2, 3]}),
        )
        .await;
    assert_eq!(
        outcome["response"],
        json!({
            "results": [{"unit_id": 1, "ok": false,
                         "error": "Connection failed: Connection refused"}],
            "attempted": 1,
            "answered": 0,
            "errors": ["10.0.0.9: Connection failed: Connection refused"],
        }),
        "{outcome}"
    );
}

#[tokio::test]
async fn read_values_and_bits() {
    let h = harness();
    let outcome = h
        .call(
            "read_modbus",
            tcp(
                json!({"unit_id": 1, "register": 8, "count": 2, "kind": "input", "tcp_port": 5020}),
            ),
        )
        .await;
    assert_eq!(
        outcome["response"],
        json!({"unit_id": 1, "register": 8, "kind": "input", "values": [801, 65535]}),
        "{outcome}"
    );
    assert_eq!(
        h.log()[0],
        "mbpoll|-m|tcp|-p|5020|-a|1|-0|-r|8|-c|2|-t|3|-o|1|-1|-q|10.0.0.5"
    );
    let outcome = h
        .call(
            "read_modbus",
            tcp(json!({"unit_id": 1, "register": 0, "count": 3, "kind": "coil"})),
        )
        .await;
    assert_eq!(
        outcome["response"]["values"],
        json!([true, false, true]),
        "{outcome}"
    );
}

#[tokio::test]
async fn read_failure_is_modbus_failed() {
    let h = harness();
    let outcome = h.call("read_modbus", tcp(json!({"unit_id": 2}))).await;
    assert_eq!(
        outcome["status"]["message"]["code"], "MODBUS_FAILED",
        "{outcome}"
    );
    assert_eq!(
        outcome["status"]["message"]["message"],
        "Read output (holding) register failed: Operation timed out"
    );
}

#[tokio::test]
async fn write_then_read_back() {
    let h = harness();
    let outcome = h
        .call(
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 5, "value": 1234})),
        )
        .await;
    assert_eq!(
        outcome["response"],
        json!({"ok": true, "unit_id": 1, "register": 5, "kind": "holding",
               "value": 1234, "readback": 1234}),
        "{outcome}"
    );
    assert_eq!(
        h.log(),
        [
            "mbpoll|-m|tcp|-p|502|-a|1|-0|-r|5|-t|4|-o|1|-1|-q|10.0.0.5|1234",
            "mbpoll|-m|tcp|-p|502|-a|1|-0|-r|5|-c|1|-t|4|-o|1|-1|-q|10.0.0.5",
        ]
    );
    assert!(
        h.steps()
            .iter()
            .any(|s| s == "Writing register 40006 on unit 1"),
        "{:?}",
        h.steps()
    );

    let outcome = h
        .call(
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 1, "kind": "coil", "value": true})),
        )
        .await;
    assert_eq!(
        outcome["response"],
        json!({"ok": true, "unit_id": 1, "register": 1, "kind": "coil",
               "value": true, "readback": true}),
        "{outcome}"
    );
}

#[tokio::test]
async fn write_refused_by_the_device() {
    let h = harness();
    let outcome = h
        .call(
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 100, "value": 1})),
        )
        .await;
    assert_eq!(
        outcome["status"]["message"]["code"], "MODBUS_FAILED",
        "{outcome}"
    );
    assert_eq!(
        outcome["status"]["message"]["message"],
        "Write output (holding) register failed: Illegal data address"
    );
    // No read-back of a write that didn't happen.
    assert_eq!(h.log().len(), 1);
}

#[tokio::test]
async fn missing_mbpoll() {
    let h = harness_with_path(Some("/nonexistent".into()));
    let outcome = h.call("read_modbus", tcp(json!({"unit_id": 1}))).await;
    assert_eq!(
        outcome["status"]["message"]["code"], "MODBUS_FAILED",
        "{outcome}"
    );
    assert_eq!(
        outcome["status"]["message"]["message"],
        "mbpoll tcp: not found on container"
    );
}

#[tokio::test]
async fn serial_port_must_exist() {
    let h = harness();
    let outcome = h
        .call(
            "probe_modbus",
            json!({"transport": "rtu", "port": "/dev/ttyNOPE0"}),
        )
        .await;
    assert_eq!(
        outcome["status"]["message"]["code"], "PORT_NOT_FOUND",
        "{outcome}"
    );
    assert!(h
        .updates()
        .iter()
        .all(|u| u["status"]["code"] != "acknowledged"));
    assert!(h.log().is_empty());
}

/// RTU on whatever serial-looking device this machine has (`/dev/ttys000`
/// on macOS, `/dev/ttyS0` on most Linux); skipped when there's none.
#[tokio::test]
async fn rtu_link_settings_are_explicit() {
    let Some(port) = std::fs::read_dir("/dev").ok().and_then(|d| {
        d.filter_map(|e| e.ok())
            .map(|e| format!("/dev/{}", e.file_name().to_string_lossy()))
            .filter(|p| {
                p.strip_prefix("/dev/tty")
                    .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric()))
            })
            .min()
    }) else {
        eprintln!("no /dev/tty* here; skipping");
        return;
    };
    let h = harness();
    let outcome = h
        .call(
            "read_modbus",
            json!({"transport": "rtu", "port": port, "baud": 19200, "parity": "E",
                   "stop_bits": 1, "unit_id": 4, "register": 2, "timeout": 0.5}),
        )
        .await;
    assert_eq!(outcome["response"]["values"], json!([204]), "{outcome}");
    assert_eq!(
        h.log()[0],
        format!(
            "mbpoll|-m|rtu|-b|19200|-P|even|-s|1|-d|8|-a|4|-0|-r|2|-c|1|-t|4|-o|0.5|-1|-q|{port}"
        )
    );
}

#[tokio::test]
async fn bad_params_are_rejected_before_acknowledging() {
    let h = harness();
    for (method, request) in [
        ("probe_modbus", json!({})),
        (
            "probe_modbus",
            json!({"transport": "rtu", "port": "/dev/sda"}),
        ),
        (
            "probe_modbus",
            tcp(json!({"unit_ids": (1..=33).collect::<Vec<_>>()})),
        ),
        ("probe_modbus", tcp(json!({"count": 17}))),
        (
            "probe_modbus",
            json!({"transport": "tcp", "host": "10.0.0.5; reboot"}),
        ),
        ("read_modbus", tcp(json!({}))),
        ("read_modbus", tcp(json!({"unit_id": 1, "count": 126}))),
        ("read_modbus", tcp(json!({"unit_id": 1, "kind": "string"}))),
        ("write_modbus", tcp(json!({"unit_id": 1, "register": 1}))),
        (
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 1, "value": [1, 2]})),
        ),
        (
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 1, "value": 1, "count": 2})),
        ),
        (
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 1, "value": 70000})),
        ),
        (
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 1, "value": 1, "kind": "input"})),
        ),
        (
            "write_modbus",
            tcp(json!({"unit_id": 1, "register": 1, "value": 1, "kind": "discrete"})),
        ),
    ] {
        let outcome = h.call(method, request.clone()).await;
        assert_eq!(
            outcome["status"]["message"]["code"], "INVALID_PARAMS",
            "{method} {request}: {outcome}"
        );
        assert!(h
            .updates()
            .iter()
            .all(|u| u["status"]["code"] != "acknowledged"));
    }
    assert!(h.log().is_empty());
}
