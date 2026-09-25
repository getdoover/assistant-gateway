//! The RPC handlers (`exec` and the typed diagnostics in [`crate::diag`]) and
//! progress streaming via `ctx.progress()`.

use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use doover::error::Result;
use doover::rpc::{RpcContext, RpcError, RpcManager};
use doover::ui::NoUi;
use doover::{AppContext, Application};
use serde_json::{json, Map, Value};
use tokio::task::{JoinError, JoinHandle};

use crate::config::AssistantGatewayConfig;
use crate::diag::{self, Runner, ScanParams, Step};
use crate::executor::{run_command, CommandResult, CommandSpec, LiveOutput};
use crate::tags::AssistantGatewayTags;

/// Longest gap between progress updates even with no new output, so the site
/// doesn't give up on a quiet command as "no response from device".
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

fn invalid(message: &str) -> RpcError {
    RpcError::new("INVALID_PARAMS", message)
}

/// Executes `exec` requests. Shared between the app and the RPC handler it
/// registers, which runs on its own task per request.
pub struct Gateway {
    config: RwLock<AssistantGatewayConfig>,
    tags: AssistantGatewayTags,
    heartbeat: Duration,
}

impl Gateway {
    pub fn new(config: AssistantGatewayConfig, tags: AssistantGatewayTags) -> Self {
        Self {
            config: RwLock::new(config),
            tags,
            heartbeat: HEARTBEAT_INTERVAL,
        }
    }

    pub fn with_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    pub fn config(&self) -> AssistantGatewayConfig {
        self.config.read().unwrap().clone()
    }

    pub fn set_config(&self, config: AssistantGatewayConfig) {
        *self.config.write().unwrap() = config;
    }

    /// Serve every method on `channel` only.
    pub fn register(self: &Arc<Self>, rpc: &RpcManager, channel: &str) {
        let gateway = self.clone();
        rpc.register(Some(channel), "exec", move |ctx, payload| {
            let gateway = gateway.clone();
            async move { gateway.exec(ctx, payload).await }
        });
        let gateway = self.clone();
        rpc.register(Some(channel), "net_status", move |ctx, payload| {
            let gateway = gateway.clone();
            async move { gateway.net_status(ctx, payload).await }
        });
        let gateway = self.clone();
        rpc.register(Some(channel), "net_wifi_scan", move |ctx, payload| {
            let gateway = gateway.clone();
            async move { gateway.net_wifi_scan(ctx, payload).await }
        });
        let gateway = self.clone();
        rpc.register(Some(channel), "scan_network", move |ctx, payload| {
            let gateway = gateway.clone();
            async move { gateway.scan_network(ctx, payload).await }
        });
    }

    pub fn timeout(&self, requested: Option<&Value>) -> std::result::Result<Duration, RpcError> {
        let config = self.config.read().unwrap();
        let requested = match requested {
            None | Some(Value::Null) => config.default_timeout,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
            Some(Value::String(s)) => s
                .trim()
                .parse()
                .map_err(|_| invalid("'timeout' must be a number"))?,
            Some(_) => return Err(invalid("'timeout' must be a number")),
        };
        if requested.is_nan() || requested <= 0.0 {
            return Err(invalid("'timeout' must be greater than zero"));
        }
        Ok(Duration::from_secs_f64(requested.min(config.max_timeout)))
    }

    fn parse(&self, payload: Value) -> std::result::Result<CommandSpec, RpcError> {
        let Value::Object(payload) = payload else {
            return Err(invalid("payload must be an object"));
        };
        let optional_str = |key: &str| match payload.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(invalid(&format!("'{key}' must be a string"))),
        };

        let command = payload
            .get("command")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| invalid("'command' must be a non-empty string"))?;
        let env = match payload.get("env") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(env)) => env
                .iter()
                .map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect::<Option<_>>()
                .ok_or_else(|| invalid("'env' must map names to strings"))?,
            Some(_) => return Err(invalid("'env' must map names to strings")),
        };

        let run_on_host = match optional_str("where")?.as_deref() {
            None => self.config.read().unwrap().run_on_host,
            Some("host") => true,
            Some("container") => false,
            Some(_) => return Err(invalid("'where' must be \"host\" or \"container\"")),
        };

        Ok(CommandSpec {
            command: command.to_string(),
            cwd: optional_str("cwd")?,
            env,
            stdin: optional_str("stdin")?,
            timeout: self.timeout(payload.get("timeout"))?,
            run_on_host,
        })
    }

    /// Run `payload["command"]` with `sh -c`.
    ///
    /// Optional: `timeout` (s), `cwd`, `env` (object), `stdin` (string),
    /// `where` (`"host"` / `"container"`, default per `run_on_host`). A
    /// timed-out or cancelled command is killed along with its children.
    pub async fn exec(
        &self,
        ctx: RpcContext,
        payload: Value,
    ) -> std::result::Result<Value, RpcError> {
        let spec = self.parse(payload)?;
        let config = self.config();

        tracing::info!("exec requested by {:?}: {:?}", ctx.actor(), spec.command);
        if let Err(e) = ctx.acknowledge().await {
            tracing::warn!("failed to acknowledge exec: {e}");
        }

        let command = spec.command.clone();
        let live = LiveOutput::new(config.max_output_bytes.max(0) as usize);
        let task = tokio::spawn(run_command(spec, live.clone(), {
            let ctx = ctx.clone();
            async move { ctx.wait_cancelled().await }
        }));

        let result = match self.stream(&ctx, task, &live, config.stream_interval).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => return Err(RpcError::new("EXEC_FAILED", e.to_string())),
            Err(e) => return Err(RpcError::new("EXEC_FAILED", e.to_string())),
        };
        self.record(&command, &result).await;

        ctx.error_if_cancelled()?;
        Ok(serde_json::to_value(result).expect("CommandResult serializes"))
    }

    /// Report output as progress until the command finishes, then return its
    /// result.
    ///
    /// Each update carries the whole output so far (capped at
    /// `max_output_bytes`), not a delta, so the message always holds a
    /// complete picture even if a consumer misses intermediate updates.
    async fn stream(
        &self,
        ctx: &RpcContext,
        mut task: JoinHandle<io::Result<CommandResult>>,
        live: &Arc<Mutex<LiveOutput>>,
        interval: f64,
    ) -> std::result::Result<io::Result<CommandResult>, JoinError> {
        let streaming = interval > 0.0;
        let tick = if streaming {
            Duration::from_secs_f64(interval).min(self.heartbeat)
        } else {
            self.heartbeat
        };
        let started = Instant::now();
        let mut last_sent = started;
        let mut sent_version = 0;
        loop {
            tokio::select! {
                result = &mut task => return result,
                _ = tokio::time::sleep(tick) => {}
            }
            let now = Instant::now();
            let (fields, version) = {
                let live = live.lock().unwrap();
                let has_new = streaming && live.version != sent_version;
                if !has_new && now - last_sent < self.heartbeat {
                    continue;
                }
                let mut fields = Map::new();
                fields.insert("elapsed".into(), json!((now - started).as_secs()));
                if streaming {
                    fields.insert("stdout".into(), json!(live.stdout_text()));
                    fields.insert("stderr".into(), json!(live.stderr_text()));
                    fields.insert("stdout_truncated".into(), json!(live.stdout_truncated));
                    fields.insert("stderr_truncated".into(), json!(live.stderr_truncated));
                }
                (fields, live.version)
            };
            sent_version = version;
            last_sent = now;
            let text = format!("Running ({}s)", (now - started).as_secs());
            if let Err(e) = ctx.progress(Some(&text), Value::Object(fields)).await {
                // A failed update must not abandon the command it's reporting on.
                tracing::warn!("progress update failed: {e}");
            }
        }
    }

    /// Network state of the host: interfaces, routes, DNS, wifi, modem, and
    /// reachability checks. Never fails for a missing tool; see `errors`.
    pub async fn net_status(
        &self,
        ctx: RpcContext,
        payload: Value,
    ) -> std::result::Result<Value, RpcError> {
        no_params(&payload)?;
        self.typed(ctx, "net_status", |runner, step| async move {
            Ok(diag::net_status(&runner, &step).await)
        })
        .await
    }

    /// Visible wifi networks (`nmcli dev wifi list --rescan yes`).
    pub async fn net_wifi_scan(
        &self,
        ctx: RpcContext,
        payload: Value,
    ) -> std::result::Result<Value, RpcError> {
        no_params(&payload)?;
        self.typed(ctx, "net_wifi_scan", |runner, step| async move {
            diag::net_wifi_scan(&runner, &step).await
        })
        .await
    }

    /// Hosts on the local network, optionally with open ports. Optional
    /// `subnet`, `interface`, `ports`.
    pub async fn scan_network(
        &self,
        ctx: RpcContext,
        payload: Value,
    ) -> std::result::Result<Value, RpcError> {
        let params = ScanParams::parse(&payload)?;
        self.typed(ctx, "scan_network", |runner, step| async move {
            diag::scan_network(&runner, &step, params).await
        })
        .await
    }

    /// The shared shape of a typed method: acknowledge, run `body` while
    /// reporting its current step as progress, record it, and leave a
    /// cancellation standing.
    async fn typed<F, Fut>(
        &self,
        ctx: RpcContext,
        method: &str,
        body: F,
    ) -> std::result::Result<Value, RpcError>
    where
        F: FnOnce(Runner, Step) -> Fut,
        Fut: Future<Output = std::result::Result<Value, RpcError>>,
    {
        tracing::info!("{method} requested by {:?}", ctx.actor());
        if let Err(e) = ctx.acknowledge().await {
            tracing::warn!("failed to acknowledge {method}: {e}");
        }
        let step = Step::new("Starting");
        let runner = Runner::new(ctx.clone(), self.config().run_on_host);
        let result = self
            .with_progress(&ctx, &step, body(runner, step.clone()))
            .await;
        self.record_method(method).await;
        ctx.error_if_cancelled()?;
        result
    }

    /// Drive `work`, sending `"<step> (<elapsed>s)"` as progress whenever the
    /// step changes (checked every `stream_interval`), and at least every
    /// heartbeat. With `stream_interval` 0, heartbeats only.
    async fn with_progress<T>(
        &self,
        ctx: &RpcContext,
        step: &Step,
        work: impl Future<Output = T>,
    ) -> T {
        let interval = self.config().stream_interval;
        let streaming = interval > 0.0;
        let tick = if streaming {
            Duration::from_secs_f64(interval).min(self.heartbeat)
        } else {
            self.heartbeat
        };
        let started = Instant::now();
        let mut last_sent = started;
        let mut sent_version = 0;
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => return result,
                _ = tokio::time::sleep(tick) => {}
            }
            let now = Instant::now();
            let (text, version) = step.get();
            if !(streaming && version != sent_version) && now - last_sent < self.heartbeat {
                continue;
            }
            sent_version = version;
            last_sent = now;
            let elapsed = (now - started).as_secs();
            let line = format!("{text} ({elapsed}s)");
            let fields = json!({"elapsed": elapsed, "step": text});
            if let Err(e) = ctx.progress(Some(&line), fields).await {
                tracing::warn!("progress update failed: {e}");
            }
        }
    }

    /// Telemetry tags for a typed method. Never fails the call.
    async fn record_method(&self, method: &str) {
        let tags = &self.tags;
        let outcome = async {
            tags.commands_run
                .set(tags.commands_run.get().unwrap_or(0) + 1)
                .await?;
            tags.last_method.set(method.to_string()).await?;
            tags.last_run_ts.set(unix_now()).await
        };
        if let Err(e) = outcome.await {
            tracing::warn!("failed to update tags: {e}");
        }
    }

    /// Telemetry tags. Never fails the call: the command has already run.
    async fn record(&self, command: &str, result: &CommandResult) {
        let tags = &self.tags;
        let now = unix_now();
        let outcome = async {
            tags.last_method.set("exec".to_string()).await?;
            tags.commands_run
                .set(tags.commands_run.get().unwrap_or(0) + 1)
                .await?;
            tags.last_command
                .set(command.chars().take(200).collect())
                .await?;
            tags.last_exit_code
                .set(result.exit_code.map_or(-1, i64::from))
                .await?;
            tags.last_run_ts.set(now).await
        };
        if let Err(e) = outcome.await {
            tracing::warn!("failed to update tags: {e}");
        }
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// The typed methods without parameters take an empty object (or nothing).
fn no_params(payload: &Value) -> std::result::Result<(), RpcError> {
    match payload {
        Value::Null => Ok(()),
        Value::Object(m) if m.is_empty() => Ok(()),
        Value::Object(m) => Err(invalid(&format!(
            "unexpected parameter(s): {}",
            m.keys().cloned().collect::<Vec<_>>().join(", ")
        ))),
        _ => Err(invalid("payload must be an object")),
    }
}

pub struct AssistantGatewayApp {
    gateway: Arc<Gateway>,
}

#[doover::async_trait]
impl Application for AssistantGatewayApp {
    type Config = AssistantGatewayConfig;
    type Tags = AssistantGatewayTags;
    type Ui = NoUi<AssistantGatewayTags>;
    type Notifications = ();

    fn create(
        config: AssistantGatewayConfig,
        tags: AssistantGatewayTags,
        _ui: NoUi<AssistantGatewayTags>,
    ) -> Self {
        Self {
            gateway: Arc::new(Gateway::new(config, tags)),
        }
    }

    fn loop_target_period(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn setup(&mut self, ctx: &AppContext) -> Result<()> {
        let channel = self.gateway.config().rpc_channel;
        self.gateway.register(ctx.rpc(), &channel);
        Ok(())
    }

    async fn main_loop(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    async fn on_config_update(
        &mut self,
        _ctx: &AppContext,
        config: AssistantGatewayConfig,
    ) -> Result<()> {
        // Limits apply from the next request. The handler stays on the channel
        // it was registered on at setup, so a channel change needs a restart.
        if config.rpc_channel != self.gateway.config().rpc_channel {
            tracing::warn!("rpc_channel changed; still serving the old one until restart");
        }
        self.gateway.set_config(config);
        Ok(())
    }
}
