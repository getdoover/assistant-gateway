//! `exec` driven through the real `RpcManager` over an in-memory backend.

use std::sync::Arc;
use std::time::Duration;

use assistant_gateway::app::Gateway;
use assistant_gateway::config::AssistantGatewayConfig;
use assistant_gateway::tags::AssistantGatewayTags;
use doover::rpc::RpcManager;
use doover::tags::TagsCollection;
use doover::testing::MockBackend;
use doover::{ChannelBackend, Event};
use serde_json::{json, Value};

const CHANNEL: &str = "dv-assistant-gateway";

fn config() -> AssistantGatewayConfig {
    AssistantGatewayConfig {
        rpc_channel: CHANNEL.into(),
        run_on_host: false,
        default_timeout: 5.0,
        max_timeout: 10.0,
        stream_interval: 0.1,
        max_output_bytes: 1024,
    }
}

struct Harness {
    backend: Arc<MockBackend>,
    rpc: Arc<RpcManager>,
}

fn harness(gateway: Gateway) -> Harness {
    let backend = Arc::new(MockBackend::new());
    let rpc = Arc::new(RpcManager::new(
        backend.clone() as Arc<dyn ChannelBackend>,
        None,
    ));
    Arc::new(gateway).register(&rpc, CHANNEL);
    Harness { backend, rpc }
}

fn gateway(config: AssistantGatewayConfig) -> Gateway {
    Gateway::new(config, AssistantGatewayTags::detached())
}

fn event(name: &str, channel: &str, id: u64, data: Value) -> Event {
    Event {
        event_name: name.into(),
        channel: channel.into(),
        payload: json!({"id": id, "data": data}),
    }
}

fn request(channel: &str, id: u64, request: Value) -> Event {
    event(
        "MessageCreate",
        channel,
        id,
        json!({"type": "rpc", "method": "exec", "request": request, "status": {"code": "sent"}, "response": {}}),
    )
}

impl Harness {
    async fn call(&self, id: u64, request_body: Value) {
        self.rpc
            .handle_event(&request(CHANNEL, id, request_body))
            .await;
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

    fn statuses(&self, code: &str) -> Vec<Value> {
        self.updates()
            .into_iter()
            .filter(|u| u["status"]["code"] == code)
            .collect()
    }

    /// The single terminal update.
    fn outcome(&self) -> Value {
        let terminal: Vec<_> = self
            .updates()
            .into_iter()
            .filter(|u| matches!(u["status"]["code"].as_str(), Some("success" | "error")))
            .collect();
        assert_eq!(
            terminal.len(),
            1,
            "expected one terminal update, got {terminal:?}"
        );
        terminal.into_iter().next().unwrap()
    }
}

#[tokio::test]
async fn exec_returns_result() {
    let h = harness(gateway(config()));
    h.call(1, json!({"command": "echo hi"})).await;
    let outcome = h.outcome();
    assert_eq!(outcome["status"]["code"], "success");
    let response = &outcome["response"];
    assert_eq!(response["exit_code"], 0);
    assert_eq!(response["stdout"], "hi\n");
    let keys: Vec<_> = response
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "exit_code",
            "stdout",
            "stderr",
            "duration",
            "timed_out",
            "cancelled",
            "stdout_truncated",
            "stderr_truncated"
        ]
    );
    assert_eq!(h.statuses("acknowledged").len(), 1);
}

#[tokio::test]
async fn exec_streams_cumulative_output() {
    let h = harness(gateway(config()));
    h.call(
        1,
        json!({"command": "echo a; sleep 0.4; echo b; sleep 0.4"}),
    )
    .await;
    let outputs: Vec<_> = h
        .statuses("pending")
        .iter()
        .map(|u| u["status"]["message"]["stdout"].clone())
        .collect();
    assert!(outputs.contains(&json!("a\n")), "{outputs:?}");
    assert!(outputs.contains(&json!("a\nb\n")), "{outputs:?}");
    let first = &h.statuses("pending")[0]["status"]["message"];
    assert!(first["text"].as_str().unwrap().starts_with("Running ("));
    assert_eq!(h.outcome()["status"]["code"], "success");
}

#[tokio::test]
async fn exec_sends_heartbeats_only_when_streaming_disabled() {
    let g = gateway(AssistantGatewayConfig {
        stream_interval: 0.0,
        ..config()
    })
    .with_heartbeat(Duration::from_millis(200));
    let h = harness(g);
    h.call(1, json!({"command": "echo a; sleep 0.5"})).await;
    let pending = h.statuses("pending");
    assert!(!pending.is_empty());
    assert!(pending
        .iter()
        .all(|u| u["status"]["message"].get("stdout").is_none()));
}

#[tokio::test]
async fn exec_rejects_bad_params() {
    let bad = [
        json!("ls"),
        json!({}),
        json!({"command": ""}),
        json!({"command": "   "}),
        json!({"command": "ls", "env": {"A": 1}}),
        json!({"command": "ls", "env": ["A"]}),
        json!({"command": "ls", "timeout": -1}),
        json!({"command": "ls", "timeout": "soon"}),
        json!({"command": "ls", "stdin": 5}),
        json!({"command": "ls", "cwd": 5}),
    ];
    for (i, payload) in bad.into_iter().enumerate() {
        let h = harness(gateway(config()));
        h.call(i as u64 + 1, payload.clone()).await;
        let outcome = h.outcome();
        assert_eq!(
            outcome["status"]["message"]["code"], "INVALID_PARAMS",
            "{payload}: {outcome}"
        );
        // Rejected before it was acknowledged or run.
        assert!(h.statuses("acknowledged").is_empty(), "{payload}");
    }
}

#[tokio::test]
async fn exec_is_only_served_on_its_channel() {
    let h = harness(gateway(config()));
    h.rpc
        .handle_event(&request("dv-rpc", 1, json!({"command": "true"})))
        .await;
    assert!(h.updates().is_empty());
}

#[tokio::test]
async fn cancel_kills_the_command_and_leaves_the_cancellation_standing() {
    let h = harness(gateway(config()));
    let rpc = h.rpc.clone();
    let running = tokio::spawn(async move {
        rpc.handle_event(&request(CHANNEL, 7, json!({"command": "sleep 30"})))
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cancel =
        json!({"code": "error", "message": {"info": "Command cancelled", "cancelled_at": 1}});
    h.rpc
        .handle_event(&event(
            "MessageUpdate",
            CHANNEL,
            7,
            json!({"type": "rpc", "method": "exec", "status": cancel}),
        ))
        .await;
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("handler returned")
        .unwrap();

    // The canceller wrote the terminal status; we must not write over it.
    assert!(
        h.statuses("success").is_empty() && h.statuses("error").is_empty(),
        "{:?}",
        h.updates()
    );
}

#[test]
fn timeout_defaults_and_caps() {
    let g = gateway(config());
    assert_eq!(g.timeout(None).unwrap(), Duration::from_secs(5));
    assert_eq!(
        g.timeout(Some(&Value::Null)).unwrap(),
        Duration::from_secs(5)
    );
    assert_eq!(
        g.timeout(Some(&json!(9999))).unwrap(),
        Duration::from_secs(10)
    );
    assert_eq!(
        g.timeout(Some(&json!(2.5))).unwrap(),
        Duration::from_millis(2500)
    );
    assert_eq!(
        g.timeout(Some(&json!("3"))).unwrap(),
        Duration::from_secs(3)
    );
    assert!(g.timeout(Some(&json!(0))).is_err());
    assert!(g.timeout(Some(&json!(true))).is_err());
}
