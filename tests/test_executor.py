import asyncio

from assistant_gateway.executor import LiveOutput, build_argv, run_command


def test_build_argv_host_and_cwd():
    argv = build_argv("ls", "/tmp/a b", run_on_host=True)
    assert argv[0] == "nsenter"
    assert argv[-3:] == ["sh", "-c", "cd -- '/tmp/a b' && ls"]
    assert build_argv("ls", None, run_on_host=False) == ["sh", "-c", "ls"]


async def run(cmd, **kw):
    kw.setdefault("timeout", 5)
    kw.setdefault("max_output_bytes", 1024)
    return await run_command(cmd, run_on_host=False, **kw)


async def test_exit_code_and_streams():
    r = await run("echo out; echo err >&2; exit 3")
    assert (r.exit_code, r.stdout, r.stderr) == (3, "out\n", "err\n")


async def test_stdin_env_cwd():
    r = await run('cat; echo "$FOO"; pwd', stdin="in\n", env={"FOO": "bar"}, cwd="/")
    assert r.stdout == "in\nbar\n/\n"


async def test_truncation():
    r = await run("yes | head -c 100000", max_output_bytes=1024)
    assert len(r.stdout) == 1024 and r.stdout_truncated


async def test_timeout_kills_children():
    r = await run("sleep 30 & sleep 30; wait", timeout=0.5)
    assert r.timed_out and r.exit_code is None and r.duration < 5


async def test_cancel():
    ev = asyncio.Event()
    asyncio.get_running_loop().call_later(0.2, ev.set)
    r = await run("sleep 30", cancelled=ev)
    assert r.cancelled and r.exit_code is None


async def test_live_output_fills_while_running():
    live = LiveOutput(1024)
    task = asyncio.create_task(run("echo first; sleep 0.5; echo second", live=live))
    await asyncio.sleep(0.25)
    assert live.text("stdout") == "first\n" and not task.done()
    assert (await task).stdout == "first\nsecond\n"
