# Assistant Gateway

Doover device app that runs arbitrary `sh` commands on the host over RPC
(`exec` on `dv-rpc`), built on pydoover 1.0. See `README.md` for the interface.

## Commands

```bash
uv run pytest tests -v   # tests
uv run export-config     # preview config schema (publish generates it; don't commit)
```

## Layout

```
src/assistant_gateway/
  executor.py      # subprocess, output capping, timeout/cancel kill; no pydoover
  application.py   # RPC handler + output streaming via ctx.progress()
  app_config.py    # config schema
  app_tags.py      # telemetry tags (no UI)
```

## Things worth knowing

- Host execution is `nsenter -t 1 -m -u -i -n -p -- sh -c ...`, which needs
  `privileged` + `pid: host`. The base image is Alpine, so `nsenter` comes from
  `util-linux-misc` (added in the Dockerfile).
- Host commands get a clean env (`HOST_ENV`), not the container's — otherwise
  the container's PATH (`/app/.venv/bin`) leaks onto the host.
- Commands run in their own session so a timeout/cancel `killpg`s everything.
- Output pipes keep draining past `max_output_bytes`, or a chatty command
  would block on a full pipe and look hung.
- Streamed progress sends cumulative output, not deltas: the message's current
  state must be complete on its own.
