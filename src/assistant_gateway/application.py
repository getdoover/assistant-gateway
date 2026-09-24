import asyncio
import logging
import time

from pydoover import rpc
from pydoover.docker import Application

from .app_config import AssistantGatewayConfig
from .app_tags import AssistantGatewayTags
from .executor import LiveOutput, run_command

log = logging.getLogger(__name__)

#: Longest gap between progress updates even with no new output, so the site
#: doesn't give up on a quiet command as "no response from device".
HEARTBEAT_INTERVAL = 10


class AssistantGatewayApplication(Application):
    """Executes arbitrary sh commands on the device, requested over RPC."""

    config_cls = AssistantGatewayConfig
    tags_cls = AssistantGatewayTags

    config: AssistantGatewayConfig
    tags: AssistantGatewayTags

    loop_target_period = 10

    async def setup(self):
        # The channel comes from config, so it can't go in the @rpc.handler
        # decorator: the handler is registered channel-less and we subscribe
        # to the configured channel here instead.
        self.rpc.subscribe(self.config.rpc_channel.value)

    async def main_loop(self):
        pass

    def _timeout(self, requested) -> float:
        max_timeout = self.config.max_timeout.value
        if requested is None:
            return min(self.config.default_timeout.value, max_timeout)
        try:
            requested = float(requested)
        except (TypeError, ValueError):
            raise rpc.RPCError("INVALID_PARAMS", "'timeout' must be a number")
        if requested <= 0:
            raise rpc.RPCError("INVALID_PARAMS", "'timeout' must be greater than zero")
        return min(requested, max_timeout)

    @rpc.handler("exec")
    async def rpc_exec(self, ctx, payload: dict) -> dict:
        """Run ``payload["command"]`` with ``sh -c``.

        Optional: ``timeout`` (s), ``cwd``, ``env`` (dict), ``stdin`` (str).
        A timed-out or cancelled command is killed along with its children.
        """
        # A channel-less handler serves every channel the RPC manager is
        # subscribed to, so only honour the configured one.
        if ctx.channel.name != self.config.rpc_channel.value:
            raise rpc.RPCError(
                "WRONG_CHANNEL", f"exec is served on {self.config.rpc_channel.value}"
            )
        if not isinstance(payload, dict):
            raise rpc.RPCError("INVALID_PARAMS", "payload must be an object")
        command = payload.get("command")
        if not isinstance(command, str) or not command.strip():
            raise rpc.RPCError("INVALID_PARAMS", "'command' must be a non-empty string")
        env = payload.get("env")
        if env is not None and not (
            isinstance(env, dict) and all(isinstance(v, str) for v in env.values())
        ):
            raise rpc.RPCError("INVALID_PARAMS", "'env' must map names to strings")
        stdin = payload.get("stdin")
        if stdin is not None and not isinstance(stdin, str):
            raise rpc.RPCError("INVALID_PARAMS", "'stdin' must be a string")
        timeout = self._timeout(payload.get("timeout"))

        log.info("exec requested by %s: %r", ctx.actor, command)
        await ctx.acknowledge()

        live = LiveOutput(self.config.max_output_bytes.value)
        task = asyncio.create_task(
            run_command(
                command,
                timeout=timeout,
                max_output_bytes=self.config.max_output_bytes.value,
                run_on_host=self.config.run_on_host.value,
                cwd=payload.get("cwd"),
                env=env,
                stdin=stdin,
                cancelled=ctx._cancelled,
                live=live,
            )
        )
        await self._stream(ctx, task, live)

        try:
            result = task.result()
        except OSError as exc:
            raise rpc.RPCError("EXEC_FAILED", str(exc)) from exc

        await self.tags.commands_run.set((self.tags.commands_run.get() or 0) + 1)
        await self.tags.last_command.set(command[:200])
        await self.tags.last_exit_code.set(
            -1 if result.exit_code is None else result.exit_code
        )
        await self.tags.last_run_ts.set(time.time())

        ctx.raise_if_cancelled()
        return result.to_dict()

    async def _stream(self, ctx, task: asyncio.Task, live: LiveOutput):
        """Report output as progress until the command finishes.

        Each update carries the whole output so far (capped at
        ``max_output_bytes``), not a delta, so the message always holds a
        complete picture even if a consumer misses intermediate updates.
        """
        interval = self.config.stream_interval.value
        tick = min(interval, HEARTBEAT_INTERVAL) if interval > 0 else HEARTBEAT_INTERVAL
        started = last_sent = time.monotonic()
        sent_version = 0
        while not task.done():
            await asyncio.wait([task], timeout=tick)
            if task.done():
                break
            now = time.monotonic()
            has_new = interval > 0 and live.version != sent_version
            if not has_new and now - last_sent < HEARTBEAT_INTERVAL:
                continue
            elapsed = int(now - started)
            fields = {"elapsed": elapsed}
            if interval > 0:
                fields.update(
                    stdout=live.text("stdout"),
                    stderr=live.text("stderr"),
                    stdout_truncated=live.stdout_truncated,
                    stderr_truncated=live.stderr_truncated,
                )
            sent_version = live.version
            last_sent = now
            try:
                await ctx.progress(f"Running ({elapsed}s)", **fields)
            except Exception as exc:
                # A failed update must not abandon the command it's reporting on.
                log.warning("progress update failed: %s", exc)
