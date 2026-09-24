import asyncio

import pytest
from pydoover import rpc

from assistant_gateway import application
from assistant_gateway.application import AssistantGatewayApplication


class FakeValue:
    def __init__(self, value):
        self.value = value


class FakeConfig:
    def __init__(self, **overrides):
        values = dict(
            run_on_host=False,
            default_timeout=5.0,
            max_timeout=10.0,
            stream_interval=0.1,
            max_output_bytes=1024,
        )
        values.update(overrides)
        for k, v in values.items():
            setattr(self, k, FakeValue(v))


class FakeTag:
    def __init__(self):
        self.value = None

    def get(self):
        return self.value

    async def set(self, value):
        self.value = value


class FakeTags:
    def __init__(self):
        for name in ("commands_run", "last_command", "last_exit_code", "last_run_ts"):
            setattr(self, name, FakeTag())


class FakeCtx:
    actor = None

    def __init__(self):
        self._cancelled = asyncio.Event()
        self.progress_calls = []

    async def acknowledge(self):
        pass

    async def progress(self, text=None, **fields):
        self.progress_calls.append(fields)

    def raise_if_cancelled(self):
        if self._cancelled.is_set():
            raise rpc.RPCCancelled("exec")


@pytest.fixture
def app():
    a = AssistantGatewayApplication.__new__(AssistantGatewayApplication)
    a.config = FakeConfig()
    a.tags = FakeTags()
    return a


async def test_exec_returns_result(app):
    result = await app.rpc_exec(FakeCtx(), {"command": "echo hi"})
    assert result["exit_code"] == 0 and result["stdout"] == "hi\n"
    assert app.tags.commands_run.value == 1


async def test_exec_streams_output(app, monkeypatch):
    ctx = FakeCtx()
    await app.rpc_exec(ctx, {"command": "echo a; sleep 0.4; echo b; sleep 0.4"})
    outputs = [c["stdout"] for c in ctx.progress_calls]
    assert "a\n" in outputs and "a\nb\n" in outputs


async def test_exec_no_stream_when_disabled(app, monkeypatch):
    monkeypatch.setattr(application, "HEARTBEAT_INTERVAL", 0.2)
    app.config = FakeConfig(stream_interval=0.0)
    ctx = FakeCtx()
    await app.rpc_exec(ctx, {"command": "echo a; sleep 0.5"})
    assert ctx.progress_calls and all("stdout" not in c for c in ctx.progress_calls)


@pytest.mark.parametrize(
    "payload",
    [{}, {"command": ""}, {"command": "ls", "env": {"A": 1}}, {"command": "ls", "timeout": -1}],
)
async def test_exec_rejects_bad_params(app, payload):
    with pytest.raises(rpc.RPCError):
        await app.rpc_exec(FakeCtx(), payload)


async def test_timeout_capped(app):
    assert app._timeout(9999) == 10.0
    assert app._timeout(None) == 5.0
