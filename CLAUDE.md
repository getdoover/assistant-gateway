# Assistant Gateway

Doover device app that runs arbitrary `sh` commands on the host over RPC
(`exec` on `dv-assistant-gateway`), plus typed network diagnostics
(`net_status`, `net_wifi_scan`, `scan_network`), built in Rust on doover-rs.
See `README.md` for the interface.

## Commands

```bash
cargo test                                   # tests (host-namespace test is #[ignore]; see README)
cargo clippy --all-targets -- -D warnings    # CI runs this and `cargo fmt --check`
cargo run -- export doover_config.json --app-name assistant_gateway
                                             # preview config schema (publish generates it; don't commit)
```

## Layout

```
src/
  executor.rs   # subprocess, output capping, timeout/cancel kill, setns; no doover
  app.rs        # Gateway (RPC handlers, param validation, progress streaming) and the Application
  diag.rs       # typed methods: Runner (host/container/fallback over executor), net_status, net_wifi_scan, scan_network
  parse.rs      # pure parsers (nmcli -t, ip -j, resolvectl, mmcli, arp-scan, nmap -oG) + param validators; unit tests inline
  config.rs     # config schema
  tags.rs       # telemetry tags
  main.rs       # doover::run
tests/
  executor.rs   # run_command
  exec_rpc.rs   # exec through a real RpcManager + MockBackend
  diag_rpc.rs   # typed methods against fake tools on PATH; `real_tools` (#[ignore]) runs the real ones
```

## Things worth knowing

- Every method is registered on the configured `rpc_channel` in `setup()`, so
  only that channel is served. Changing the channel needs a restart.
- Host execution joins PID 1's ipc/uts/net/mnt namespaces with `setns` in the
  forked child (`pre_exec`), then runs the host's `/bin/sh` — no `nsenter`.
  Needs `privileged` + `pid: host`; without `pid: host`, `/proc/1` is the
  container's init and commands silently run in the container.
- The image is Alpine, not scratch, so container mode has a shell and the
  diagnostic tools. Those are apk packages installed per target arch (CI sets
  up QEMU); mbpoll isn't packaged and is built from a tagged git clone in its
  own stage (its CMake reads the version from `git describe`). Only the Rust
  binary is cross-compiled with zig.
- Typed methods run fixed argv through `executor::run_command` (via
  `shell_join`), not a second spawn path. `Place::HostThenContainer` falls
  back to the container on exit 127 or a failed host entry; the container
  shares the host's network, so network tools answer the same. With
  `run_on_host: false` they never enter the host.
- A missing tool or failed check in `net_status` is an `errors` entry, never
  a failed call. Validate typed-method params before `acknowledge()`, like
  `exec` does.
- Host commands get a clean env (`HOST_ENV`), not the container's.
- Commands run in their own session so a timeout/cancel `killpg`s everything.
- Output pipes keep draining past `max_output_bytes`, or a chatty command
  would block on a full pipe and look hung. The timeout also bounds the
  drain, so a backgrounded child holding the pipes can't hang the call.
- Streamed progress sends cumulative output, not deltas: the message's current
  state must be complete on its own.
- The handler runs on its own task per request, so it can't borrow the app;
  shared state lives in `Gateway` behind an `Arc`, and `on_config_update`
  swaps its config.
