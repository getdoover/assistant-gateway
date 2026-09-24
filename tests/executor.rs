use std::time::Duration;

use assistant_gateway::executor::{
    build_script, run_command, CommandResult, CommandSpec, LiveOutput,
};

fn spec(command: &str) -> CommandSpec {
    CommandSpec {
        command: command.to_string(),
        timeout: Duration::from_secs(5),
        run_on_host: false,
        ..Default::default()
    }
}

async fn run(spec: CommandSpec) -> CommandResult {
    run_command(spec, LiveOutput::new(1024), std::future::pending())
        .await
        .unwrap()
}

#[test]
fn build_script_quotes_cwd() {
    assert_eq!(
        build_script("ls", Some("/tmp/a b")),
        "cd -- '/tmp/a b' && ls"
    );
    assert_eq!(build_script("ls", Some("/tmp")), "cd -- /tmp && ls");
    assert_eq!(
        build_script("ls", Some("it's")),
        r#"cd -- 'it'"'"'s' && ls"#
    );
    assert_eq!(build_script("ls", None), "ls");
}

#[tokio::test]
async fn exit_code_and_streams() {
    let r = run(spec("echo out; echo err >&2; exit 3")).await;
    assert_eq!(
        (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
        (Some(3), "out\n", "err\n")
    );
}

#[tokio::test]
async fn killed_by_signal_reports_negative_exit_code() {
    let r = run(spec("kill -9 $$")).await;
    assert_eq!(r.exit_code, Some(-9));
    assert!(!r.timed_out);
}

#[tokio::test]
async fn stdin_env_cwd() {
    let r = run(CommandSpec {
        stdin: Some("in\n".into()),
        env: vec![("FOO".into(), "bar".into())],
        cwd: Some("/".into()),
        ..spec(r#"cat; echo "$FOO"; pwd"#)
    })
    .await;
    assert_eq!(r.stdout, "in\nbar\n/\n");
}

#[tokio::test]
async fn truncation() {
    let r = run(spec("yes | head -c 100000")).await;
    assert_eq!(r.stdout.len(), 1024);
    assert!(r.stdout_truncated && !r.stderr_truncated);
    assert_eq!(r.exit_code, Some(0));
}

#[tokio::test]
async fn timeout_kills_children() {
    let r = run(CommandSpec {
        timeout: Duration::from_millis(500),
        ..spec("sleep 30 & sleep 30; wait")
    })
    .await;
    assert!(r.timed_out && r.exit_code.is_none());
    assert!(r.duration < 5.0, "took {}s", r.duration);
}

#[tokio::test]
async fn background_child_holding_pipes_is_bounded_by_timeout() {
    // sh exits at once, but the backgrounded sleep keeps stdout open.
    let r = run(CommandSpec {
        timeout: Duration::from_millis(500),
        ..spec("echo hi; sleep 30 &")
    })
    .await;
    assert!(r.timed_out && r.exit_code.is_none());
    assert_eq!(r.stdout, "hi\n");
    assert!(r.duration < 5.0, "took {}s", r.duration);
}

#[tokio::test]
async fn background_child_with_redirected_output_does_not_block() {
    let r = run(spec("sleep 30 >/dev/null 2>&1 & echo started")).await;
    assert_eq!((r.exit_code, r.stdout.as_str()), (Some(0), "started\n"));
    assert!(!r.timed_out && r.duration < 2.0, "took {}s", r.duration);
}

#[tokio::test]
async fn cancel() {
    let cancelled = tokio::time::sleep(Duration::from_millis(200));
    let r = run_command(spec("sleep 30"), LiveOutput::new(1024), cancelled)
        .await
        .unwrap();
    assert!(r.cancelled && !r.timed_out && r.exit_code.is_none());
    assert!(r.duration < 5.0);
}

#[tokio::test]
async fn live_output_fills_while_running() {
    let live = LiveOutput::new(1024);
    let task = tokio::spawn(run_command(
        spec("echo first; sleep 0.5; echo second"),
        live.clone(),
        std::future::pending(),
    ));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(live.lock().unwrap().stdout_text(), "first\n");
    assert!(!task.is_finished());
    assert_eq!(task.await.unwrap().unwrap().stdout, "first\nsecond\n");
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn run_on_host_needs_linux() {
    let err = run_command(
        CommandSpec {
            run_on_host: true,
            ..spec("true")
        },
        LiveOutput::new(1024),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

/// Needs root in a container with `--privileged --pid=host`:
///   docker run --rm --privileged --pid=host -v "$PWD":/src -w /src rust:1 \
///     cargo test --test executor -- --ignored
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore]
async fn run_on_host_joins_init_namespaces() {
    // Compared against our own namespaces too: without `--pid=host`, /proc/1
    // is this container's init and "joining" it changes nothing.
    let ours = std::fs::read_link("/proc/self/ns/mnt").unwrap();
    let check = r#"for ns in mnt uts net ipc; do
        [ "$(readlink /proc/self/ns/$ns)" = "$(readlink /proc/1/ns/$ns)" ] || echo "not in host $ns"
    done
    [ "$(readlink /proc/self/ns/mnt)" != "$CONTAINER_MNT" ] || echo "still in the container"
    echo "PATH=$PATH"; pwd"#;
    let r = run(CommandSpec {
        run_on_host: true,
        env: vec![("CONTAINER_MNT".into(), ours.to_string_lossy().into_owned())],
        ..spec(check)
    })
    .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert_eq!(
        r.stdout,
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n/\n"
    );
}
