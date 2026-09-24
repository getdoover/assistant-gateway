# Assistant Gateway

Runs arbitrary `sh` commands on a Linux Doover device, requested over RPC.
Commands run **on the host, as root** (the app joins PID 1's namespaces),
so this app is effectively remote root shell access to the device. Install it
only on devices where everyone who can write to the device's channels should
have that.

## RPC

Channel `dv-assistant-gateway` (config `rpc_channel`, advanced), method `exec`. Pass `app_key` to target this install.

| Param     | Type   | Default          | Notes                                     |
|-----------|--------|------------------|-------------------------------------------|
| `command` | string | required         | Run with `sh -c`                          |
| `timeout` | number | `default_timeout`| Seconds; capped at `max_timeout`          |
| `cwd`     | string | `/`              | Working directory                         |
| `env`     | object | —                | Extra environment variables (strings)     |
| `stdin`   | string | —                | Fed to the command's stdin                |

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

### Example (pydoover, from another app)

```python
result = await self.rpc.call(
    "exec", {"command": "uptime && df -h /"},
    channel="dv-assistant-gateway", app_key="assistant_gateway_1", timeout=60,
)
```

## Config

| Field              | Default | Notes                                             |
|--------------------|---------|---------------------------------------------------|
| `rpc_channel`      | `dv-assistant-gateway` | Advanced; channel `exec` is served on |
| `run_on_host`      | true    | false runs inside the container instead           |
| `default_timeout`  | 60      | Seconds                                           |
| `max_timeout`      | 600     | Upper bound on any requested timeout              |
| `stream_interval`  | 2       | Seconds between output updates; 0 disables        |
| `max_output_bytes` | 65536   | Per stream                                        |

## Deployment

The container needs `privileged: true` and `pid: host` to join the host's
namespaces — see `deployment/docker-compose.yml`. Without `pid: host`, PID 1
is the container's own init, and commands quietly run in the container.

## Development

Written in Rust on [doover-rs](https://github.com/getdoover/doover-rs).

```bash
cargo test                                   # tests
cargo run -- export doover_config.json --app-name assistant_gateway   # preview config schema
```

The host-namespace test needs a privileged Linux container:

```bash
docker run --rm --privileged --pid=host -v "$PWD":/src -w /src rust:1 \
  cargo test --test executor -- --ignored
```
