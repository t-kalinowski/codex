#![cfg(unix)]
#![allow(clippy::expect_used)]

mod support;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use support::Runner;
use support::RunnerExecutable;
use support::apply_sanitized_environment;
use support::default_lifecycle;
use support::fixture_target;
use support::launch_request;
use support::null_streams;
use support::passed_streams;
use support::wait_request;
use tempfile::TempDir;

#[test]
fn passed_streams_are_byte_transparent_independent_and_closed_by_the_runner() {
    let target = fixture_target(&["copy"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 1,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted");

    let input = b"\0binary\xffinput\r\n";
    io.stdin.write_all(input).expect("write target stdin");
    io.stdin
        .shutdown(std::net::Shutdown::Write)
        .expect("close target stdin");
    let outcome = runner.request(wait_request(/*id*/ 2));
    assert_target_exit(&outcome, /*code*/ 0);

    let mut stdout = Vec::new();
    io.stdout
        .read_to_end(&mut stdout)
        .expect("read target stdout");
    let mut stderr = Vec::new();
    io.stderr
        .read_to_end(&mut stderr)
        .expect("read target stderr");
    assert_eq!(stdout, input);
    assert_eq!(stderr, input);
}

#[test]
fn large_simultaneous_stdout_and_stderr_do_not_deadlock() {
    const LENGTH: usize = 2 * 1024 * 1024;
    let target = fixture_target(&["emit-large", &LENGTH.to_string()]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 10,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted");
    drop(io.stdin);
    let stdout = std::thread::spawn(move || read_all(io.stdout));
    let stderr = std::thread::spawn(move || read_all(io.stderr));
    let outcome = runner.request(wait_request(/*id*/ 11));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(stdout.join().expect("stdout reader").len(), LENGTH);
    assert_eq!(stderr.join().expect("stderr reader").len(), LENGTH);
}

#[test]
fn null_streams_are_independent_from_the_control_channel() {
    let target = fixture_target(&["copy"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 20,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 21)), /*code*/ 0);
}

#[test]
fn inherited_output_endpoints_reach_eof_while_runner_remains_resident() {
    let target = fixture_target(&["emit-large", "1"]);
    let (mut runner, mut stdout, mut stderr) = Runner::spawn_with_inherited_output(&target);
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "inherited" },
    });
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 22,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            streams,
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 23)), /*code*/ 0);

    let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout_bytes = Vec::new();
        let result = stdout.read_to_end(&mut stdout_bytes).map(|_| stdout_bytes);
        let _ = stdout_sender.send(result);
    });
    let (stderr_sender, stderr_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stderr_bytes = Vec::new();
        let result = stderr.read_to_end(&mut stderr_bytes).map(|_| stderr_bytes);
        let _ = stderr_sender.send(result);
    });
    let timeout = std::time::Duration::from_secs(1);
    let stdout_bytes = stdout_receiver.recv_timeout(timeout);
    let stderr_bytes = stderr_receiver.recv_timeout(timeout);
    if stdout_bytes.is_err() || stderr_bytes.is_err() {
        runner.close_control();
        let _ = runner.wait_for_exit();
    }
    let stdout_bytes = stdout_bytes
        .expect("inherited stdout did not reach EOF while runner remained resident")
        .expect("read inherited stdout to EOF");
    let stderr_bytes = stderr_bytes
        .expect("inherited stderr did not reach EOF while runner remained resident")
        .expect("read inherited stderr to EOF");
    assert_eq!(stdout_bytes, b"o");
    assert_eq!(stderr_bytes, b"e");
    assert_eq!(
        runner.request(json!({
            "type": "status",
            "id": 24,
            "protocol_version": support::PROTOCOL_VERSION,
        }))["status"]["phase"],
        "retired"
    );
}

#[test]
fn inherited_stdin_is_byte_transparent_and_reaches_target_eof() {
    let target = fixture_target(&["copy"]);
    let (mut runner, mut stdin, mut stdout) =
        Runner::spawn_with_inherited_stdin_and_stdout(&target);
    let streams = json!({
        "stdin": { "mode": "inherited" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "null" },
    });
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 241,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            streams,
        ))["type"],
        "launch_accepted"
    );
    let input = b"\0inherited-binary\xff\r\n";
    stdin
        .write_all(input)
        .expect("write inherited target stdin");
    stdin
        .shutdown(std::net::Shutdown::Write)
        .expect("close inherited target stdin");
    let mut output = Vec::new();
    stdout
        .read_to_end(&mut output)
        .expect("read inherited target stdout");
    assert_eq!(output, input);
    assert_target_exit(&runner.request(wait_request(/*id*/ 242)), /*code*/ 0);
}

#[test]
fn inherited_stream_cannot_alias_the_private_control_descriptor() {
    let directory = TempDir::new().expect("target directory");
    let marker = directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let mut runner = Runner::spawn_with_control_on_stdout(&target);
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(default_launch(
        /*id*/ 25,
        directory.path(),
        json!([{
            "path": directory.path(),
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn duplicated_control_endpoint_cannot_be_claimed_as_a_passed_stream() {
    let target = fixture_target(&["exit", "0"]);
    let mut runner = Runner::spawn_with_duplicated_control_as_passed_stream(&target);
    let mut unexpected = [0_u8; 1];
    assert_eq!(
        runner
            .control
            .read(&mut unexpected)
            .expect("read closed control endpoint"),
        0
    );
    assert_eq!(runner.wait_for_exit().code(), Some(2));
}

#[test]
fn duplicated_runner_standard_stream_cannot_be_claimed_as_a_passed_stream() {
    let target = fixture_target(&["exit", "0"]);
    let mut runner = Runner::spawn_with_duplicated_stdout_as_passed_stream(&target);
    runner
        .control
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .expect("set control read timeout");
    let mut unexpected = [0_u8; 1];
    assert_eq!(
        runner
            .control
            .read(&mut unexpected)
            .expect("read closed control endpoint"),
        0
    );
    assert_eq!(runner.wait_for_exit().code(), Some(2));
}

#[test]
fn duplicated_control_endpoint_cannot_be_inherited_by_the_target() {
    let directory = TempDir::new().expect("target directory");
    let marker = directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let mut runner = Runner::spawn_with_duplicated_control_as_inherited_stdout(&target);
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(default_launch(
        /*id*/ 244,
        directory.path(),
        json!([{
            "path": directory.path(),
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn unavailable_inherited_stream_is_rejected_before_target_start() {
    let directory = TempDir::new().expect("target directory");
    let marker = directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let mut runner = Runner::spawn_with_closed_standard_stream(&target, libc::STDOUT_FILENO);
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(default_launch(
        /*id*/ 243,
        directory.path(),
        json!([{
            "path": directory.path(),
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn passed_stream_cannot_alias_an_inherited_standard_descriptor() {
    let directory = TempDir::new().expect("target directory");
    let marker = directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let (mut runner, _stdout, _stderr) = Runner::spawn_with_inherited_output(&target);
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "passed_handle", "handle": libc::STDOUT_FILENO },
    });
    let response = runner.request(default_launch(
        /*id*/ 26,
        directory.path(),
        json!([{
            "path": directory.path(),
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn invalid_passed_stream_is_rejected_before_policy_preparation() {
    let directory = TempDir::new().expect("target directory");
    let missing = directory.path().join("missing");
    let target = fixture_target(&["exit", "0"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": i32::MAX as u64 },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(default_launch(
        /*id*/ 27,
        directory.path(),
        json!([{
            "path": missing,
            "access": "read",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["phase"], "validation");
    assert_eq!(response["error"]["target_started"], false);
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("was not declared at runner bootstrap"))
    );
}

#[test]
fn invalid_bootstrap_stream_descriptor_fails_before_protocol_startup() {
    let state = TempDir::new().expect("state directory");
    let target = fixture_target(&["exit", "0"]);
    let (control, child_control) = UnixStream::pair().expect("control socket pair");
    let child_control_fd = child_control.as_raw_fd();
    let executable = RunnerExecutable::packaged();
    let mut command = std::process::Command::new(executable.path());
    command
        .arg("--state-dir")
        .arg(state.path())
        .arg("--control-fd")
        .arg("198")
        .arg("--stream-fd")
        .arg(i32::MAX.to_string())
        .arg("--")
        .args(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    apply_sanitized_environment(&mut command, &[]);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_control_fd, 198) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("spawn runner");
    drop((control, child_control));
    let output = child.wait_with_output().expect("wait for runner");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .expect("Unicode runner diagnostic")
            .contains("invalid native bootstrap endpoint")
    );
}

#[test]
fn duplicate_bootstrap_stream_descriptor_fails_before_protocol_startup() {
    const PASSED_FD: i32 = 190;

    let state = TempDir::new().expect("state directory");
    let target = fixture_target(&["exit", "0"]);
    let (control, child_control) = UnixStream::pair().expect("control socket pair");
    let (passed_owner, child_passed) = UnixStream::pair().expect("passed stream pair");
    let child_control_fd = child_control.as_raw_fd();
    let child_passed_fd = child_passed.as_raw_fd();
    let executable = RunnerExecutable::packaged();
    let mut command = std::process::Command::new(executable.path());
    command
        .arg("--state-dir")
        .arg(state.path())
        .arg("--control-fd")
        .arg("198")
        .arg("--stream-fd")
        .arg(PASSED_FD.to_string())
        .arg("--stream-fd")
        .arg(PASSED_FD.to_string())
        .arg("--")
        .args(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    apply_sanitized_environment(&mut command, &[]);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_control_fd, 198) == -1
                || libc::dup2(child_passed_fd, PASSED_FD) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("spawn runner");
    drop((control, child_control, passed_owner, child_passed));
    let output = child.wait_with_output().expect("wait for runner");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .expect("Unicode runner diagnostic")
            .contains("distinct descriptor")
    );
}

#[test]
fn runner_owned_descriptor_cannot_be_selected_as_a_passed_stream() {
    let directory = TempDir::new().expect("target directory");
    let marker = directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let streams = json!({
        "stdin": { "mode": "passed_handle", "handle": libc::STDIN_FILENO },
        "stdout": { "mode": "null" },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(default_launch(
        /*id*/ 28,
        directory.path(),
        json!([{
            "path": directory.path(),
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn post_commit_exec_failure_consumes_the_target_generation() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().expect("target directory");
    let target = directory.path().join("missing-interpreter");
    std::fs::write(&target, b"#!/definitely/missing/interpreter\n")
        .expect("write target executable");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .expect("make target executable");
    let (mut runner, _) = Runner::spawn(&[target.into_os_string()], &[], /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 21,
        directory.path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "launch_failed", "{response}");
    assert_eq!(response["error"]["phase"], "launch");
    assert_eq!(response["error"]["target_started"], true);

    let second = runner.request(default_launch(
        /*id*/ 22,
        directory.path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(second["type"], "error");
    assert_eq!(second["error"]["code"], "invalid_state");

    let outcome = runner.request(wait_request(/*id*/ 23));
    assert_eq!(outcome["type"], "final");
    assert_eq!(outcome["outcome"]["target"], Value::Null);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert!(
        outcome["outcome"]["infrastructure"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("launch bridge")),
        "{outcome}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_launch_does_not_expose_a_namespace_local_root_process_id() {
    let target = fixture_target(&["exit", "0"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 25,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));

    assert_eq!(response["type"], "launch_accepted");
    assert_eq!(response["root_process_id"], Value::Null);
    assert_target_exit(&runner.request(wait_request(/*id*/ 26)), /*code*/ 0);
}

#[test]
fn target_exit_125_remains_a_target_outcome() {
    let target = fixture_target(&["exit", "125"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 27,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );

    let outcome = runner.request(wait_request(/*id*/ 28));
    assert_target_exit(&outcome, /*code*/ 125);
    assert_eq!(outcome["outcome"]["infrastructure"]["error"], Value::Null);
}

#[test]
fn launch_bridge_failure_is_infrastructure_not_a_target_outcome() {
    let target = fixture_target(&["sleep", "10000"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 29,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted");

    #[cfg(target_os = "linux")]
    let bridge_process_id = sandbox_process_group_leader(runner.process_id());
    #[cfg(target_os = "macos")]
    let bridge_process_id = parent_process_id(
        response["root_process_id"]
            .as_u64()
            .expect("root process id") as i32,
    );
    assert_eq!(unsafe { libc::kill(bridge_process_id, libc::SIGUSR1) }, 0);
    let outcome = runner.request(wait_request(/*id*/ 30));
    assert_eq!(outcome["type"], "final");
    assert_eq!(outcome["outcome"]["target"], Value::Null);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert!(
        outcome["outcome"]["infrastructure"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("launch bridge")),
        "{outcome}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn missing_packaged_bwrap_is_unavailable_and_fails_before_target_start() {
    let target_directory = TempDir::new().expect("target directory");
    let marker = target_directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let (mut runner, _) = Runner::spawn_without_companion(&target, &[], /*with_io*/ false);

    let capabilities = runner.request(json!({
        "type": "discover",
        "id": 22,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    assert_eq!(capabilities["type"], "capabilities");
    assert_eq!(
        capabilities["capabilities"]["setup"]["state"],
        "unavailable"
    );
    assert_eq!(
        capabilities["capabilities"]["required_companions"],
        json!([{
            "name": "bubblewrap",
            "relative_path": "codex-resources/bwrap",
            "required": true,
        }])
    );

    let response = runner.request(default_launch(
        /*id*/ 23,
        target_directory.path(),
        json!([{ "path": target_directory.path(), "access": "write", "missing": "error" }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "companion_missing");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn incompatible_packaged_bwrap_is_unavailable_and_fails_before_target_start() {
    let target_directory = TempDir::new().expect("target directory");
    let marker = target_directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let (mut runner, _) =
        Runner::spawn_with_incompatible_companion(&target, &[], /*with_io*/ false);

    let capabilities = runner.request(json!({
        "type": "discover",
        "id": 24,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    assert_eq!(capabilities["type"], "capabilities");
    assert_eq!(
        capabilities["capabilities"]["setup"]["state"],
        "unavailable"
    );
    assert!(
        capabilities["capabilities"]["setup"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("incompatible")),
        "{capabilities}"
    );

    let response = runner.request(default_launch(
        /*id*/ 25,
        target_directory.path(),
        json!([{ "path": target_directory.path(), "access": "write", "missing": "error" }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "companion_missing");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn packaged_bwrap_compatibility_is_bounded_and_exact() {
    let exact = format!(
        "mcp-console-sandbox-bwrap/2 codex/{}\\n",
        env!("CARGO_PKG_VERSION")
    );
    let cases = [
        (
            "extra-stdout",
            format!("printf '%s' '{exact}extra'"),
            "incompatible",
        ),
        (
            "stderr",
            format!("printf '%s' '{exact}'; printf '%s' 'diagnostic' >&2"),
            "incompatible",
        ),
        (
            "oversized",
            format!("printf '%s' '{}'", "x".repeat(1025)),
            "exceeded 1024 bytes",
        ),
    ];
    let target = fixture_target(&["exit", "0"]);
    for (name, body, expected_detail) in cases {
        let directory = TempDir::new().expect("companion fixture directory");
        let companion = write_linux_companion(directory.path(), name, &body);
        let (mut runner, _) =
            Runner::spawn_with_companion(&target, &companion, /*with_io*/ false);
        let capabilities = runner.request(json!({
            "type": "discover",
            "id": 26,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        assert_eq!(
            capabilities["capabilities"]["setup"]["state"], "unavailable",
            "{name}: {capabilities}"
        );
        assert!(
            capabilities["capabilities"]["setup"]["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(expected_detail)),
            "{name}: {capabilities}"
        );
    }

    let directory = TempDir::new().expect("timeout companion fixture directory");
    let companion = write_linux_companion(directory.path(), "timeout", "/bin/sleep 30");
    let (mut runner, _) = Runner::spawn_with_companion(&target, &companion, /*with_io*/ false);
    let started = std::time::Instant::now();
    let capabilities = runner.request(json!({
        "type": "discover",
        "id": 27,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(
        capabilities["capabilities"]["setup"]["state"],
        "unavailable"
    );
    assert!(
        capabilities["capabilities"]["setup"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("timed out")),
        "{capabilities}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn packaged_bwrap_compatibility_retires_a_pipe_holding_descendant() {
    let exact = format!(
        "mcp-console-sandbox-bwrap/2 codex/{}\\n",
        env!("CARGO_PKG_VERSION")
    );
    let body = format!(
        "/bin/sh -c 'printf %s \"$$\" > \"$1\"; /bin/sleep 30' child \"$0.pid\" &\nprintf '%s' '{exact}'"
    );
    let directory = TempDir::new().expect("descendant companion fixture directory");
    let companion = write_linux_companion(directory.path(), "descendant", &body);
    let target = fixture_target(&["exit", "0"]);
    let (mut runner, _) = Runner::spawn_with_companion(&target, &companion, /*with_io*/ false);
    let packaged_companion = runner
        .runner_path()
        .parent()
        .expect("runner directory")
        .join("codex-resources/bwrap");
    let descendant_process_id_path = packaged_companion.with_extension("pid");
    let capabilities = runner.request(json!({
        "type": "discover",
        "id": 28,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    assert_eq!(
        capabilities["capabilities"]["setup"]["state"],
        "unavailable"
    );
    assert!(
        capabilities["capabilities"]["setup"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("timed out")),
        "{capabilities}"
    );
    let descendant_process_id = std::fs::read_to_string(&descendant_process_id_path)
        .expect("read compatibility descendant process ID")
        .parse::<i32>()
        .expect("compatibility descendant process ID");
    wait_for_linux_process_exit(descendant_process_id, std::time::Duration::from_secs(2));
}

#[test]
fn filesystem_write_deny_and_nested_read_precedence_are_enforced() {
    let root = TempDir::new().expect("policy root");
    let denied = root.path().join("denied");
    let read_only = root.path().join("read-only");
    std::fs::create_dir_all(&denied).expect("denied directory");
    std::fs::create_dir_all(&read_only).expect("read-only directory");
    let rules = json!([
        { "path": root.path(), "access": "write", "missing": "error" },
        { "path": denied, "access": "deny", "missing": "error" },
        { "path": read_only, "access": "read", "missing": "error" },
    ]);

    assert_write_outcome(
        root.path().join("allowed"),
        root.path(),
        rules.clone(),
        /*exit_code*/ 0,
    );
    assert_write_outcome(
        denied.join("blocked"),
        root.path(),
        rules.clone(),
        /*exit_code*/ 73,
    );
    assert_write_outcome(
        read_only.join("blocked"),
        root.path(),
        rules,
        /*exit_code*/ 73,
    );
}

#[test]
fn filesystem_deny_covering_target_executable_fails_before_launch() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().expect("policy root");
    let executable_directory = root.path().join("executables");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&executable_directory).expect("create executable directory");
    std::fs::create_dir(&workspace).expect("create workspace");
    let target = executable_directory.join("target");
    let fixture = fixture_target(&["exit", "0"]).remove(0);
    std::fs::copy(&fixture, &target).expect("copy target executable");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .expect("make target executable");
    let marker = workspace.join("target-started");
    let command = vec![
        target.into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let (mut runner, _) = Runner::spawn(&command, &[], /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 29,
        &workspace,
        json!([
            {
                "path": workspace,
                "access": "write",
                "missing": "error",
            },
            {
                "path": executable_directory,
                "access": "deny",
                "missing": "error",
            },
        ]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn non_unicode_target_executable_path_is_rejected_before_launch() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().expect("policy root");
    let target = root.path().join(OsString::from_vec(vec![
        b't', b'a', b'r', b'g', b'e', b't', b'-', 0xff,
    ]));
    let fixture = fixture_target(&["exit", "0"]).remove(0);
    std::fs::copy(&fixture, &target).expect("copy target executable");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .expect("make target executable");
    let marker = root.path().join("target-started");
    let command = vec![
        target.into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let (mut runner, _) = Runner::spawn(&command, &[], /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 30,
        root.path(),
        json!([{
            "path": root.path(),
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["phase"], "validation");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn filesystem_read_roots_and_denials_do_not_widen_platform_minimal() {
    let current_directory = std::env::current_dir().expect("current directory");
    let private_root = tempfile::Builder::new()
        .prefix("mcp-console-sandbox-read-")
        .tempdir_in(&current_directory)
        .expect("private read root");
    let secret = private_root.path().join("secret");
    std::fs::write(&secret, b"secret").expect("write secret fixture");
    let target = fixture_target(&["reopen", secret.to_str().expect("Unicode secret path")]);

    assert_read_outcome(
        &target,
        "platform_minimal",
        json!([]),
        /*exit_code*/ 73,
    );
    assert_read_outcome(
        &target,
        "platform_minimal",
        json!([{
            "path": private_root.path(),
            "access": "read",
            "missing": "error",
        }]),
        /*exit_code*/ 0,
    );
    let denied_rules = json!([
        { "path": private_root.path(), "access": "read", "missing": "error" },
        { "path": secret, "access": "deny", "missing": "error" },
    ]);
    assert_read_outcome(
        &target,
        "platform_minimal",
        denied_rules.clone(),
        /*exit_code*/ 73,
    );
    assert_read_outcome(
        &target,
        "host_read_only",
        denied_rules,
        /*exit_code*/ 73,
    );
}

#[test]
fn missing_path_behavior_is_explicit_and_fail_closed() {
    let root = TempDir::new().expect("policy root");
    let missing = root.path().join("missing");
    let target = fixture_target(&["exit", "0"]);

    let (mut error_runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let response = error_runner.request(default_launch(
        /*id*/ 30,
        root.path(),
        json!([{ "path": missing, "access": "read", "missing": "error" }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);

    let (mut ignore_runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        ignore_runner.request(default_launch(
            /*id*/ 31,
            root.path(),
            json!([{ "path": missing, "access": "read", "missing": "ignore" }]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(
        &ignore_runner.request(wait_request(/*id*/ 32)),
        /*code*/ 0,
    );
}

#[test]
fn runner_state_is_protected_from_broad_writes_and_specific_overrides() {
    let target = fixture_target(&["exit", "0"]);
    let (mut override_runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let state = override_runner.state_dir().to_path_buf();
    let response = override_runner.request(default_launch(
        /*id*/ 40,
        state.parent().expect("state parent"),
        json!([{ "path": state, "access": "write", "missing": "error" }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);

    let state_dir = TempDir::new().expect("state directory");
    let actual_state = state_dir.path().to_path_buf();
    let target = fixture_target(&[
        "write",
        actual_state
            .join("target-write")
            .to_str()
            .expect("Unicode state path"),
        "blocked",
    ]);
    let (mut runner, _) = Runner::spawn_with_state(&target, &[], /*with_io*/ false, state_dir);
    let state = runner.state_dir().to_path_buf();
    let parent = state.parent().expect("state parent");
    let response = runner.request(default_launch(
        /*id*/ 41,
        parent,
        json!([{ "path": parent, "access": "write", "missing": "error" }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted");
    assert_target_exit(&runner.request(wait_request(/*id*/ 42)), /*code*/ 73);
    assert!(!state.join("target-write").exists());
}

#[test]
fn runner_and_companion_resources_reject_authority_overrides() {
    let target = fixture_target(&["exit", "0"]);
    for (resource, access) in [
        ("runner", "write"),
        ("companions", "write"),
        ("companion_child", "write"),
        ("runner_parent", "deny"),
    ] {
        let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
        let companion_directory = runner
            .runner_path()
            .parent()
            .expect("runner parent")
            .join("codex-resources");
        let path = match resource {
            "runner" => runner.runner_path().to_path_buf(),
            "runner_parent" => runner
                .runner_path()
                .parent()
                .expect("runner parent")
                .to_path_buf(),
            "companions" => companion_directory,
            "companion_child" => {
                let Ok(mut entries) = std::fs::read_dir(&companion_directory) else {
                    continue;
                };
                let Some(Ok(entry)) = entries.next() else {
                    continue;
                };
                entry.path()
            }
            _ => unreachable!(),
        };
        let response = runner.request(default_launch(
            /*id*/ 43,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([{ "path": path, "access": access, "missing": "error" }]),
            json!({ "mode": "denied" }),
            null_streams(),
        ));
        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "invalid_request");
        assert_eq!(response["error"]["target_started"], false);
    }
}

#[test]
fn working_directory_and_both_filesystem_bases_launch() {
    for (index, base) in ["host_read_only", "platform_minimal"]
        .into_iter()
        .enumerate()
    {
        let cwd = TempDir::new().expect("working directory");
        let target = fixture_target(&["cwd"]);
        let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
        let mut io = io.expect("target streams");
        let rules = if base == "platform_minimal" {
            json!([{ "path": cwd.path(), "access": "read", "missing": "error" }])
        } else {
            json!([])
        };
        let response = runner.request(launch_request(
            index as u64 + 50,
            cwd.path(),
            base,
            rules,
            json!({ "mode": "denied" }),
            passed_streams(),
            default_lifecycle(),
        ));
        assert_eq!(response["type"], "launch_accepted", "{response}");
        assert_target_exit(
            &runner.request(wait_request(index as u64 + 60)),
            /*code*/ 0,
        );
        let output = read_all(&mut io.stdout);
        let values = decode_values(&output);
        let canonical_cwd = cwd
            .path()
            .canonicalize()
            .expect("canonical working directory");
        assert_eq!(values, vec![canonical_cwd.as_os_str().as_encoded_bytes()]);
    }
}

#[test]
fn host_read_only_denies_host_writes_without_creating_the_file() {
    let host_directory = TempDir::new().expect("host directory");
    let attempted_write = host_directory.path().join("must-not-exist");
    let target = fixture_target(&[
        "write",
        attempted_write.to_str().expect("Unicode write path"),
        "blocked",
    ]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 69,
            host_directory.path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 691)), /*code*/ 73);
    assert!(!attempted_write.exists());
}

#[test]
fn denied_and_unrestricted_network_modes_do_not_fallback() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
    let port = listener
        .local_addr()
        .expect("listener address")
        .port()
        .to_string();
    let target = fixture_target(&["connect", "127.0.0.1", &port]);

    let (mut denied_runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        denied_runner.request(default_launch(
            /*id*/ 70,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(
        &denied_runner.request(wait_request(/*id*/ 71)),
        /*code*/ 73,
    );

    let accept = std::thread::spawn(move || listener.accept().expect("accept connection"));
    let (mut unrestricted_runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        unrestricted_runner.request(default_launch(
            /*id*/ 72,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "unrestricted" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(
        &unrestricted_runner.request(wait_request(/*id*/ 73)),
        /*code*/ 0,
    );
    accept.join().expect("accept thread");
}

#[test]
fn managed_proxy_enforces_its_reported_local_binding_behavior() {
    let target = fixture_target(&["bind", "127.0.0.1", "0"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let capabilities = runner.request(json!({
        "type": "discover",
        "id": 74,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    assert_eq!(
        capabilities["capabilities"]["network"]["local_binding_policy"],
        cfg!(target_os = "macos")
    );
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 75,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            managed_network(json!([]), json!([])),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    #[cfg(target_os = "macos")]
    let expected_exit = 73;
    #[cfg(target_os = "linux")]
    let expected_exit = 0;
    assert_target_exit(&runner.request(wait_request(/*id*/ 76)), expected_exit);
}

#[cfg(target_os = "linux")]
#[test]
fn managed_proxy_preserves_unrelated_non_unicode_target_environment() {
    let native_value = vec![b'n', b'a', b't', b'i', b'v', b'e', 0xff];
    let target = fixture_target(&["environment", "NATIVE_VALUE"]);
    let environment = [(
        OsString::from("NATIVE_VALUE"),
        OsString::from_vec(native_value.clone()),
    )];
    let (mut runner, io) =
        Runner::spawn_with_native_environment(&target, &environment, /*with_io*/ true);
    let mut io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 77,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            managed_network(json!([]), json!([])),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    drop(io.stdin);

    assert_target_exit(&runner.request(wait_request(/*id*/ 78)), /*code*/ 0);
    assert_eq!(
        decode_values(&read_all(&mut io.stdout)),
        vec![native_value.as_slice()]
    );
}

#[test]
fn non_unicode_loader_variable_name_is_rejected_before_target_launch() {
    let root = TempDir::new().expect("target directory");
    let marker = root.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let mut loader_key = if cfg!(target_os = "macos") {
        b"DYLD_".to_vec()
    } else {
        b"LD_".to_vec()
    };
    loader_key.push(0xff);
    let environment = [(
        OsString::from_vec(loader_key),
        OsString::from("private-loader-value"),
    )];
    let (mut runner, _) =
        Runner::spawn_with_native_environment(&target, &environment, /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 781,
        root.path(),
        json!([{ "path": root.path(), "access": "write", "missing": "error" }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(
        !response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("private-loader-value")
    );
    assert!(!marker.exists());
}

#[test]
fn managed_proxy_injects_the_documented_environment_and_removes_private_values() {
    const HTTP_PROXY_KEYS: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "YARN_HTTP_PROXY",
        "YARN_HTTPS_PROXY",
        "npm_config_http_proxy",
        "npm_config_https_proxy",
        "npm_config_proxy",
        "NPM_CONFIG_HTTP_PROXY",
        "NPM_CONFIG_HTTPS_PROXY",
        "NPM_CONFIG_PROXY",
        "BUNDLE_HTTP_PROXY",
        "BUNDLE_HTTPS_PROXY",
        "PIP_PROXY",
        "DOCKER_HTTP_PROXY",
        "DOCKER_HTTPS_PROXY",
        "WS_PROXY",
        "WSS_PROXY",
        "ws_proxy",
        "wss_proxy",
    ];
    const NO_PROXY_KEYS: &[&str] = &[
        "NO_PROXY",
        "no_proxy",
        "npm_config_noproxy",
        "NPM_CONFIG_NOPROXY",
        "YARN_NO_PROXY",
        "BUNDLE_NO_PROXY",
    ];
    const OTHER_KEYS: &[&str] = &[
        "CODEX_NETWORK_PROXY_ACTIVE",
        "CODEX_NETWORK_ALLOW_LOCAL_BINDING",
        "ELECTRON_GET_USE_PROXY",
        "NODE_USE_ENV_PROXY",
        "ALL_PROXY",
        "all_proxy",
        "FTP_PROXY",
        "ftp_proxy",
        "SSL_CERT_FILE",
        "CODEX_CA_CERTIFICATE",
        "CODEX_WINDOWS_SANDBOX_PROXY_PORTS",
        "CODEX_HOME",
        "MCP_CONSOLE_SANDBOX_CONTROL",
        "CARGO_BIN_EXE_PRIVATE_HELPER",
    ];
    let keys = HTTP_PROXY_KEYS
        .iter()
        .chain(NO_PROXY_KEYS)
        .chain(OTHER_KEYS)
        .copied()
        .collect::<Vec<_>>();
    let mut arguments = vec!["environment"];
    arguments.extend(keys.iter().copied());
    let target = fixture_target(&arguments);
    let environment = [
        ("HTTP_PROXY", "http://stale.invalid:1"),
        ("NO_PROXY", "*"),
        ("ALL_PROXY", "socks5h://stale.invalid:2"),
        ("SSL_CERT_FILE", "/trusted/caller-ca.pem"),
        ("CODEX_CA_CERTIFICATE", "/private/managed-ca.pem"),
        ("CODEX_WINDOWS_SANDBOX_PROXY_PORTS", "1234"),
        ("CODEX_HOME", "/private/codex-home"),
        ("MCP_CONSOLE_SANDBOX_CONTROL", "private"),
        ("CARGO_BIN_EXE_PRIVATE_HELPER", "/private/helper"),
    ];
    let (mut runner, io) = Runner::spawn(&target, &environment, /*with_io*/ true);
    let mut io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 79,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            managed_network(json!([]), json!([])),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    drop(io.stdin);
    assert_target_exit(&runner.request(wait_request(/*id*/ 791)), /*code*/ 0);
    let values = decode_optional_values(&read_all(&mut io.stdout));
    let environment = keys
        .iter()
        .copied()
        .zip(values)
        .collect::<std::collections::BTreeMap<_, _>>();
    let http_proxy = environment["HTTP_PROXY"]
        .as_deref()
        .expect("managed HTTP proxy");
    assert!(http_proxy.starts_with(b"http://"));
    for key in HTTP_PROXY_KEYS {
        assert_eq!(environment[key].as_deref(), Some(http_proxy), "{key}");
    }
    let expected_no_proxy = if cfg!(target_os = "linux") {
        b"localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16".as_slice()
    } else {
        b"".as_slice()
    };
    for key in NO_PROXY_KEYS {
        assert_eq!(
            environment[key].as_deref(),
            Some(expected_no_proxy),
            "{key}"
        );
    }
    assert_eq!(
        environment["CODEX_NETWORK_PROXY_ACTIVE"].as_deref(),
        Some(b"1".as_slice())
    );
    assert_eq!(
        environment["CODEX_NETWORK_ALLOW_LOCAL_BINDING"].as_deref(),
        Some(if cfg!(target_os = "linux") {
            b"1".as_slice()
        } else {
            b"0".as_slice()
        })
    );
    assert_eq!(
        environment["ELECTRON_GET_USE_PROXY"].as_deref(),
        Some(b"true".as_slice())
    );
    assert_eq!(
        environment["NODE_USE_ENV_PROXY"].as_deref(),
        Some(b"1".as_slice())
    );
    for key in ["ALL_PROXY", "all_proxy", "FTP_PROXY", "ftp_proxy"] {
        assert_eq!(environment[key].as_deref(), Some(http_proxy), "{key}");
    }
    assert_eq!(
        environment["SSL_CERT_FILE"].as_deref(),
        Some(b"/trusted/caller-ca.pem".as_slice())
    );
    for key in [
        "CODEX_CA_CERTIFICATE",
        "CODEX_WINDOWS_SANDBOX_PROXY_PORTS",
        "CODEX_HOME",
        "MCP_CONSOLE_SANDBOX_CONTROL",
        "CARGO_BIN_EXE_PRIVATE_HELPER",
    ] {
        assert_eq!(environment[key], None, "{key}");
    }
}

#[test]
fn managed_socks_changes_only_the_documented_all_and_ftp_proxy_aliases() {
    let keys = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "all_proxy",
        "FTP_PROXY",
        "ftp_proxy",
    ];
    let mut arguments = vec!["environment"];
    arguments.extend(keys);
    let target = fixture_target(&arguments);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut network = managed_network(json!([]), json!([]));
    network["socks"] = json!(true);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 792,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            network,
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    drop(io.stdin);
    assert_target_exit(&runner.request(wait_request(/*id*/ 793)), /*code*/ 0);
    let values = decode_optional_values(&read_all(&mut io.stdout));
    let environment = keys
        .into_iter()
        .zip(values)
        .collect::<std::collections::BTreeMap<_, _>>();
    let http_proxy = environment["HTTP_PROXY"]
        .as_deref()
        .expect("managed HTTP proxy");
    assert!(http_proxy.starts_with(b"http://"));
    assert_eq!(environment["HTTPS_PROXY"].as_deref(), Some(http_proxy));
    let socks_proxy = environment["ALL_PROXY"]
        .as_deref()
        .expect("managed SOCKS proxy");
    assert!(socks_proxy.starts_with(b"socks5h://"));
    for key in ["all_proxy", "FTP_PROXY", "ftp_proxy"] {
        assert_eq!(environment[key].as_deref(), Some(socks_proxy), "{key}");
    }
}

#[test]
fn managed_proxy_allows_a_domain_while_confining_direct_egress() {
    let origin = TcpListener::bind("127.0.0.1:0").expect("origin listener");
    let port = origin
        .local_addr()
        .expect("origin address")
        .port()
        .to_string();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().expect("accept proxy request");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("read proxy request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
            .expect("write origin response");
    });
    let target = fixture_target(&["http-get", "localhost", &port]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let managed = managed_network(json!(["localhost"]), json!([]));
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 80,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            managed,
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    drop(io.stdin);
    assert_target_exit(&runner.request(wait_request(/*id*/ 81)), /*code*/ 0);
    let stdout = read_all(&mut io.stdout);
    assert!(stdout.windows(6).any(|bytes| bytes == b"200 OK"));
    server.join().expect("origin server");

    let listener = TcpListener::bind("127.0.0.1:0").expect("direct listener");
    listener
        .set_nonblocking(true)
        .expect("make direct listener nonblocking");
    let direct_port = listener
        .local_addr()
        .expect("listener address")
        .port()
        .to_string();
    let target = fixture_target(&["connect", "127.0.0.1", &direct_port]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 82,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            managed_network(json!(["localhost"]), json!([])),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 83)), /*code*/ 73);
    let error = listener
        .accept()
        .expect_err("managed target direct egress must not reach the host listener");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn managed_proxy_applies_domain_policy_to_redirected_requests() {
    let redirect_origin = TcpListener::bind("127.0.0.1:0").expect("redirect origin");
    let final_origin = TcpListener::bind("127.0.0.1:0").expect("final origin");
    let redirect_port = redirect_origin
        .local_addr()
        .expect("redirect address")
        .port()
        .to_string();
    let final_port = final_origin
        .local_addr()
        .expect("final address")
        .port()
        .to_string();
    let location = format!("http://127.0.0.1:{final_port}/final");
    let redirect_server = std::thread::spawn(move || {
        let (mut stream, _) = redirect_origin.accept().expect("accept redirect request");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("read redirect request");
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
        )
        .expect("write redirect response");
    });
    final_origin
        .set_nonblocking(true)
        .expect("make denied redirect destination nonblocking");
    let target = fixture_target(&["http-follow", "localhost", &redirect_port]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 840,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            managed_network(json!(["localhost"]), json!(["127.0.0.1"])),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 841)), /*code*/ 73);
    redirect_server.join().expect("redirect server");
    let error = final_origin
        .accept()
        .expect_err("denied redirect destination must not be reached");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn managed_proxy_limited_access_allows_get_and_blocks_post_before_origin() {
    let get_origin = TcpListener::bind("127.0.0.1:0").expect("GET origin listener");
    let get_port = get_origin
        .local_addr()
        .expect("GET origin address")
        .port()
        .to_string();
    let get_server = std::thread::spawn(move || {
        let (mut stream, _) = get_origin.accept().expect("accept GET proxy request");
        let mut request = [0_u8; 4096];
        let length = stream.read(&mut request).expect("read GET proxy request");
        assert!(request[..length].starts_with(b"GET "));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
            .expect("write GET origin response");
    });
    let mut limited = managed_network(json!(["localhost"]), json!([]));
    limited["access"] = json!("limited");
    let get_target = fixture_target(&["http-request", "GET", "localhost", &get_port]);
    let (mut get_runner, _) = Runner::spawn(&get_target, &[], /*with_io*/ false);
    assert_eq!(
        get_runner.request(default_launch(
            /*id*/ 842,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            limited.clone(),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(
        &get_runner.request(wait_request(/*id*/ 843)),
        /*code*/ 0,
    );
    get_server.join().expect("GET origin server");

    let post_origin = TcpListener::bind("127.0.0.1:0").expect("POST origin listener");
    post_origin
        .set_nonblocking(true)
        .expect("make POST origin nonblocking");
    let post_port = post_origin
        .local_addr()
        .expect("POST origin address")
        .port()
        .to_string();
    let post_target = fixture_target(&["http-request", "POST", "localhost", &post_port]);
    let (mut post_runner, _) = Runner::spawn(&post_target, &[], /*with_io*/ false);
    assert_eq!(
        post_runner.request(default_launch(
            /*id*/ 844,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            limited,
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(
        &post_runner.request(wait_request(/*id*/ 845)),
        /*code*/ 73,
    );
    let error = post_origin
        .accept()
        .expect_err("limited POST must not reach the allowlisted origin");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn managed_proxy_enforces_domain_denials_and_supports_socks() {
    let denied_target = fixture_target(&["http-get", "localhost", "9"]);
    let (mut denied_runner, _) = Runner::spawn(&denied_target, &[], /*with_io*/ false);
    let response = denied_runner.request(default_launch(
        /*id*/ 84,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        managed_network(json!(["localhost"]), json!(["localhost"])),
        null_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted");
    assert_target_exit(
        &denied_runner.request(wait_request(/*id*/ 85)),
        /*code*/ 73,
    );

    let origin = TcpListener::bind("127.0.0.1:0").expect("SOCKS origin");
    let port = origin
        .local_addr()
        .expect("origin address")
        .port()
        .to_string();
    let accept = std::thread::spawn(move || origin.accept().expect("accept SOCKS connection"));
    let target = fixture_target(&["socks-connect", "localhost", &port]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let mut network = managed_network(json!(["localhost"]), json!([]));
    network["socks"] = json!(true);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 86,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            network,
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 87)), /*code*/ 0);
    accept.join().expect("SOCKS origin thread");
}

#[cfg(target_os = "macos")]
#[test]
fn managed_socks_preserves_a_non_unicode_git_ssh_command() {
    let native_value = vec![b't', b'r', b'u', b's', b't', b'e', b'd', b'-', 0xff];
    let target = fixture_target(&["environment", "GIT_SSH_COMMAND"]);
    let environment = [(
        OsString::from("GIT_SSH_COMMAND"),
        OsString::from_vec(native_value.clone()),
    )];
    let (mut runner, io) =
        Runner::spawn_with_native_environment(&target, &environment, /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut network = managed_network(json!(["example.com"]), json!([]));
    network["socks"] = json!(true);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 880,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            network,
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    drop(io.stdin);
    assert_target_exit(&runner.request(wait_request(/*id*/ 881)), /*code*/ 0);
    assert_eq!(
        decode_values(&read_all(&mut io.stdout)),
        vec![native_value.as_slice()]
    );
}

#[test]
fn managed_proxy_uses_a_trusted_upstream_and_closes_its_listener() {
    let upstream = TcpListener::bind("127.0.0.1:0").expect("upstream listener");
    let upstream_url = format!(
        "http://{}",
        upstream.local_addr().expect("upstream address")
    );
    let server = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().expect("accept upstream request");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("read upstream request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
            .expect("write upstream response");
    });
    let target = fixture_target(&["http-get", "example.com", "80"]);
    let (mut runner, _) = Runner::spawn(
        &target,
        &[("HTTP_PROXY", &upstream_url)],
        /*with_io*/ false,
    );
    let mut network = managed_network(json!(["example.com"]), json!([]));
    network["upstream_proxy"] = json!(true);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 88,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            network,
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 89)), /*code*/ 0);
    server.join().expect("upstream server");

    let target = fixture_target(&["environment", "HTTP_PROXY"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 890,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            managed_network(json!(["example.com"]), json!([])),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    drop(io.stdin);
    assert_target_exit(&runner.request(wait_request(/*id*/ 891)), /*code*/ 0);
    let proxy_output = read_all(&mut io.stdout);
    let proxy_value = decode_values(&proxy_output)[0];
    let proxy = std::str::from_utf8(proxy_value)
        .expect("UTF-8 proxy URL")
        .strip_prefix("http://")
        .expect("HTTP proxy URL");
    assert!(std::net::TcpStream::connect(proxy).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn managed_proxy_applies_typed_unix_socket_policy() {
    use std::os::unix::net::UnixListener;

    let directory = TempDir::new().expect("Unix socket directory");
    let socket_path = directory.path().join("allowed.sock");
    let listener = UnixListener::bind(&socket_path).expect("Unix listener");
    let accept = std::thread::spawn(move || listener.accept().expect("accept Unix connection"));
    let target = fixture_target(&[
        "unix-connect",
        socket_path.to_str().expect("Unicode socket path"),
    ]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let mut network = managed_network(json!([]), json!([]));
    network["unix_sockets"] = json!([{
        "path": socket_path,
        "access": "allow",
    }]);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 892,
            directory.path(),
            json!([]),
            network,
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 893)), /*code*/ 0);
    accept.join().expect("Unix listener thread");
}

#[cfg(target_os = "macos")]
#[test]
fn managed_proxy_unix_socket_denial_fails_before_proxy_or_target_start() {
    let directory = TempDir::new().expect("Unix socket directory");
    let socket_path = directory.path().join("denied.sock");
    let marker_path = directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker_path.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let mut network = managed_network(json!([]), json!([]));
    network["unix_sockets"] = json!([{
        "path": socket_path,
        "access": "deny",
    }]);
    let response = runner.request(default_launch(
        /*id*/ 8920,
        directory.path(),
        json!([{
            "path": directory.path(),
            "access": "write",
            "missing": "error",
        }]),
        network,
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker_path.exists());
    runner.close_control();
    assert!(runner.wait_for_exit().success());
}

#[cfg(target_os = "macos")]
#[test]
fn duplicate_managed_proxy_unix_socket_paths_fail_before_launch() {
    let directory = TempDir::new().expect("Unix socket directory");
    let socket_path = directory.path().join("duplicate.sock");
    let marker_path = directory.path().join("target-started");
    let target = fixture_target(&[
        "write",
        marker_path.to_str().expect("Unicode marker path"),
        "started",
    ]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let mut network = managed_network(json!([]), json!([]));
    network["unix_sockets"] = json!([
        {
            "path": socket_path,
            "access": "allow",
        },
        {
            "path": socket_path,
            "access": "deny",
        },
    ]);
    let response = runner.request(default_launch(
        /*id*/ 8930,
        directory.path(),
        json!([{
            "path": directory.path(),
            "access": "write",
            "missing": "error",
        }]),
        network,
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker_path.exists());
    runner.close_control();
    assert!(runner.wait_for_exit().success());
}

#[test]
fn unsupported_managed_network_authority_is_rejected_before_launch() {
    let target = fixture_target(&["exit", "0"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let mut network = managed_network(json!([]), json!([]));
    network["socks_udp"] = json!(true);
    let response = runner.request(default_launch(
        /*id*/ 894,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        network,
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);

    let mut network = managed_network(json!(["region*.example.com"]), json!([]));
    network["socks_udp"] = json!(false);
    let response = runner.request(default_launch(
        /*id*/ 895,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        network,
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);

    let network = managed_network(json!(["example.com:443"]), json!([]));
    let response = runner.request(default_launch(
        /*id*/ 896,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        network,
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_isolation_allows_new_ptys_and_denies_preexisting_device_reopen() {
    let target = fixture_target(&["create-pty"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let mut launch = default_launch(
        /*id*/ 895,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    );
    launch["launch"]["terminal"] = json!("isolate_host_devices");
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");
    assert_target_exit(&runner.request(wait_request(/*id*/ 896)), /*code*/ 0);

    let (master, slave, slave_path) = open_pty();
    let target = fixture_target(&["reopen", slave_path.to_str().expect("Unicode PTY path")]);
    let (mut preserve_runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        preserve_runner.request(default_launch(
            /*id*/ 897,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(
        &preserve_runner.request(wait_request(/*id*/ 898)),
        /*code*/ 0,
    );

    let (mut isolate_runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let mut launch = default_launch(
        /*id*/ 899,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    );
    launch["launch"]["terminal"] = json!("isolate_host_devices");
    assert_eq!(isolate_runner.request(launch)["type"], "launch_accepted");
    assert_target_exit(
        &isolate_runner.request(wait_request(/*id*/ 900)),
        /*code*/ 73,
    );
    drop((master, slave));
}

#[test]
fn passed_pty_endpoints_remain_native_terminal_streams() {
    let target = fixture_target(&["tty-status"]);
    let (mut runner, mut master) = Runner::spawn_with_passed_pty(&target);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 901,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    let mut tty_status = [0_u8; 3];
    master
        .read_exact(&mut tty_status)
        .expect("read terminal status");
    assert_eq!(tty_status, [1, 1, 1]);
    assert_target_exit(&runner.request(wait_request(/*id*/ 902)), /*code*/ 0);
}

#[test]
fn inherited_terminal_streams_remain_terminals() {
    let target = fixture_target(&["tty-status"]);
    let (mut runner, mut master) = Runner::spawn_with_inherited_pty(&target);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 903,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            json!({
                "stdin": { "mode": "inherited" },
                "stdout": { "mode": "inherited" },
                "stderr": { "mode": "inherited" },
            }),
        ))["type"],
        "launch_accepted"
    );
    let mut tty_status = [0_u8; 3];
    master
        .read_exact(&mut tty_status)
        .expect("read terminal status");
    assert_eq!(tty_status, [1, 1, 1]);
    assert_target_exit(&runner.request(wait_request(/*id*/ 904)), /*code*/ 0);
}

#[cfg(target_os = "macos")]
#[test]
fn preserve_keeps_the_inherited_controlling_terminal() {
    let target = fixture_target(&["open-controlling-terminal"]);
    let (mut runner, _master) = Runner::spawn_with_inherited_pty(&target);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 905,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            json!({
                "stdin": { "mode": "inherited" },
                "stdout": { "mode": "inherited" },
                "stderr": { "mode": "inherited" },
            }),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 906)), /*code*/ 0);
}

#[test]
fn lifecycle_reports_exit_signal_interrupt_and_bounded_retirement() {
    assert_lifecycle_target(
        &["exit", "17"],
        /*expected_kind*/ None,
        /*expected_code*/ 17,
        /*expected_signal*/ None,
    );
    assert_lifecycle_target(
        &["signal", "15"],
        /*expected_kind*/ None,
        /*expected_code*/ 0,
        Some(15),
    );

    let target = fixture_target(&["sleep", "10000"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 90,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    let interrupt = runner.request(json!({
        "type": "interrupt",
        "id": 91,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    #[cfg(target_os = "macos")]
    {
        assert_eq!(interrupt["type"], "acknowledged");
        let outcome = runner.request(wait_request(/*id*/ 92));
        assert_eq!(outcome["outcome"]["target"]["kind"], "signaled");
        assert_eq!(outcome["outcome"]["target"]["signal"], libc::SIGINT);
        assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    }
    #[cfg(target_os = "linux")]
    {
        assert_eq!(interrupt["type"], "error");
        assert_eq!(interrupt["error"]["code"], "unsupported_policy");
        assert_eq!(interrupt["error"]["target_started"], true);
        assert_eq!(
            runner.request(json!({
                "type": "terminate",
                "id": 92,
                "protocol_version": support::PROTOCOL_VERSION,
                "deadlines": { "graceful_ms": 0, "force_ms": 2000 },
            }))["type"],
            "acknowledged"
        );
        let outcome = runner.request(wait_request(/*id*/ 920));
        assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
        assert_eq!(outcome["outcome"]["retirement"]["forced"], true);
    }

    let target = fixture_target(&["spawn-descendant", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 93,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    drop(io.stdin);
    let outcome = runner.request(wait_request(/*id*/ 94));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert!(read_all(&mut io.stdout).is_empty());
}

#[test]
fn target_output_reaches_eof_during_root_exit_grace() {
    let target = fixture_target(&["spawn-descendant-and-sleep", "10000", "100"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 935,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(2500);
    launch["launch"]["lifecycle"]["force_timeout_ms"] = json!(2000);
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");
    drop(io.stdin);

    let mut descendant_process_id = [0_u8; 4];
    io.stdout
        .read_exact(&mut descendant_process_id)
        .expect("read descendant process ID");
    assert_ne!(u32::from_be_bytes(descendant_process_id), 0);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = runner.request(json!({
            "type": "status",
            "id": 936,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if status["status"]["phase"] == "root_exited" {
            break;
        }
        assert_ne!(status["status"]["phase"], "retired", "{status}");
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = read_all(&mut io.stdout);
        let _ = stdout_sender.send(result);
    });
    let (stderr_sender, stderr_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = read_all(&mut io.stderr);
        let _ = stderr_sender.send(result);
    });
    let timeout = std::time::Duration::from_millis(500);
    let stdout = stdout_receiver.recv_timeout(timeout);
    let stderr = stderr_receiver.recv_timeout(timeout);
    if stdout.is_err() || stderr.is_err() {
        runner.close_control();
        let _ = runner.wait_for_exit();
    }
    assert_eq!(
        stdout.expect("target stdout did not reach EOF during root-exit grace"),
        Vec::<u8>::new()
    );
    assert_eq!(
        stderr.expect("target stderr did not reach EOF during root-exit grace"),
        Vec::<u8>::new()
    );
    assert_eq!(
        runner.request(json!({
            "type": "status",
            "id": 937,
            "protocol_version": support::PROTOCOL_VERSION,
        }))["status"]["phase"],
        "root_exited"
    );

    let outcome = runner.request(wait_request(/*id*/ 938));
    assert_target_exit(&outcome, /*code*/ 0);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_session_escaping_descendant_survives_grace_before_forced_retirement() {
    let target = fixture_target(&["spawn-session-escaping-descendant", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 940,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(1000);
    launch["launch"]["lifecycle"]["force_timeout_ms"] = json!(2000);
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = runner.request(json!({
            "type": "status",
            "id": 941,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if status["status"]["phase"] == "root_exited" {
            break;
        }
        assert_ne!(status["status"]["phase"], "retired", "{status}");
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    io.stdin
        .write_all(b"D")
        .expect("write to adopted descendant");
    let mut echoed = [0_u8; 1];
    io.stdout
        .read_exact(&mut echoed)
        .expect("adopted descendant survived root exit");
    assert_eq!(echoed, *b"D");

    let outcome = runner.request(wait_request(/*id*/ 942));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], true);
    assert_eq!(outcome["outcome"]["infrastructure"]["error"], Value::Null);
    assert!(read_all(&mut io.stdout).is_empty());
}

#[test]
fn forced_termination_without_a_target_frame_is_an_infrastructure_failure() {
    let target = fixture_target(&["ignore-term", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 95,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    let mut ready = [0_u8; 1];
    io.stdout.read_exact(&mut ready).expect("target readiness");
    assert_eq!(ready, *b"R");
    assert_eq!(
        runner.request(json!({
            "type": "terminate",
            "id": 96,
            "protocol_version": support::PROTOCOL_VERSION,
            "deadlines": {
                "graceful_ms": if cfg!(target_os = "linux") { 0 } else { 20 },
                "force_ms": 2000,
            },
        }))["type"],
        "acknowledged"
    );
    let outcome = runner.request(wait_request(/*id*/ 97));
    assert_eq!(outcome["outcome"]["target"], Value::Null);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], true);
    assert!(
        outcome["outcome"]["infrastructure"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("target outcome observation failed")),
        "{outcome}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn zero_deadline_termination_does_not_leave_the_target_group_running() {
    let target = fixture_target(&["spawn-ignore-term-descendant", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 951,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let target_process_id = host_observable_target_process_id(&runner, &response);
    let mut ready = [0_u8; 1];
    io.stdout.read_exact(&mut ready).expect("target readiness");
    assert_eq!(ready, *b"R");

    assert_eq!(
        runner.request(json!({
            "type": "terminate",
            "id": 952,
            "protocol_version": support::PROTOCOL_VERSION,
            "deadlines": { "graceful_ms": 0, "force_ms": 0 },
        }))["type"],
        "acknowledged"
    );
    let outcome = runner.request(wait_request(/*id*/ 953));
    assert_eq!(outcome["type"], "final", "{outcome}");
    assert_eq!(outcome["outcome"]["target"], Value::Null, "{outcome}");
    assert_eq!(
        outcome["outcome"]["retirement"],
        json!({ "complete": true, "forced": true, "error": null }),
        "{outcome}"
    );
    assert!(
        outcome["outcome"]["infrastructure"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("target outcome observation failed")),
        "{outcome}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while unsafe { libc::kill(target_process_id, 0) } == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let final_probe = unsafe { libc::kill(target_process_id, 0) };
    let final_error = std::io::Error::last_os_error().raw_os_error();
    if final_probe == 0 {
        let _ = codex_utils_pty::process_group::kill_process_group(target_process_id as u32);
    }
    assert_eq!(final_probe, -1);
    assert_eq!(final_error, Some(libc::ESRCH));
}

#[test]
fn retired_generation_rejects_signals_while_cleanup_is_blocked() {
    let target = fixture_target(&["copy"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 98,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    let watchdog_process_id = watchdog_process_id(runner.process_id());
    assert_eq!(unsafe { libc::kill(watchdog_process_id, libc::SIGSTOP) }, 0);
    let mut stopped_watchdog = StoppedProcess::new(watchdog_process_id);
    drop(io.stdin);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let response = runner.request(json!({
            "type": "status",
            "id": 99,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if response["status"]["phase"] == "retired" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{response}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let signal_requests = [
        #[cfg(target_os = "macos")]
        json!({
            "type": "interrupt",
            "id": 100,
            "protocol_version": support::PROTOCOL_VERSION,
        }),
        json!({
            "type": "terminate",
            "id": 101,
            "protocol_version": support::PROTOCOL_VERSION,
            "deadlines": {
                "graceful_ms": if cfg!(target_os = "linux") { 0 } else { 20 },
                "force_ms": 2000,
            },
        }),
    ];
    for request in signal_requests {
        let response = runner.request(request);
        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "invalid_state");
        assert_eq!(
            response["error"]["message"],
            "target generation is already retired"
        );
    }

    stopped_watchdog.resume();
    assert_target_exit(&runner.request(wait_request(/*id*/ 102)), /*code*/ 0);
}

#[test]
fn valid_target_outcome_is_preserved_when_watchdog_cleanup_fails() {
    let target = fixture_target(&["copy"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 1021,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    let watchdog_process_id = watchdog_process_id(runner.process_id());
    assert_eq!(unsafe { libc::kill(watchdog_process_id, libc::SIGSTOP) }, 0);
    let mut stopped_watchdog = StoppedProcess::new(watchdog_process_id);
    drop(io.stdin);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let response = runner.request(json!({
            "type": "status",
            "id": 1022,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if response["status"]["phase"] == "retired" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{response}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let outcome = runner.request(wait_request(/*id*/ 1023));
    stopped_watchdog.process_id = 0;
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], false);
    assert_eq!(outcome["outcome"]["infrastructure"]["error"], Value::Null);
    assert!(
        outcome["outcome"]["infrastructure"]["cleanup_error"]
            .as_str()
            .is_some_and(|error| error
                .contains("watchdog exit timed out and forced retirement was requested")),
        "{outcome}"
    );
}

#[test]
fn watchdog_exit_forces_retirement_without_replacing_a_valid_target_outcome() {
    #[cfg(target_os = "linux")]
    let marker = linux_descendant_marker("watchdog-exit");
    #[cfg(target_os = "linux")]
    let target = fixture_target(&["spawn-marked-descendant", marker.as_str(), "10000"]);
    #[cfg(not(target_os = "linux"))]
    let target = fixture_target(&["spawn-reporting-descendant", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 103,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(10000);
    launch["launch"]["lifecycle"]["force_timeout_ms"] = json!(2000);
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");
    drop(io.stdin);

    #[cfg(target_os = "linux")]
    {
        let mut ready = [0_u8; 1];
        io.stdout
            .read_exact(&mut ready)
            .expect("read marked descendant readiness");
        assert_eq!(ready, *b"R");
    }
    #[cfg(not(target_os = "linux"))]
    let descendant_process_id = {
        let mut descendant_process_id = [0_u8; 4];
        io.stdout
            .read_exact(&mut descendant_process_id)
            .expect("read descendant host process ID");
        i32::try_from(u32::from_be_bytes(descendant_process_id)).expect("descendant process ID")
    };
    let root_exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = runner.request(json!({
            "type": "status",
            "id": 104,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if status["status"]["phase"] == "root_exited" {
            break;
        }
        assert_ne!(status["status"]["phase"], "retired", "{status}");
        assert!(std::time::Instant::now() < root_exit_deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    #[cfg(target_os = "linux")]
    let descendant_process =
        open_linux_process_with_marker(&marker, std::time::Duration::from_secs(2));

    let watchdog_process_id = watchdog_process_id(runner.process_id());
    assert_eq!(unsafe { libc::kill(watchdog_process_id, libc::SIGKILL) }, 0);

    let outcome = runner.request(wait_request(/*id*/ 105));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], true);
    assert!(
        outcome["outcome"]["infrastructure"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("watchdog exited unexpectedly")),
        "{outcome}"
    );
    #[cfg(target_os = "linux")]
    wait_for_linux_process_handle_exit(&descendant_process, std::time::Duration::from_secs(2));
    #[cfg(not(target_os = "linux"))]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while unsafe { libc::kill(descendant_process_id, 0) } == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(unsafe { libc::kill(descendant_process_id, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[test]
fn state_machine_rejects_invalid_and_second_generation_transitions() {
    let target = fixture_target(&["exit", "0"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let status = runner.request(json!({
        "type": "status",
        "id": 110,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    assert_eq!(status["status"]["phase"], "idle");
    let interrupt = runner.request(json!({
        "type": "interrupt",
        "id": 111,
        "protocol_version": support::PROTOCOL_VERSION,
    }));
    assert_eq!(interrupt["type"], "error");
    assert_eq!(interrupt["error"]["code"], "invalid_state");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 112,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 113)), /*code*/ 0);
    let cwd = std::env::current_dir().expect("current directory");
    let setup_after_launch = runner.request(json!({
        "type": "setup_status",
        "id": 114,
        "protocol_version": support::PROTOCOL_VERSION,
        "setup": {
            "working_directory": cwd,
            "policy_base_directory": cwd,
            "filesystem": { "base": "host_read_only", "rules": [] },
            "network": { "mode": "denied" },
            "platform_extensions": {},
        },
    }));
    assert_eq!(setup_after_launch["type"], "error");
    assert_eq!(setup_after_launch["error"]["code"], "invalid_state");
    let second = runner.request(default_launch(
        /*id*/ 115,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(second["type"], "error");
    assert_eq!(second["error"]["code"], "invalid_state");
}

#[test]
fn control_loss_retires_the_target_generation() {
    let target = fixture_target(&["sleep", "10000"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 100,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    let target_pid = host_observable_target_process_id(&runner, &response);
    runner.close_control();
    assert!(runner.wait_for_exit().success());
    assert_eq!(unsafe { libc::kill(target_pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn control_loss_interrupts_root_exit_grace_for_remaining_descendants() {
    #[cfg(target_os = "linux")]
    let marker = linux_descendant_marker("control-loss");
    #[cfg(target_os = "linux")]
    let target = fixture_target(&["spawn-marked-descendant", marker.as_str(), "10000"]);
    #[cfg(not(target_os = "linux"))]
    let target = fixture_target(&["spawn-reporting-descendant", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 110,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(10000);
    launch["launch"]["lifecycle"]["force_timeout_ms"] = json!(500);
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");
    drop(io.stdin);
    #[cfg(target_os = "linux")]
    {
        let mut ready = [0_u8; 1];
        io.stdout
            .read_exact(&mut ready)
            .expect("read marked descendant readiness");
        assert_eq!(ready, *b"R");
    }
    #[cfg(not(target_os = "linux"))]
    let descendant_process_id = {
        let mut descendant_process_id = [0_u8; 4];
        io.stdout
            .read_exact(&mut descendant_process_id)
            .expect("read descendant host process ID");
        i32::try_from(u32::from_be_bytes(descendant_process_id)).expect("descendant process ID")
    };
    let root_exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = runner.request(json!({
            "type": "status",
            "id": 111,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if status["status"]["phase"] == "root_exited" {
            break;
        }
        assert_ne!(status["status"]["phase"], "retired");
        assert!(std::time::Instant::now() < root_exit_deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    #[cfg(target_os = "linux")]
    let descendant_process =
        open_linux_process_with_marker(&marker, std::time::Duration::from_secs(2));

    let started = std::time::Instant::now();
    runner.close_control();
    assert!(runner.wait_for_exit().success());
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    #[cfg(target_os = "linux")]
    wait_for_linux_process_handle_exit(&descendant_process, std::time::Duration::from_secs(2));
    #[cfg(not(target_os = "linux"))]
    {
        let retirement_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while unsafe { libc::kill(descendant_process_id, 0) } == 0
            && std::time::Instant::now() < retirement_deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(unsafe { libc::kill(descendant_process_id, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_termination_retires_a_root_exited_descendant() {
    let marker = linux_descendant_marker("explicit-termination");
    let target = fixture_target(&["spawn-marked-descendant", marker.as_str(), "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 116,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(10000);
    launch["launch"]["lifecycle"]["force_timeout_ms"] = json!(500);
    assert_eq!(runner.request(launch)["type"], "launch_accepted");
    drop(io.stdin);

    let mut ready = [0_u8; 1];
    io.stdout
        .read_exact(&mut ready)
        .expect("read marked descendant readiness");
    assert_eq!(ready, *b"R");
    let root_exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = runner.request(json!({
            "type": "status",
            "id": 117,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if status["status"]["phase"] == "root_exited" {
            break;
        }
        assert_ne!(status["status"]["phase"], "retired", "{status}");
        assert!(std::time::Instant::now() < root_exit_deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let descendant_process =
        open_linux_process_with_marker(&marker, std::time::Duration::from_secs(2));

    assert_eq!(
        runner.request(json!({
            "type": "terminate",
            "id": 118,
            "protocol_version": support::PROTOCOL_VERSION,
            "deadlines": { "graceful_ms": 0, "force_ms": 500 },
        }))["type"],
        "acknowledged"
    );
    let outcome = runner.request(wait_request(/*id*/ 119));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], true);
    wait_for_linux_process_handle_exit(&descendant_process, std::time::Duration::from_secs(2));
}

#[cfg(target_os = "linux")]
#[test]
fn abrupt_runner_termination_after_root_exit_retires_the_descendant() {
    let marker = linux_descendant_marker("abrupt-runner-termination");
    let target = fixture_target(&["spawn-marked-descendant", marker.as_str(), "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 122,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(10000);
    launch["launch"]["lifecycle"]["force_timeout_ms"] = json!(500);
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");
    drop(io.stdin);

    let mut ready = [0_u8; 1];
    io.stdout
        .read_exact(&mut ready)
        .expect("read marked descendant readiness");
    assert_eq!(ready, *b"R");
    let root_exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = runner.request(json!({
            "type": "status",
            "id": 123,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if status["status"]["phase"] == "root_exited" {
            break;
        }
        assert_ne!(status["status"]["phase"], "retired", "{status}");
        assert!(std::time::Instant::now() < root_exit_deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let descendant_process =
        open_linux_process_with_marker(&marker, std::time::Duration::from_secs(2));

    runner.kill();
    assert!(!runner.wait_for_exit().success());
    wait_for_linux_process_handle_exit(&descendant_process, std::time::Duration::from_secs(2));
}

#[test]
fn abrupt_runner_termination_does_not_orphan_the_target_group() {
    let target = fixture_target(&["sleep", "10000"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let response = runner.request(default_launch(
        /*id*/ 120,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    let target_pid = host_observable_target_process_id(&runner, &response);
    runner.kill();
    assert!(!runner.wait_for_exit().success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while unsafe { libc::kill(target_pid, 0) } == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(target_pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn runner_process_group_termination_still_retires_the_target_group() {
    let target = fixture_target(&["ignore-term", "10000"]);
    let (mut runner, io) = Runner::spawn_in_new_process_group(&target, &[], /*with_io*/ true);
    let response = runner.request(default_launch(
        /*id*/ 121,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted");
    let target_pid = host_observable_target_process_id(&runner, &response);
    let runner_pid = i32::try_from(runner.process_id()).expect("runner process ID");
    assert_eq!(unsafe { libc::getpgid(runner_pid) }, runner_pid);

    let mut stdout = io.expect("target streams").stdout;
    stdout
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("bound target retirement wait");
    let mut ready = [0_u8; 1];
    stdout.read_exact(&mut ready).expect("target readiness");
    assert_eq!(ready, *b"R");

    assert_eq!(unsafe { libc::killpg(runner_pid, libc::SIGKILL) }, 0);
    assert!(!runner.wait_for_exit().success());
    let mut byte = [0_u8; 1];
    let retirement = stdout.read(&mut byte);
    if !matches!(retirement, Ok(0)) {
        let _ = codex_utils_pty::process_group::kill_process_group(target_pid as u32);
    }
    assert!(
        matches!(retirement, Ok(0)),
        "target stream did not reach EOF after runner group termination: {retirement:?}"
    );
}

fn default_launch(
    id: u64,
    cwd: &Path,
    filesystem_rules: Value,
    network: Value,
    streams: Value,
) -> Value {
    launch_request(
        id,
        cwd,
        "host_read_only",
        filesystem_rules,
        network,
        streams,
        default_lifecycle(),
    )
}

fn assert_write_outcome(path: std::path::PathBuf, cwd: &Path, rules: Value, exit_code: i32) {
    let target = fixture_target(&["write", path.to_str().expect("Unicode path"), "contents"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 1,
            cwd,
            rules,
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 2)), exit_code);
}

fn assert_read_outcome(target: &[std::ffi::OsString], base: &str, rules: Value, exit_code: i32) {
    let (mut runner, _) = Runner::spawn(target, &[], /*with_io*/ false);
    let cwd = std::env::current_dir().expect("current directory");
    let response = runner.request(launch_request(
        /*id*/ 1,
        &cwd,
        base,
        rules,
        json!({ "mode": "denied" }),
        null_streams(),
        default_lifecycle(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    assert_target_exit(&runner.request(wait_request(/*id*/ 2)), exit_code);
}

fn assert_lifecycle_target(
    arguments: &[&str],
    expected_kind: Option<&str>,
    expected_code: i32,
    expected_signal: Option<i32>,
) {
    let target = fixture_target(arguments);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 1,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    let outcome = runner.request(wait_request(/*id*/ 2));
    match expected_signal {
        Some(signal) => {
            assert_eq!(outcome["outcome"]["target"]["kind"], "signaled");
            assert_eq!(outcome["outcome"]["target"]["signal"], signal);
        }
        None => {
            assert_eq!(
                outcome["outcome"]["target"]["kind"],
                expected_kind.unwrap_or("exited")
            );
            assert_eq!(outcome["outcome"]["target"]["code"], expected_code);
        }
    }
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
}

fn managed_network(allowed_domains: Value, denied_domains: Value) -> Value {
    let local_binding = cfg!(target_os = "linux");
    let loopback = if local_binding { "allow" } else { "proxy_only" };
    json!({
        "mode": "managed_proxy",
        "access": "full",
        "allowed_domains": allowed_domains,
        "denied_domains": denied_domains,
        "socks": false,
        "socks_udp": false,
        "upstream_proxy": false,
        "local_binding": local_binding,
        "loopback": loopback,
        "local_ports": [],
        "unix_sockets": [],
    })
}

fn assert_target_exit(outcome: &Value, code: i32) {
    assert_eq!(outcome["type"], "final");
    assert_eq!(outcome["outcome"]["target"]["kind"], "exited");
    assert_eq!(outcome["outcome"]["target"]["code"], code);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
}

fn host_observable_target_process_id(runner: &Runner, response: &Value) -> i32 {
    #[cfg(target_os = "linux")]
    {
        assert_eq!(response["root_process_id"], Value::Null);
        sandbox_process_group_leader(runner.process_id())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = runner;
        response["root_process_id"]
            .as_u64()
            .expect("root process id") as i32
    }
}

fn watchdog_process_id(runner_process_id: u32) -> i32 {
    let output = std::process::Command::new("/bin/ps")
        .args(["-ww", "-axo", "pid=,ppid=,command="])
        .output()
        .expect("inspect runner child processes");
    assert!(output.status.success(), "ps failed: {output:?}");
    let output = String::from_utf8(output.stdout).expect("Unicode ps output");
    let watchdogs = output
        .lines()
        .filter(|line| line.contains("--mcp-console-sandbox-watch-process-group"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let process_id = fields.next()?.parse::<i32>().ok()?;
            let parent_process_id = fields.next()?.parse::<u32>().ok()?;
            (parent_process_id == runner_process_id).then_some(process_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(watchdogs.len(), 1, "runner watchdog process: {output}");
    watchdogs[0]
}

struct StoppedProcess {
    process_id: i32,
}

impl StoppedProcess {
    fn new(process_id: i32) -> Self {
        Self { process_id }
    }

    fn resume(&mut self) {
        assert_eq!(unsafe { libc::kill(self.process_id, libc::SIGCONT) }, 0);
        self.process_id = 0;
    }
}

impl Drop for StoppedProcess {
    fn drop(&mut self) {
        if self.process_id != 0 {
            let _ = unsafe { libc::kill(self.process_id, libc::SIGCONT) };
        }
    }
}

#[cfg(target_os = "linux")]
fn sandbox_process_group_leader(runner_process_id: u32) -> i32 {
    let children = std::fs::read_to_string(format!(
        "/proc/{runner_process_id}/task/{runner_process_id}/children"
    ))
    .expect("read runner child process IDs");
    let leaders = children
        .split_whitespace()
        .map(|process_id| process_id.parse::<i32>().expect("child process ID"))
        .filter(|process_id| unsafe { libc::getpgid(*process_id) } == *process_id)
        .filter(|process_id| {
            let command_line = std::fs::read(format!("/proc/{process_id}/cmdline"))
                .expect("read runner child command line");
            !command_line
                .split(|byte| *byte == 0)
                .any(|argument| argument == b"--mcp-console-sandbox-watch-process-group")
        })
        .collect::<Vec<_>>();
    assert_eq!(leaders.len(), 1, "sandbox process-group leader: {children}");
    leaders[0]
}

#[cfg(target_os = "macos")]
fn parent_process_id(process_id: i32) -> i32 {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "ppid=", "-p"])
        .arg(process_id.to_string())
        .output()
        .expect("inspect target parent process");
    assert!(output.status.success(), "ps failed: {output:?}");
    String::from_utf8(output.stdout)
        .expect("Unicode ps output")
        .trim()
        .parse()
        .expect("target parent process ID")
}

fn read_all(mut stream: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read stream to EOF");
    bytes
}

fn decode_values(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut values = Vec::new();
    while !bytes.is_empty() {
        let mut length = [0_u8; 4];
        bytes.read_exact(&mut length).expect("native value length");
        let length = u32::from_be_bytes(length) as usize;
        let (value, remaining) = bytes.split_at(length);
        values.push(value);
        bytes = remaining;
    }
    values
}

fn decode_optional_values(mut bytes: &[u8]) -> Vec<Option<Vec<u8>>> {
    let mut values = Vec::new();
    while !bytes.is_empty() {
        let mut length = [0_u8; 4];
        bytes
            .read_exact(&mut length)
            .expect("environment value length");
        let length = u32::from_be_bytes(length);
        if length == u32::MAX {
            values.push(None);
        } else {
            let mut value = vec![0; length as usize];
            bytes
                .read_exact(&mut value)
                .expect("environment value bytes");
            values.push(Some(value));
        }
    }
    values
}

#[cfg(target_os = "linux")]
fn write_linux_companion(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write companion fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make companion fixture executable");
    path
}

#[cfg(target_os = "linux")]
fn wait_for_linux_process_exit(process_id: i32, timeout: std::time::Duration) {
    let descriptor = unsafe {
        libc::syscall(libc::SYS_pidfd_open, process_id, /*flags*/ 0)
    };
    if descriptor == -1 {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "open process {process_id}"
        );
        return;
    }
    let process = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
    wait_for_linux_process_handle_exit(&process, timeout);
}

#[cfg(target_os = "linux")]
fn linux_descendant_marker(label: &str) -> String {
    format!("mcp-console-sandbox-test-{}-{label}", std::process::id())
}

#[cfg(target_os = "linux")]
fn open_linux_process_with_marker(marker: &str, timeout: std::time::Duration) -> OwnedFd {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut matching_processes = Vec::new();
        for entry in std::fs::read_dir("/proc").expect("read host process table") {
            let Ok(entry) = entry else {
                continue;
            };
            let Some(process_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            let Ok(command_line) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let arguments = command_line
                .split(|byte| *byte == 0)
                .filter(|argument| !argument.is_empty())
                .collect::<Vec<_>>();
            if arguments.contains(&b"marked-sleep".as_slice())
                && arguments.contains(&marker.as_bytes())
            {
                matching_processes.push(process_id);
            }
        }
        assert!(
            matching_processes.len() <= 1,
            "multiple host processes matched descendant marker {marker}: {matching_processes:?}"
        );
        if let Some(process_id) = matching_processes.pop() {
            let descriptor = unsafe {
                libc::syscall(libc::SYS_pidfd_open, process_id, /*flags*/ 0)
            };
            if descriptor >= 0 {
                return unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH),
                "open marked descendant {process_id}"
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "marked descendant {marker} was not visible in the host process table"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn wait_for_linux_process_handle_exit(process: &OwnedFd, timeout: std::time::Duration) {
    let timeout_ms = i32::try_from(timeout.as_millis()).expect("process-exit timeout milliseconds");
    let mut descriptor = libc::pollfd {
        fd: process.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe {
            libc::poll(&mut descriptor, /*nfds*/ 1, timeout_ms)
        },
        1,
        "process did not exit before {timeout:?}: {}",
        std::io::Error::last_os_error()
    );
    assert_ne!(descriptor.revents & libc::POLLIN, 0);
}

#[cfg(target_os = "macos")]
fn open_pty() -> (std::fs::File, std::fs::File, std::path::PathBuf) {
    use std::ffi::CStr;
    use std::os::fd::FromRawFd;

    let mut master = -1;
    let mut slave = -1;
    let mut name = [0_i8; 1024];
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let path = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .expect("UTF-8 PTY path")
        .into();
    (
        unsafe { std::fs::File::from_raw_fd(master) },
        unsafe { std::fs::File::from_raw_fd(slave) },
        path,
    )
}
