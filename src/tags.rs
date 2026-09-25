use doover::tags::Tag;
use doover::Tags;

/// Telemetry tags (no UI). `commands_run` counts every call, typed methods
/// included; `last_command` and `last_exit_code` are `exec`'s.
#[derive(Clone, Tags)]
pub struct AssistantGatewayTags {
    #[tag(default = 0)]
    pub commands_run: Tag<i64>,
    #[tag(default = "")]
    pub last_command: Tag<String>,
    #[tag(default = 0)]
    pub last_exit_code: Tag<i64>,
    #[tag(default = 0.0)]
    pub last_run_ts: Tag<f64>,
    /// The RPC method of the last call: `exec`, `net_status`, ...
    #[tag(default = "")]
    pub last_method: Tag<String>,
}
