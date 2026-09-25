//! Runs one command: subprocess, output capping, timeout/cancel kill. Knows
//! nothing about doover.

use std::future::Future;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// A clean environment for host commands, so the container's variables don't
/// leak onto the host.
pub const HOST_ENV: [(&str, &str); 3] = [
    (
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    ),
    ("HOME", "/root"),
    ("LANG", "C.UTF-8"),
];

/// The host init's IPC, UTS, network and mount namespaces, in the order
/// `nsenter` joins them. PID is already shared: the container runs with
/// `pid: host`, which is also what makes `/proc/1` the host's init.
#[cfg(target_os = "linux")]
const HOST_NAMESPACES: [&str; 4] = ["ipc", "uts", "net", "mnt"];

/// Once a command has exited or been killed, how long its output pipes get
/// to reach EOF before we stop reading them.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Output captured so far, updated as the command writes it.
#[derive(Debug, Default)]
pub struct LiveOutput {
    pub limit: usize,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Bumped on every captured chunk, so a streamer can tell what's new.
    pub version: u64,
}

impl LiveOutput {
    pub fn new(limit: usize) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            limit,
            ..Default::default()
        }))
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: f64,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CommandSpec {
    pub command: String,
    pub cwd: Option<String>,
    /// Extra variables, on top of [`HOST_ENV`] or the container's own.
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    pub timeout: Duration,
    pub run_on_host: bool,
}

/// The `sh -c` script: `command`, run from `cwd` when one is given.
pub fn build_script(command: &str, cwd: Option<&str>) -> String {
    match cwd {
        Some(cwd) if !cwd.is_empty() => format!("cd -- {} && {command}", shell_quote(cwd)),
        _ => command.to_string(),
    }
}

/// Python's `shlex.quote`.
pub fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// An argv as one `sh -c` command line, every word quoted: how the typed
/// methods run fixed tools through the same path as `exec`.
pub fn shell_join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|a| shell_quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keep draining past the limit, or a chatty command blocks on a full pipe.
async fn capture(mut pipe: impl AsyncRead + Unpin, live: Arc<Mutex<LiveOutput>>, stream: Stream) {
    let mut chunk = vec![0u8; 65536];
    loop {
        let n = match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let mut guard = live.lock().unwrap();
        let live = &mut *guard;
        let limit = live.limit;
        let (buf, truncated) = match stream {
            Stream::Stdout => (&mut live.stdout, &mut live.stdout_truncated),
            Stream::Stderr => (&mut live.stderr, &mut live.stderr_truncated),
        };
        let room = limit.saturating_sub(buf.len());
        if n > room {
            *truncated = true;
        }
        if room > 0 {
            buf.extend_from_slice(&chunk[..n.min(room)]);
            live.version += 1;
        }
    }
}

fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // ESRCH (already gone) is fine.
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// Python's `returncode`: negative signal number when killed by one.
fn exit_code(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| -s))
}

/// Open the host init's namespaces up front, in the parent, so the forked
/// child only has to `setns` into them.
#[cfg(target_os = "linux")]
fn open_host_namespaces() -> io::Result<Vec<std::os::fd::OwnedFd>> {
    HOST_NAMESPACES
        .iter()
        .map(|ns| {
            let path = format!("/proc/1/ns/{ns}");
            std::fs::File::open(&path)
                .map(Into::into)
                .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))
        })
        .collect()
}

fn build_command(spec: &CommandSpec) -> io::Result<Command> {
    // Absolute, so it resolves the same inside the container and on the host.
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(build_script(&spec.command, spec.cwd.as_deref()));
    if spec.run_on_host {
        cmd.env_clear().envs(HOST_ENV);
    }
    cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
    cmd.stdin(if spec.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    #[cfg(target_os = "linux")]
    let namespaces = if spec.run_on_host {
        open_host_namespaces()?
    } else {
        Vec::new()
    };
    #[cfg(not(target_os = "linux"))]
    if spec.run_on_host {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "running commands on the host needs Linux",
        ));
    }

    // SAFETY: only async-signal-safe syscalls between fork and exec. The
    // namespace fds are CLOEXEC, so they don't leak into the command.
    unsafe {
        cmd.pre_exec(move || {
            // Own session (and process group), so a timeout kills everything
            // the command spawned.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            for fd in &namespaces {
                use std::os::fd::AsRawFd;
                // Joining the mount namespace also moves root and cwd to the
                // host's `/`.
                if libc::setns(fd.as_raw_fd(), 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(cmd)
}

/// Run `spec`, capturing output into `live` as it arrives.
///
/// The timeout covers the whole run, including draining output: a
/// background child that keeps the pipes open is killed with the rest of the
/// process group rather than holding the call open. A timed-out or cancelled
/// command reports `exit_code: None`.
pub async fn run_command(
    spec: CommandSpec,
    live: Arc<Mutex<LiveOutput>>,
    cancelled: impl Future<Output = ()>,
) -> io::Result<CommandResult> {
    let start = Instant::now();
    let mut child = build_command(&spec)?.spawn()?;
    let pid = child.id();

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut drain = tokio::spawn({
        let live = live.clone();
        async move {
            tokio::join!(
                capture(stdout, live.clone(), Stream::Stdout),
                capture(stderr, live, Stream::Stderr),
            );
        }
    });
    if let (Some(mut pipe), Some(data)) = (child.stdin.take(), spec.stdin) {
        tokio::spawn(async move {
            // A command that doesn't read its stdin closes the pipe early.
            let _ = pipe.write_all(data.as_bytes()).await;
        });
    }

    let deadline = tokio::time::sleep(spec.timeout);
    tokio::pin!(deadline, cancelled);
    let mut timed_out = false;
    let mut was_cancelled = false;

    let status = tokio::select! {
        status = child.wait() => Some(status?),
        _ = &mut deadline => { timed_out = true; None }
        _ = &mut cancelled => { was_cancelled = true; None }
    };
    let status = match status {
        Some(status) => status,
        None => {
            kill_group(pid);
            child.wait().await?
        }
    };

    let drained = !(timed_out || was_cancelled)
        && tokio::select! {
            _ = &mut drain => true,
            _ = &mut deadline => { timed_out = true; false }
            _ = &mut cancelled => { was_cancelled = true; false }
        };
    if !drained {
        // Whatever still holds the pipes is in the command's process group,
        // unless it made a session of its own; stop waiting for that.
        kill_group(pid);
        if tokio::time::timeout(DRAIN_GRACE, &mut drain).await.is_err() {
            drain.abort();
        }
    }

    let live = live.lock().unwrap();
    Ok(CommandResult {
        exit_code: if timed_out || was_cancelled {
            None
        } else {
            exit_code(status)
        },
        stdout: live.stdout_text(),
        stderr: live.stderr_text(),
        duration: (start.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
        timed_out,
        cancelled: was_cancelled,
        stdout_truncated: live.stdout_truncated,
        stderr_truncated: live.stderr_truncated,
    })
}
