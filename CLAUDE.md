# Assistant Gateway

Doover device app that runs arbitrary `sh` commands on the host over RPC
(`exec` on `dv-assistant-gateway`), plus typed network diagnostics
(`net_status`, `net_wifi_scan`, `scan_network`), checkpointed network changes
(`net_apply`) and Modbus (`probe_modbus`, `read_modbus`, `write_modbus`),
built in Rust on doover-rs.
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
  diag.rs       # typed methods: Runner (host/container/fallback over executor), net_status, net_wifi_scan, scan_network, the platform check
  netapply.rs   # net_apply: params, nmcli/busctl/dbus-send argv, checkpoint parsers (unit-tested inline), the checkpoint->apply->verify->destroy/rollback flow
  modbus.rs     # probe/read/write_modbus: params, mbpoll argv, mbpoll output parsers (fixtures from the real mbpoll inline)
  parse.rs      # pure parsers (nmcli -t, ip -j, resolvectl, mmcli, arp-scan, nmap -oG) + param validators + `Params` (strict object reader); unit tests inline
  config.rs     # config schema
  tags.rs       # telemetry tags
  main.rs       # doover::run
tests/
  executor.rs   # run_command
  exec_rpc.rs   # exec through a real RpcManager + MockBackend
  diag_rpc.rs   # typed methods against fake tools on PATH; `real_tools` (#[ignore]) runs the real ones
  net_apply_rpc.rs # net_apply against fake busctl/dbus-send/nmcli/curl (per-test state via with_tool_env), NetTiming-shrunk waits
  modbus_rpc.rs # Modbus methods against a fake mbpoll that prints the real one's format
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
- `net_apply` never changes addressing/wifi/LTE/DNS or takes anything down
  without a NetworkManager checkpoint (`NO_CHECKPOINT` instead); only "up"
  ops may go ahead without one. The checkpoint is created *before* the
  change and destroyed only after the platform answers; on a failed verify
  it's left for NetworkManager's timer (then confirmed gone). Resolve
  anything that needs looking up before creating the checkpoint. Its waits
  are in `NetTiming` units so tests can shrink them.
- mbpoll: always pass every link setting (its defaults are 19200 baud, EVEN
  parity), `-0` for 0-based addresses, `-1 -q`; never `-c` on a write (it
  refuses). Quiet mode still prints `-- Polling slave N...`; holding values
  over 32767 print as `65535 (-1)`.
- `net_apply` and the Modbus methods each hold a `try_lock` for the call
  (`BUSY` for a concurrent one).
- Tests give each gateway its own tool env (`Gateway::with_tool_env`) rather
  than mutating the process PATH, so per-test fakes can differ. Pre-run new
  fake scripts once: macOS makes the first exec of a fresh file slow.
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
