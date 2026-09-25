# Assistant Gateway

Runs arbitrary `sh` commands on a Linux Doover device, requested over RPC.
Commands run **on the host, as root** (the app joins PID 1's namespaces),
so this app is effectively remote root shell access to the device. Install it
only on devices where everyone who can write to the device's channels should
have that.

It also serves typed, read-only network diagnostics (`net_status`,
`net_wifi_scan`, `scan_network`) that return structured JSON, for callers
such as the platform's AI assistant that shouldn't need a free-form shell to
commission a device.

## RPC

Channel `dv-assistant-gateway` (config `rpc_channel`, advanced). Pass
`app_key` to target this install.

| Method          | Runs in   | Does                                              |
|-----------------|-----------|---------------------------------------------------|
| `exec`          | host*     | Any `sh -c` command                               |
| `net_status`    | host      | Interfaces, routes, DNS, wifi, modem, reachability|
| `net_wifi_scan` | host      | Visible wifi networks                             |
| `scan_network`  | container | Hosts on the LAN (ARP + ping sweep), open ports   |

\* per `where`, defaulting to config `run_on_host`.

"host" means the host's namespaces and binaries; "container" means this
app's Alpine image, which ships `nmap`, `arp-scan`, `nmcli`, `mbpoll`,
`socat`, `ping`, `ip`, `curl`, `jq` and busybox-extras (`telnet`, ...). The
container shares the host's network (`network_mode: host`) and, being
privileged, its `/dev`, so container tools see the host's interfaces and
serial ports.

Invalid parameters fail with `INVALID_PARAMS` before the call is acknowledged
or anything runs. All methods stream progress while they run (see
[Streaming](#streaming)) and can be cancelled from the site.

### `exec`

| Param     | Type   | Default          | Notes                                     |
|-----------|--------|------------------|-------------------------------------------|
| `command` | string | required         | Run with `sh -c`                          |
| `timeout` | number | `default_timeout`| Seconds; capped at `max_timeout`          |
| `cwd`     | string | `/`              | Working directory                         |
| `env`     | object | —                | Extra environment variables (strings)     |
| `stdin`   | string | —                | Fed to the command's stdin                |
| `where`   | string | per `run_on_host`| `"host"` or `"container"` (the Alpine tools); overrides `run_on_host` for this call |

Response:

```json
{"exit_code": 0, "stdout": "...", "stderr": "...", "duration": 0.12,
 "timed_out": false, "cancelled": false,
 "stdout_truncated": false, "stderr_truncated": false}
```

`exit_code` is `null` when the command was killed by a timeout or
cancellation, and negative (`-9`) when it died to a signal of its own. The
whole process group is killed, so background children go too.

The timeout covers the command's output as well as the command: a background
child still holding stdout/stderr (`daemon &`) keeps the call open until the
timeout, and is then killed. Redirect its output (`daemon >/dev/null 2>&1 &`)
to leave it running and return straight away.

### `net_status`

No parameters. Reads the host's network state and checks the way to the
platform; never fails because a tool is missing or a check fails -- those are
entries in `errors`. Each tool is tried on the host first and, if the host
doesn't have it (or `run_on_host` is false), in the container.

Sources: `nmcli -t -f DEVICE,TYPE,STATE,CONNECTION dev`,
`nmcli -t -f NAME,UUID,TYPE,DEVICE con show --active`, `ip -j addr`,
`ip -j route` (falling back to `ip -j route get 1.1.1.1` for the gateway when
the main table has no default route), `resolvectl status` or else
`/etc/resolv.conf`, `nmcli -t -f ACTIVE,SSID,SIGNAL,DEVICE dev wifi list
--rescan no` when there's a wifi device, `mmcli -J -L` / `-m <n>` / `-b <n>`.
Checks: `ping -c 1 -W 2` the default gateway and `1.1.1.1`,
`getent hosts api.doover.com`, and
`curl -sS -o /dev/null -m 5 -w %{http_code} https://api.doover.com/`.

```json
{
  "interfaces": [
    {"name": "eth0", "type": "ethernet", "state": "connected",
     "ip4": ["192.168.1.23/24"], "ip6": ["fe80::dea6:32ff:fe01:203/64"],
     "gateway": "192.168.1.1", "dns": ["192.168.1.1"],
     "mac": "dc:a6:32:01:02:03", "connection": "Wired connection 1",
     "wifi": null},
    {"name": "wlan0", "type": "wifi", "state": "connected",
     "ip4": ["10.1.0.5/16"], "ip6": [], "gateway": "10.1.0.1", "dns": [],
     "mac": "dc:a6:32:0a:0b:0c", "connection": "Farm Office",
     "wifi": {"ssid": "Farm Office", "signal": 71}}
  ],
  "routes": [
    {"dst": "default", "gateway": "192.168.1.1", "dev": "eth0",
     "protocol": "dhcp", "metric": 100, "prefsrc": null},
    {"dst": "192.168.1.0/24", "gateway": null, "dev": "eth0",
     "protocol": "kernel", "metric": 100, "prefsrc": "192.168.1.23"}
  ],
  "dns": ["192.168.1.1"],
  "connections": [
    {"name": "Wired connection 1", "uuid": "2b0e7c1e-...", "type": "802-3-ethernet", "device": "eth0"}
  ],
  "modem": {"state": "connected", "signal": 67, "operator": "Telstra", "apn": "telstra.internet"},
  "checks": {"gateway_ping": true, "internet_ping": true, "dns_resolve": true,
             "platform_https": true, "platform_status": 200},
  "errors": ["mmcli: not found on host; mmcli: not found on container"]
}
```

- `interfaces`: NetworkManager's devices, then any only `ip` knows of
  (Docker bridges, veths, ...); loopback left out. `type`/`state` are
  NetworkManager's (`ethernet`, `wifi`, `gsm`; `connected`, `disconnected`,
  `unavailable`, ...), or `ip`'s `link_type`/`operstate` (lower-cased) for
  interfaces NetworkManager doesn't list. `gateway` is the interface's
  default route, `dns` its per-link servers from resolvectl (empty without
  systemd-resolved), `wifi` the associated network or `null`.
- `dns`: every resolver in use (resolvectl's global and per-link servers, or
  `resolv.conf`'s `nameserver`s).
- `modem`: the first ModemManager modem, or `null` when there's none; any
  field can be `null`. `signal` is percent.
- `checks.platform_https` is true when the HTTPS request completed (any HTTP
  status); `platform_status` is that status, `null` if there was none.

### `net_wifi_scan`

No parameters. Runs `nmcli -t -f SSID,SIGNAL,SECURITY,FREQ dev wifi list
--rescan yes` (up to 30 s). Hidden (blank) SSIDs are dropped and each SSID
appears once, with its strongest access point, strongest first:

```json
{"networks": [
  {"ssid": "Farm Office", "signal": 71, "security": "WPA2", "freq": 2437},
  {"ssid": "Shed", "signal": 38, "security": "open", "freq": 2462}
]}
```

`signal` is percent, `freq` MHz, `security` nmcli's string (`"WPA1 WPA2"`,
`"WPA3"`, ...) or `"open"`. Fails with `WIFI_SCAN_FAILED` when nmcli can't
scan (no wifi device, NetworkManager not running).

### `scan_network`

Finds hosts on the local network, from the container: `arp-scan` and
`nmap -sn` side by side, merged by IP, then optionally `nmap -p <ports>
--open` against the hosts found. The whole scan is capped at 60 s; a step
cut short leaves a note in `errors` and whatever it found.

| Param       | Type            | Default | Notes |
|-------------|-----------------|---------|-------|
| `subnet`    | string          | the primary interface's network | IPv4 CIDR (or one address), no larger than a /22 (1024 addresses) |
| `interface` | string          | the default route's, or the one on `subnet` | Interface to scan from |
| `ports`     | string or array | —       | TCP ports to check: `"22,80,500-510"` or `[502, "8000-8080"]`; each 1-65535 |

With no `subnet`, the range is the interface's own network: refused
(`NO_SUBNET`) if that's larger than a /16, narrowed to the /22 around the
device if it's larger than a /22. Progress reads `Scanning 192.168.1.0/24
(12s)`.

```json
{
  "subnet": "192.168.1.0/24",
  "interface": "eth0",
  "hosts": [
    {"ip": "192.168.1.1", "mac": "00:1a:2b:3c:4d:5e", "vendor": "NETGEAR",
     "hostname": "router.lan", "open_ports": [80, 443]},
    {"ip": "192.168.1.40", "mac": "00:80:f4:11:22:33",
     "vendor": "Schneider Electric", "hostname": null, "open_ports": [502]}
  ],
  "errors": []
}
```

`hosts` is sorted by IP. `mac`/`vendor` come from arp-scan, `hostname` from
nmap's reverse DNS; hosts only nmap found (the device itself, say) have
`mac: null`. Fails with `SCAN_FAILED` only when neither arp-scan nor nmap
ran.

### Streaming

While a command runs, its output so far is written back to the command
message as `pending` progress updates, every `stream_interval` seconds
(default 2) when there's new output, and at least every 10 s as a heartbeat:

```json
{"status": {"code": "pending", "message": {
  "text": "Running (12s)", "elapsed": 12,
  "stdout": "...", "stderr": "...",
  "stdout_truncated": false, "stderr_truncated": false}}}
```

Each update carries the **cumulative** output (capped at `max_output_bytes`),
not a delta, so a consumer that misses an update loses nothing. Set
`stream_interval` to 0 to send heartbeats only.

Cancelling the command from the site kills it.

The typed methods report their current step instead of output, whenever the
step changes (checked every `stream_interval`) and at least every 10 s:

```json
{"status": {"code": "pending", "message": {
  "text": "Scanning 192.168.1.0/24 (12s)", "elapsed": 12,
  "step": "Scanning 192.168.1.0/24"}}}
```

### Example (pydoover, from another app)

```python
result = await self.rpc.call(
    "exec", {"command": "uptime && df -h /"},
    channel="dv-assistant-gateway", app_key="assistant_gateway_1", timeout=60,
)
hosts = await self.rpc.call(
    "scan_network", {"ports": "502"},
    channel="dv-assistant-gateway", app_key="assistant_gateway_1", timeout=90,
)
```

## Config

| Field              | Default | Notes                                             |
|--------------------|---------|---------------------------------------------------|
| `rpc_channel`      | `dv-assistant-gateway` | Advanced; channel `exec` is served on |
| `run_on_host`      | true    | false: `exec` defaults to the container, and the typed methods never enter the host (container tools only) |
| `default_timeout`  | 60      | Seconds                                           |
| `max_timeout`      | 600     | Upper bound on any requested timeout              |
| `stream_interval`  | 2       | Seconds between output updates; 0 disables        |
| `max_output_bytes` | 65536   | Per stream                                        |

## Telemetry tags

`commands_run` (every call), `last_method`, `last_run_ts`, and `exec`'s
`last_command` and `last_exit_code`.

## Deployment

The container needs `privileged: true` and `pid: host` to join the host's
namespaces — see `deployment/docker-compose.yml`. Without `pid: host`, PID 1
is the container's own init, and commands quietly run in the container.

`privileged: true` also exposes the host's `/dev` to the container, serial
ports included (and ones plugged in later), so container tools like `mbpoll`
reach RS-485 adapters without `devices:` entries. `network_mode: host` gives
container tools the host's interfaces. The host's D-Bus socket is mounted for
the container's `nmcli`, should anything use it; host-mode `nmcli` doesn't
need it.

## Development

Written in Rust on [doover-rs](https://github.com/getdoover/doover-rs).

```bash
cargo test                                   # tests
cargo run -- export doover_config.json --app-name assistant_gateway   # preview config schema
docker buildx build --platform linux/arm64 -t assistant-gateway:local --load .
```

The diagnostics' tests run against fake tools on PATH. To see the parsers on
real tool output (Linux, privileged, host network):

```bash
docker run --rm --privileged --network host -v "$PWD":/src -w /src rust:1-alpine sh -c \
  'apk add -q musl-dev networkmanager-cli nmap arp-scan iputils iproute2 curl &&
   cargo test --test diag_rpc -- --ignored --nocapture'
```

The host-namespace test needs a privileged Linux container:

```bash
docker run --rm --privileged --pid=host -v "$PWD":/src -w /src rust:1 \
  cargo test --test executor -- --ignored
```
