import asyncio
import os
import shlex
import signal
import time
from dataclasses import dataclass, asdict

#: A clean environment for host commands, so the container's PATH (which
#: points at /app/.venv) and other variables don't leak onto the host.
HOST_ENV = {
    "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME": "/root",
    "LANG": "C.UTF-8",
}

#: Join the mount, UTS, IPC, network and PID namespaces of the host's init.
NSENTER = ["nsenter", "-t", "1", "-m", "-u", "-i", "-n", "-p", "--"]


class LiveOutput:
    """Output captured so far, updated as the command writes it."""

    def __init__(self, limit: int):
        self.limit = limit
        self.stdout = bytearray()
        self.stderr = bytearray()
        self.stdout_truncated = False
        self.stderr_truncated = False
        #: Bumped on every captured chunk, so a streamer can tell what's new.
        self.version = 0

    def text(self, name: str) -> str:
        return getattr(self, name).decode("utf-8", errors="replace")


@dataclass
class CommandResult:
    exit_code: int | None
    stdout: str
    stderr: str
    duration: float
    timed_out: bool = False
    cancelled: bool = False
    stdout_truncated: bool = False
    stderr_truncated: bool = False

    def to_dict(self) -> dict:
        return asdict(self)


def build_argv(command: str, cwd: str | None, run_on_host: bool) -> list[str]:
    if cwd:
        command = f"cd -- {shlex.quote(cwd)} && {command}"
    argv = ["sh", "-c", command]
    return NSENTER + argv if run_on_host else argv


async def _capture(stream: asyncio.StreamReader, live: LiveOutput, name: str):
    # Keep draining past the limit, or a chatty command blocks on a full pipe.
    buf = getattr(live, name)
    while chunk := await stream.read(65536):
        room = live.limit - len(buf)
        if room > 0:
            buf += chunk[:room]
            live.version += 1
        if len(chunk) > room:
            setattr(live, f"{name}_truncated", True)


async def _feed(pipe: asyncio.StreamWriter, data: str):
    try:
        pipe.write(data.encode())
        await pipe.drain()
        pipe.close()
    except (BrokenPipeError, ConnectionResetError):
        pass


def _kill(proc: asyncio.subprocess.Process):
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


async def run_command(
    command: str,
    *,
    timeout: float,
    max_output_bytes: int,
    run_on_host: bool = True,
    cwd: str | None = None,
    env: dict[str, str] | None = None,
    stdin: str | None = None,
    cancelled: asyncio.Event | None = None,
    live: LiveOutput | None = None,
) -> CommandResult:
    base_env = dict(HOST_ENV) if run_on_host else dict(os.environ)
    base_env.update(env or {})

    start = time.monotonic()
    proc = await asyncio.create_subprocess_exec(
        *build_argv(command, cwd, run_on_host),
        stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=base_env,
        # Own process group, so a timeout kills everything the command spawned.
        start_new_session=True,
    )

    live = live or LiveOutput(max_output_bytes)
    readers = asyncio.gather(
        _capture(proc.stdout, live, "stdout"),
        _capture(proc.stderr, live, "stderr"),
    )
    if stdin is not None:
        asyncio.ensure_future(_feed(proc.stdin, stdin))
    waiter = asyncio.ensure_future(proc.wait())
    watchers = [waiter]
    cancel_task = None
    if cancelled is not None:
        cancel_task = asyncio.ensure_future(cancelled.wait())
        watchers.append(cancel_task)

    done, _ = await asyncio.wait(
        watchers, timeout=timeout, return_when=asyncio.FIRST_COMPLETED
    )
    timed_out = not done
    was_cancelled = cancel_task is not None and cancel_task in done
    if cancel_task is not None:
        cancel_task.cancel()
    if timed_out or was_cancelled:
        _kill(proc)

    await waiter
    await readers

    return CommandResult(
        exit_code=None if (timed_out or was_cancelled) else proc.returncode,
        stdout=live.text("stdout"),
        stderr=live.text("stderr"),
        duration=round(time.monotonic() - start, 3),
        timed_out=timed_out,
        cancelled=was_cancelled,
        stdout_truncated=live.stdout_truncated,
        stderr_truncated=live.stderr_truncated,
    )
