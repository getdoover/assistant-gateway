from pathlib import Path

from pydoover import config


class AssistantGatewayConfig(config.Schema):
    run_on_host = config.Boolean(
        "Run On Host",
        name="run_on_host",
        default=True,
        description=(
            "Run commands in the host's namespaces (via nsenter into PID 1) "
            "rather than inside this container. Needs the container to run "
            "privileged with pid: host."
        ),
    )

    default_timeout = config.Number(
        "Default Timeout",
        name="default_timeout",
        default=60.0,
        minimum=1.0,
        description="Seconds a command may run when the call gives no timeout.",
    )

    max_timeout = config.Number(
        "Max Timeout",
        name="max_timeout",
        default=600.0,
        minimum=1.0,
        description="Upper bound on any requested timeout, in seconds.",
    )

    stream_interval = config.Number(
        "Stream Interval",
        name="stream_interval",
        default=2.0,
        minimum=0.0,
        description=(
            "Seconds between streaming a running command's output back as "
            "progress updates on its RPC message. Updates are only sent when "
            "there is new output. 0 disables streaming."
        ),
    )

    max_output_bytes = config.Integer(
        "Max Output Bytes",
        name="max_output_bytes",
        default=65536,
        minimum=1024,
        description=(
            "Stdout and stderr are each truncated to this many bytes, to keep "
            "the response message a sensible size."
        ),
    )


def export():
    AssistantGatewayConfig.export(
        Path(__file__).parents[2] / "doover_config.json", "assistant_gateway"
    )


if __name__ == "__main__":
    export()
