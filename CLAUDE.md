# Assistant Gateway

Doover device app that runs arbitrary `sh` commands on the host over RPC
(`exec` on `dv-assistant-gateway`), built in Rust on doover-rs. See `README.md`
for the interface.

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
  app.rs        # Gateway (RPC handler + progress streaming) and the Application
  config.rs     # config schema
  tags.rs       # telemetry tags
  main.rs       # doover::run
```

## Things worth knowing

- `exec` is registered on the configured `rpc_channel` in `setup()`, so only
  that channel is served. Changing the channel needs a restart.
- Host execution joins PID 1's ipc/uts/net/mnt namespaces with `setns` in the
  forked child (`pre_exec`), then runs the host's `/bin/sh` — no `nsenter`.
  Needs `privileged` + `pid: host`; without `pid: host`, `/proc/1` is the
  container's init and commands silently run in the container.
- The image is busybox, not scratch, only so `run_on_host: false` has a shell.
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
