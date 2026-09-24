use doover::Config;

#[derive(Debug, Clone, Config)]
pub struct AssistantGatewayConfig {
    /// Channel this app listens on for exec requests.
    #[config(title = "RPC Channel", default = "dv-assistant-gateway", advanced)]
    pub rpc_channel: String,

    /// Run commands in the host's namespaces (joining PID 1's) rather than inside this container. Needs the container to run privileged with pid: host.
    #[config(default = true)]
    pub run_on_host: bool,

    /// Seconds a command may run when the call gives no timeout.
    #[config(default = 60.0, min = 1.0)]
    pub default_timeout: f64,

    /// Upper bound on any requested timeout, in seconds.
    #[config(default = 600.0, min = 1.0)]
    pub max_timeout: f64,

    /// Seconds between streaming a running command's output back as progress updates on its RPC message. Updates are only sent when there is new output. 0 disables streaming.
    #[config(default = 2.0, min = 0.0)]
    pub stream_interval: f64,

    /// Stdout and stderr are each truncated to this many bytes, to keep the response message a sensible size.
    #[config(default = 65536, min = 1024)]
    pub max_output_bytes: i64,
}
