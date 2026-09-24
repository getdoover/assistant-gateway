from pydoover.tags import Tag, Tags


class AssistantGatewayTags(Tags):
    commands_run = Tag("integer", default=0)
    last_command = Tag("string", default="")
    last_exit_code = Tag("integer", default=0)
    last_run_ts = Tag("number", default=0)
