//! The typed diagnostic methods driven through the real `RpcManager`, against
//! fake `nmcli` / `ip` / `mmcli` / `ping` / `getent` / `curl` / `arp-scan` /
//! `nmap` scripts put first on PATH.
//!
//! `run_on_host: false`, so every tool runs "in the container" -- here, this
//! process's environment -- and nothing enters the host's namespaces.

use std::path::PathBuf;
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

const FAKES: &[(&str, &str)] = &[
    (
        "nmcli",
        r#"case "$*" in
  *DEVICE,TYPE,STATE,CONNECTION*) printf 'eth0:ethernet:connected:Wired connection 1\nwlan0:wifi:connected:Farm\\: Office\nlo:loopback:connected (externally):lo\n' ;;
  *NAME,UUID,TYPE,DEVICE*) printf 'Wired connection 1:1111-2222:802-3-ethernet:eth0\nFarm\\: Office:3333-4444:802-11-wireless:wlan0\n' ;;
  *ACTIVE,SSID,SIGNAL,DEVICE*) printf 'no:Neighbour:30:wlan0\nyes:Farm\\: Office:71:wlan0\n' ;;
  *SSID,SIGNAL,SECURITY,FREQ*) printf 'Farm\\: Office:71:WPA2:2437 MHz\n:88:WPA2:2412 MHz\nFarm\\: Office:40:WPA2:5180 MHz\nShed::--:2462 MHz\n' ;;
  *) exit 2 ;;
esac"#,
    ),
    (
        "ip",
        r#"case "$*" in
  "-j addr") echo '[{"ifname":"lo","operstate":"UNKNOWN","link_type":"loopback","address":"00:00:00:00:00:00","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]},{"ifname":"eth0","operstate":"UP","link_type":"ether","address":"dc:a6:32:01:02:03","addr_info":[{"family":"inet","local":"192.168.1.23","prefixlen":24},{"family":"inet6","local":"fe80::1","prefixlen":64}]},{"ifname":"wlan0","operstate":"UP","link_type":"ether","address":"dc:a6:32:0a:0b:0c","addr_info":[{"family":"inet","local":"10.1.0.5","prefixlen":16}]},{"ifname":"docker0","operstate":"DOWN","link_type":"ether","address":"02:42:00:00:00:01","addr_info":[]}]' ;;
  "-j route") echo '[{"dst":"default","gateway":"192.168.1.1","dev":"eth0","protocol":"dhcp","metric":100},{"dst":"default","gateway":"10.1.0.1","dev":"wlan0","protocol":"dhcp","metric":600},{"dst":"192.168.1.0/24","dev":"eth0","protocol":"kernel","metric":100}]' ;;
  *) exit 2 ;;
esac"#,
    ),
    // Behaves like a missing tool.
    ("mmcli", "echo 'mmcli: not found' >&2; exit 127"),
    (
        "ping",
        r#"for a; do t=$a; done; [ "$t" = 192.168.1.1 ] || { echo "no reply from $t"; exit 1; }"#,
    ),
    ("getent", "echo '203.0.113.9 api.doover.com'"),
    ("curl", "printf 200"),
    (
        "arp-scan",
        r#"sleep 0.4
echo "Interface: eth0, type: EN10MB, MAC: dc:a6:32:01:02:03, IPv4: 192.168.1.23"
printf '192.168.1.1\t00:1a:2b:3c:4d:5e\tNETGEAR\n192.168.1.40\t00:80:f4:11:22:33\tSchneider Electric\n'"#,
    ),
    (
        "nmap",
        r#"case "$*" in
  *-sn*) printf 'Host: 192.168.1.1 (router.lan)\tStatus: Up\nHost: 192.168.1.23 ()\tStatus: Up\n' ;;
  *-iL*) while read ip; do
           case $ip in 192.168.1.40) printf 'Host: 192.168.1.40 ()\tPorts: 502/open/tcp//mbap///\n' ;; esac
         done ;;
  *) exit 2 ;;
esac"#,
    ),
];

/// Write the fakes once and put them first on PATH (shared by every test in
/// this binary, so they all see the same fakes).
fn install_fakes() {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ag-fakes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in FAKES {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            "{}:{}",
            dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", path);
        dir
    });
}

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

fn harness() -> Harness {
    install_fakes();
    let backend = Arc::new(MockBackend::new());
    let rpc = Arc::new(RpcManager::new(
        backend.clone() as Arc<dyn ChannelBackend>,
        None,
    ));
    Arc::new(Gateway::new(config(), AssistantGatewayTags::detached())).register(&rpc, CHANNEL);
    Harness { backend, rpc }
}

impl Harness {
    async fn call(&self, method: &str, request: Value) -> Value {
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

    fn progress_texts(&self) -> Vec<String> {
        self.updates()
            .iter()
            .filter(|u| u["status"]["code"] == "pending")
            .filter_map(|u| u["status"]["message"]["text"].as_str().map(String::from))
            .collect()
    }
}

#[tokio::test]
async fn net_status_shape() {
    let h = harness();
    let outcome = h.call("net_status", json!({})).await;
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    let r = &outcome["response"];

    let keys: Vec<_> = r.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "interfaces",
            "routes",
            "dns",
            "connections",
            "modem",
            "checks",
            "errors"
        ]
    );
    // NetworkManager's devices first (loopback dropped), then ip-only ones.
    let names: Vec<_> = r["interfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["eth0", "wlan0", "docker0"]);
    assert_eq!(
        r["interfaces"][0],
        json!({"name": "eth0", "type": "ethernet", "state": "connected",
               "ip4": ["192.168.1.23/24"], "ip6": ["fe80::1/64"],
               "gateway": "192.168.1.1", "dns": [], "mac": "dc:a6:32:01:02:03",
               "connection": "Wired connection 1", "wifi": null})
    );
    assert_eq!(r["interfaces"][1]["connection"], "Farm: Office");
    assert_eq!(r["interfaces"][1]["gateway"], "10.1.0.1");
    assert_eq!(
        r["interfaces"][1]["wifi"],
        json!({"ssid": "Farm: Office", "signal": 71})
    );
    assert_eq!(r["interfaces"][2]["type"], "ether");
    assert_eq!(r["interfaces"][2]["state"], "down");
    assert_eq!(r["routes"].as_array().unwrap().len(), 3);
    assert!(r["dns"].is_array());
    assert_eq!(r["connections"][1]["uuid"], "3333-4444");

    // mmcli "missing": no modem, and a note, not a failure.
    assert_eq!(r["modem"], Value::Null);
    let errors: Vec<_> = r["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    assert!(
        errors.contains(&"mmcli: not found on container"),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.starts_with("internet ping: ping: exit 1")),
        "{errors:?}"
    );

    assert_eq!(
        r["checks"],
        json!({"gateway_ping": true, "internet_ping": false, "dns_resolve": true,
               "platform_https": true, "platform_status": 200})
    );
}

#[tokio::test]
async fn net_wifi_scan_dedupes() {
    let h = harness();
    let outcome = h.call("net_wifi_scan", Value::Null).await;
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    assert_eq!(
        outcome["response"],
        json!({"networks": [
            {"ssid": "Farm: Office", "signal": 71, "security": "WPA2", "freq": 2437},
            {"ssid": "Shed", "signal": null, "security": "open", "freq": 2462},
        ]})
    );
}

#[tokio::test]
async fn scan_network_defaults_to_the_primary_network() {
    let h = harness();
    let outcome = h.call("scan_network", json!({"ports": "502"})).await;
    assert_eq!(outcome["status"]["code"], "success", "{outcome}");
    assert_eq!(
        outcome["response"],
        json!({
            "subnet": "192.168.1.0/24",
            "interface": "eth0",
            "hosts": [
                {"ip": "192.168.1.1", "mac": "00:1a:2b:3c:4d:5e", "vendor": "NETGEAR",
                 "hostname": "router.lan", "open_ports": []},
                {"ip": "192.168.1.23", "mac": null, "vendor": null, "hostname": null,
                 "open_ports": []},
                {"ip": "192.168.1.40", "mac": "00:80:f4:11:22:33",
                 "vendor": "Schneider Electric", "hostname": null, "open_ports": [502]},
            ],
            "errors": [],
        })
    );
    let texts = h.progress_texts();
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("Scanning 192.168.1.0/24 (")),
        "{texts:?}"
    );
}

#[tokio::test]
async fn scan_network_narrows_big_networks() {
    let h = harness();
    let outcome = h.call("scan_network", json!({"interface": "wlan0"})).await;
    assert_eq!(outcome["response"]["subnet"], "10.1.0.0/22", "{outcome}");
    assert_eq!(outcome["response"]["interface"], "wlan0");
}

#[tokio::test]
async fn scan_network_picks_the_interface_for_a_subnet() {
    let h = harness();
    let outcome = h
        .call("scan_network", json!({"subnet": "192.168.1.128/25"}))
        .await;
    assert_eq!(
        outcome["response"]["subnet"], "192.168.1.128/25",
        "{outcome}"
    );
    assert_eq!(outcome["response"]["interface"], "eth0");
}

#[tokio::test]
async fn bad_params_are_rejected_before_acknowledging() {
    let cases = [
        ("scan_network", json!({"subnet": "10.0.0.0/16"})),
        ("scan_network", json!({"subnet": "nope"})),
        ("scan_network", json!({"subnet": 5})),
        ("scan_network", json!({"interface": "eth0; reboot"})),
        ("scan_network", json!({"ports": "0-10"})),
        ("scan_network", json!({"ports": "22 -sV"})),
        ("scan_network", json!("x")),
        ("net_status", json!({"verbose": true})),
        ("net_wifi_scan", json!([1])),
        ("exec", json!({"command": "true", "where": "moon"})),
        ("exec", json!({"command": "true", "where": 1})),
    ];
    for (method, request) in cases {
        let h = harness();
        let outcome = h.call(method, request.clone()).await;
        assert_eq!(
            outcome["status"]["message"]["code"], "INVALID_PARAMS",
            "{method} {request}: {outcome}"
        );
        assert!(
            h.updates()
                .iter()
                .all(|u| u["status"]["code"] != "acknowledged"),
            "{method} {request}"
        );
    }
}

#[tokio::test]
async fn exec_where_container_overrides_run_on_host() {
    let h = harness();
    let outcome = h
        .call("exec", json!({"command": "echo hi", "where": "container"}))
        .await;
    assert_eq!(outcome["response"]["stdout"], "hi\n", "{outcome}");
}

/// Off Linux, `where: "host"` can't enter the host and says so, even though
/// config `run_on_host` is false: the per-call value wins.
#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn exec_where_host_overrides_run_on_host() {
    let h = harness();
    let outcome = h
        .call("exec", json!({"command": "true", "where": "host"}))
        .await;
    assert_eq!(
        outcome["status"]["message"]["code"], "EXEC_FAILED",
        "{outcome}"
    );
}

/// The real tools, not the fakes, for eyeballing their output through the
/// parsers. Needs them installed, e.g. in the image's own Alpine:
///   docker run --rm --privileged --network host -v "$PWD":/src -w /src rust:1-alpine sh -c \
///     'apk add -q musl-dev networkmanager-cli nmap arp-scan iputils iproute2 curl &&
///      cargo test --test diag_rpc -- --ignored --nocapture'
#[tokio::test]
#[ignore]
async fn real_tools() {
    let backend = Arc::new(MockBackend::new());
    let rpc = Arc::new(RpcManager::new(
        backend.clone() as Arc<dyn ChannelBackend>,
        None,
    ));
    Arc::new(Gateway::new(
        AssistantGatewayConfig {
            max_timeout: 120.0,
            ..config()
        },
        AssistantGatewayTags::detached(),
    ))
    .register(&rpc, CHANNEL);
    let h = Harness { backend, rpc };
    for (method, request) in [
        ("net_status", json!({})),
        ("scan_network", json!({"ports": "22,80,443,502"})),
    ] {
        h.backend.message_updates.lock().unwrap().clear();
        let outcome = h.call(method, request).await;
        println!("{method}: {:#}", outcome);
        assert_eq!(outcome["status"]["code"], "success");
    }
}
