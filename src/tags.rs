use doover::tags::Tag;
use doover::Tags;

/// Telemetry tags (no UI).
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
}
