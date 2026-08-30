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
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use support::Runner;
use support::default_lifecycle;
use support::fixture_target;
use support::launch_request;
use support::null_streams;
#[cfg(target_os = "macos")]
use support::open_pty;
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
fn target_closing_stdin_releases_the_writer_while_the_target_remains_live() {
    let target = fixture_target(&["close-stdin-then-ready-and-wait"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 5,
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
    io.stdout
        .read_exact(&mut ready)
        .expect("read target readiness");
    assert_eq!(ready, *b"R");
    io.stdin
        .write_all(b"D")
        .expect_err("target stdin writer should be released");

    runner.close_control();
    assert!(runner.wait_for_exit().success());
}

#[cfg(target_os = "linux")]
#[test]
fn denied_network_preserves_connected_unix_stream_io() {
    let target = fixture_target(&["connected-unix-stream-io"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 3,
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
    io.stdout
        .read_exact(&mut ready)
        .expect("read target readiness");
    assert_eq!(ready, *b"R");
    io.stdin.write_all(b"I").expect("write target request");

    let mut response = Vec::new();
    io.stdout
        .read_to_end(&mut response)
        .expect("read target response to shutdown");
    assert_eq!(response, b"O");
    assert_target_exit(&runner.request(wait_request(/*id*/ 4)), /*code*/ 0);
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

#[cfg(target_os = "macos")]
#[test]
fn preparation_failure_keeps_the_runner_idle_for_a_corrected_launch() {
    use std::os::unix::fs::PermissionsExt;

    let target = fixture_target(&["exit", "0"]);
    let state_directory = TempDir::new().expect("state directory");
    let state_path = state_directory.path().to_path_buf();
    let (mut runner, _) =
        Runner::spawn_with_state(&target, &[], /*with_io*/ false, state_directory);
    std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o500))
        .expect("make state directory read-only");
    let first = runner.request(default_launch(
        /*id*/ 26,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(first["type"], "error", "{first}");
    assert_eq!(first["error"]["phase"], "sandbox_preparation");
    assert_eq!(first["error"]["target_started"], false);

    std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o700))
        .expect("make state directory writable");
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
    assert_target_exit(&runner.request(wait_request(/*id*/ 28)), /*code*/ 0);
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
    assert_eq!(response["type"], "launch_accepted", "{response}");

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
            .is_some_and(|error| error.contains("target executable could not start")),
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
fn packaged_bwrap_does_not_search_the_target_path() {
    let poison_directory = TempDir::new().expect("poison PATH directory");
    let poison = poison_directory.path().join("bwrap");
    let fixture = std::path::PathBuf::from(fixture_target(&["exit", "0"]).remove(0));
    std::fs::copy(fixture, &poison).expect("stage PATH bubblewrap probe");
    let marker = poison_directory.path().join("selected");
    let path = std::env::join_paths(
        std::iter::once(poison_directory.path().to_path_buf()).chain(
            std::env::var_os("PATH")
                .as_deref()
                .map(std::env::split_paths)
                .into_iter()
                .flatten(),
        ),
    )
    .expect("compose target PATH");
    let path = path.to_str().expect("Unicode target PATH");
    let marker_value = marker.to_str().expect("Unicode marker path");
    let target = fixture_target(&["exit", "0"]);
    let (mut runner, _) = Runner::spawn(
        &target,
        &[
            ("PATH", path),
            ("TEST_MCP_CONSOLE_PATH_BWRAP_MARKER", marker_value),
        ],
        /*with_io*/ false,
    );
    let response = runner.request(default_launch(
        /*id*/ 231,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    assert_target_exit(&runner.request(wait_request(/*id*/ 232)), /*code*/ 0);
    assert!(
        !marker.exists(),
        "runner selected bwrap from the target PATH"
    );
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
            .is_some_and(|detail| detail.contains("digest mismatch")),
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
    let response = runner.request(default_launch(
        /*id*/ 69,
        host_directory.path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    assert_target_exit(&runner.request(wait_request(/*id*/ 691)), /*code*/ 73);
    assert!(!attempted_write.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_host_read_only_preserves_required_native_runtime_access() {
    let target = fixture_target(&["macos-policy-probe"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 692,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 693)), /*code*/ 0);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_host_read_only_denies_preexisting_pseudo_terminal_reopen() {
    let (_master, slave) = open_pty();
    let name = unsafe { libc::ttyname(slave.as_raw_fd()) };
    assert!(!name.is_null(), "resolve pseudo-terminal name");
    let name = unsafe { std::ffi::CStr::from_ptr(name) }
        .to_str()
        .expect("Unicode pseudo-terminal name");
    let target = fixture_target(&["reopen", name]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 694,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 695)), /*code*/ 73);
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
    assert_eq!(
        environment["CODEX_CA_CERTIFICATE"].as_deref(),
        Some(b"/private/managed-ca.pem".as_slice())
    );
    assert_eq!(
        environment["CODEX_WINDOWS_SANDBOX_PROXY_PORTS"].as_deref(),
        Some(b"1234".as_slice())
    );
    assert_eq!(
        environment["CODEX_HOME"].as_deref(),
        Some(b"/private/codex-home".as_slice())
    );
    assert_eq!(environment["MCP_CONSOLE_SANDBOX_CONTROL"], None);
    assert_eq!(
        environment["CARGO_BIN_EXE_PRIVATE_HELPER"].as_deref(),
        Some(b"/private/helper".as_slice())
    );
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
    assert_eq!(response["type"], "error", "{response}");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);

    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 896,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    assert_target_exit(&runner.request(wait_request(/*id*/ 897)), /*code*/ 0);
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
fn macos_preserves_non_unicode_target_arguments() {
    let argument = OsString::from_vec(b"native-\xff-argument".to_vec());
    let mut target = fixture_target(&["argv"]);
    target.push(argument.clone());
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 9041,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    assert_target_exit(&runner.request(wait_request(/*id*/ 9042)), /*code*/ 0);
    assert_eq!(
        decode_values(&read_all(&mut io.stdout)),
        vec![argument.as_encoded_bytes()]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_preserves_inherited_ignored_signal_dispositions() {
    for (index, signal) in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM]
        .into_iter()
        .enumerate()
    {
        let target = fixture_target(&["signal-disposition", &signal.to_string()]);
        let (mut runner, io) = Runner::spawn_with_ignored_signal(&target, signal);
        let mut io = io.expect("target streams");
        assert_eq!(
            runner.request(default_launch(
                /*id*/ 9043 + index as u64 * 2,
                std::env::current_dir()
                    .expect("current directory")
                    .as_path(),
                json!([]),
                json!({ "mode": "denied" }),
                passed_streams(),
            ))["type"],
            "launch_accepted"
        );
        assert_target_exit(
            &runner.request(wait_request(/*id*/ 9044 + index as u64 * 2)),
            /*code*/ 0,
        );
        assert_eq!(read_all(&mut io.stdout), vec![1], "signal {signal}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_service_launch_preserves_the_runner_foreground_group() {
    let target = fixture_target(&["sleep", "10000"]);
    let (mut runner, master, launcher_process_group, mut launcher_release) =
        Runner::spawn_with_surviving_pty_launcher(&target);
    let mut launch = default_launch(
        /*id*/ 9045,
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
    );
    launch["launch"]["lifecycle"]["kind"] = json!("service");
    assert_eq!(runner.request(launch)["type"], "launch_accepted");
    assert_eq!(
        unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
        launcher_process_group
    );
    assert_eq!(
        runner.request(json!({
            "type": "terminate",
            "id": 9046,
            "protocol_version": support::PROTOCOL_VERSION,
            "deadlines": { "graceful_ms": 0, "force_ms": 2000 },
        }))["type"],
        "acknowledged"
    );
    assert_eq!(runner.request(wait_request(/*id*/ 9047))["type"], "final");
    runner.close_control();
    launcher_release
        .write_all(b"x")
        .expect("release surviving terminal launcher");
    assert!(runner.wait_for_exit().success());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_foregrounds_the_target_terminal_and_restores_the_runner() {
    let target = fixture_target(&["echo-then-sleep", "10000"]);
    let (mut runner, mut master) = Runner::spawn_with_inherited_pty(&target);
    let runner_process_group = runner.process_id() as libc::pid_t;
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

    master.write_all(b"x").expect("write target input");
    let mut echoed = [0_u8; 1];
    master.read_exact(&mut echoed).expect("read target output");
    assert_eq!(echoed, *b"x");
    master.write_all(&[3]).expect("write terminal interrupt");

    let outcome = runner.request(wait_request(/*id*/ 906));
    assert_eq!(outcome["outcome"]["target"]["kind"], "signaled");
    assert_eq!(outcome["outcome"]["target"]["signal"], libc::SIGINT);
    assert_eq!(
        unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
        runner_process_group
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_restores_the_foreground_terminal_for_a_fast_target() {
    let target = vec![OsString::from("/usr/bin/true")];
    let (mut runner, master) = Runner::spawn_with_inherited_pty(&target);
    let runner_process_group = runner.process_id() as libc::pid_t;
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 907,
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
    assert_target_exit(&runner.request(wait_request(/*id*/ 908)), /*code*/ 0);
    assert_eq!(
        unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
        runner_process_group
    );
}

#[test]
fn lifecycle_reports_exit_signal_interrupt_and_bounded_retirement() {
    assert_lifecycle_target(
        &["exit", "17"],
        /*expected_kind*/ None,
        /*expected_code*/ 17,
        /*expected_signal*/ None,
    );
    #[cfg(target_os = "macos")]
    assert_lifecycle_target(
        &["signal", "15"],
        /*expected_kind*/ None,
        /*expected_code*/ 0,
        Some(15),
    );
    #[cfg(target_os = "linux")]
    assert_lifecycle_target(
        &["signal", "15"],
        Some("exited"),
        128 + libc::SIGTERM,
        /*expected_signal*/ None,
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

#[cfg(target_os = "macos")]
#[test]
fn graceful_termination_forces_a_target_that_ignores_terminate() {
    let target = fixture_target(&["ignore-terminate-and-sleep", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 95,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let mut ready = [0_u8; 1];
    io.stdout.read_exact(&mut ready).expect("target ready");
    assert_eq!(ready, *b"R");

    assert_eq!(
        runner.request(json!({
            "type": "terminate",
            "id": 96,
            "protocol_version": support::PROTOCOL_VERSION,
            "deadlines": { "graceful_ms": 50, "force_ms": 2000 },
        }))["type"],
        "acknowledged"
    );
    let outcome = runner.request(wait_request(/*id*/ 97));
    assert_eq!(outcome["outcome"]["target"]["kind"], "signaled");
    assert_eq!(outcome["outcome"]["target"]["signal"], libc::SIGKILL);
    assert_eq!(
        outcome["outcome"]["retirement"]["complete"], true,
        "{outcome}"
    );
    assert_eq!(
        outcome["outcome"]["retirement"]["forced"], true,
        "{outcome}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_retires_a_descendant_in_a_new_session() {
    let target = fixture_target(&["spawn-session-escaping-descendant-and-wait", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let response = runner.request(default_launch(
        /*id*/ 925,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");

    let descendant_process_id = read_process_id(&mut io.stdout);
    assert!(cleanup_directory.exists());
    assert_eq!(
        unsafe { libc::getpgid(descendant_process_id) },
        descendant_process_id
    );
    drop(io.stdin);
    let outcome = runner.request(wait_request(/*id*/ 926));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_processes_exit(&[descendant_process_id], std::time::Duration::from_secs(2));
    assert!(!cleanup_directory.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_normal_finish_allows_both_grace_periods_before_force() {
    let target = fixture_target(&[
        "spawn-session-escaping-stubborn-descendant-and-wait",
        "10000",
    ]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 930,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"] = json!({
        "kind": "command",
        "root_exit_grace_ms": 700,
        "terminate_grace_ms": 700,
        "force_timeout_ms": 50,
    });
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");

    let descendant_process_id = read_process_id(&mut io.stdout);
    drop(io.stdin);
    let outcome = runner.request(wait_request(/*id*/ 931));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(
        outcome["outcome"]["retirement"]["forced"], true,
        "{outcome}"
    );
    assert_processes_exit(&[descendant_process_id], std::time::Duration::from_secs(2));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_preserves_cleanup_state_when_directory_identity_changes() {
    let target = fixture_target(&["ready-then-wait"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let displaced_directory = cleanup_directory.with_extension("displaced");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 926,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    let mut ready = [0];
    io.stdout.read_exact(&mut ready).expect("target readiness");
    assert_eq!(ready, *b"R");
    std::fs::rename(&cleanup_directory, &displaced_directory)
        .expect("displace target cleanup directory");
    std::fs::create_dir(&cleanup_directory).expect("replace target cleanup directory");
    drop(io.stdin);

    let outcome = runner.request(wait_request(/*id*/ 927));
    assert_target_exit(&outcome, /*code*/ 0);
    assert!(
        outcome["outcome"]["infrastructure"]["cleanup_error"]
            .as_str()
            .is_some_and(|error| error.contains("identity changed"))
    );
    assert!(cleanup_directory.exists());
    assert!(displaced_directory.exists());

    std::fs::remove_dir(&cleanup_directory).expect("remove replacement cleanup directory");
    std::fs::remove_dir_all(&displaced_directory).expect("remove displaced cleanup directory");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_cleanup_repairs_target_directory_permissions() {
    let target = fixture_target(&["lock-child-and-exit", "23"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let response = runner.request(default_launch(
        /*id*/ 9250,
        &cleanup_directory,
        json!([{
            "path": cleanup_directory,
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");

    let outcome = runner.request(wait_request(/*id*/ 9251));
    assert_target_exit(&outcome, /*code*/ 23);
    assert_eq!(
        outcome["outcome"]["infrastructure"]["cleanup_error"],
        Value::Null,
        "{outcome}"
    );
    assert!(!cleanup_directory.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_does_not_remove_a_replacement_cleanup_directory_during_launch() {
    let target = fixture_target(&["exit", "0"]);
    let (mut runner, _) = Runner::spawn(&target, &[], /*with_io*/ false);
    assert_eq!(
        runner.request(json!({
            "id": 9260,
            "protocol_version": 1,
            "type": "discover",
        }))["type"],
        "capabilities"
    );
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let displaced_directory = cleanup_directory.with_extension("displaced-before-launch");
    std::fs::rename(&cleanup_directory, &displaced_directory)
        .expect("displace target cleanup directory");
    std::fs::create_dir(&cleanup_directory).expect("replace target cleanup directory");

    let response = runner.request(default_launch(
        /*id*/ 9261,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(response["type"], "error", "{response}");
    assert_eq!(response["error"]["phase"], "launch");
    assert_eq!(response["error"]["target_started"], true);
    assert!(cleanup_directory.exists());
    assert!(displaced_directory.exists());

    let status = runner.request(json!({
        "id": 9262,
        "protocol_version": 1,
        "type": "status",
    }));
    assert_eq!(status["status"]["phase"], "failed", "{status}");
    for request in [
        json!({
            "id": 9263,
            "protocol_version": 1,
            "type": "interrupt",
        }),
        json!({
            "id": 9264,
            "protocol_version": 1,
            "type": "terminate",
            "deadlines": { "graceful_ms": 0, "force_ms": 10 },
        }),
        wait_request(/*id*/ 9265),
    ] {
        let response = runner.request(request);
        assert_eq!(response["error"]["code"], "invalid_state", "{response}");
        assert_eq!(response["error"]["target_started"], true, "{response}");
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("launch failed")),
            "{response}"
        );
    }

    std::fs::remove_dir(&cleanup_directory).expect("remove replacement cleanup directory");
    std::fs::remove_dir_all(&displaced_directory).expect("remove displaced cleanup directory");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_runner_sigkill_retires_the_observed_target_tree() {
    let target = fixture_target(&["spawn-session-escaping-descendant-and-wait", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let response = runner.request(default_launch(
        /*id*/ 927,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let root_process_id = response["root_process_id"]
        .as_i64()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .expect("macOS root process ID");
    let descendant_process_id = read_process_id(&mut io.stdout);
    let manager_process_id = wait_for_manager_process(runner.process_id(), root_process_id);

    runner.signal(libc::SIGKILL);
    assert_eq!(runner.wait_for_exit().signal(), Some(libc::SIGKILL));
    assert_eq!(unsafe { libc::kill(root_process_id, 0) }, 0);
    assert_processes_exit(
        &[root_process_id, descendant_process_id, manager_process_id],
        std::time::Duration::from_secs(5),
    );
    assert!(!cleanup_directory.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_runner_sigkill_restores_the_foreground_terminal() {
    let target = fixture_target(&["ready-then-wait"]);
    let (mut runner, mut master, launcher_process_group, mut launcher_release) =
        Runner::spawn_with_surviving_pty_launcher(&target);
    let response = runner.request(default_launch(
        /*id*/ 932,
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
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let root_process_id = response["root_process_id"]
        .as_i64()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .expect("macOS root process ID");
    let mut ready = [0];
    master.read_exact(&mut ready).expect("target readiness");
    assert_eq!(ready, *b"R");
    let manager_process_id = wait_for_manager_process(runner.process_id(), root_process_id);

    runner.signal(libc::SIGKILL);
    assert_processes_exit(
        &[runner.process_id() as libc::pid_t],
        std::time::Duration::from_secs(5),
    );
    assert_processes_exit(&[root_process_id], std::time::Duration::from_secs(5));
    assert_processes_exit(&[manager_process_id], std::time::Duration::from_secs(5));
    assert_eq!(
        unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
        launcher_process_group
    );
    launcher_release
        .write_all(b"x")
        .expect("release surviving terminal launcher");
    assert_eq!(runner.wait_for_exit().signal(), Some(libc::SIGKILL));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_runner_recovers_from_lifetime_manager_crash() {
    let target = fixture_target(&["spawn-session-escaping-descendant-and-wait", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let response = runner.request(default_launch(
        /*id*/ 928,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let root_process_id = response["root_process_id"]
        .as_i64()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .expect("macOS root process ID");
    let descendant_process_id = read_process_id(&mut io.stdout);
    let manager_process_id = wait_for_manager_process(runner.process_id(), root_process_id);

    assert_eq!(unsafe { libc::kill(manager_process_id, libc::SIGKILL) }, 0);
    let outcome = runner.request(wait_request(/*id*/ 929));
    assert_eq!(outcome["type"], "final", "{outcome}");
    assert_eq!(outcome["outcome"]["target"]["kind"], "signaled");
    assert_eq!(outcome["outcome"]["target"]["signal"], libc::SIGKILL);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert_eq!(unsafe { libc::kill(root_process_id, 0) }, 0);
    assert_processes_exit(
        &[descendant_process_id, manager_process_id],
        std::time::Duration::from_secs(5),
    );
    assert!(!cleanup_directory.exists());
    runner.close_control();
    assert!(runner.wait_for_exit().success());
    assert_processes_exit(&[root_process_id], std::time::Duration::from_secs(1));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_stop_notification_does_not_replace_the_target_exit() {
    let target = fixture_target(&["stop-then-exit", "86"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 9281,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let root_process_id = response["root_process_id"]
        .as_i64()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .expect("macOS root process ID");
    let runner_process_id = libc::pid_t::try_from(runner.process_id()).expect("runner process ID");
    let mut ready = [0];
    io.stdout.read_exact(&mut ready).expect("target readiness");
    assert_eq!(ready, *b"R");
    wait_for_process_status(root_process_id, libc::SSTOP);
    assert_eq!(unsafe { libc::kill(root_process_id, libc::SIGCONT) }, 0);

    assert_target_exit(&runner.request(wait_request(/*id*/ 9282)), /*code*/ 86);
    runner.close_control();
    wait_for_process_status(runner_process_id, libc::SZOMB);
    assert!(runner.wait_for_exit().success());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_control_loss_recovers_from_an_unresponsive_lifetime_manager() {
    let target = fixture_target(&["spawn-session-escaping-descendant-and-wait", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let response = runner.request(default_launch(
        /*id*/ 9291,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let root_process_id = response["root_process_id"]
        .as_i64()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .expect("macOS root process ID");
    let descendant_process_id = read_process_id(&mut io.stdout);
    let manager_process_id = wait_for_manager_process(runner.process_id(), root_process_id);
    assert_eq!(unsafe { libc::kill(manager_process_id, libc::SIGSTOP) }, 0);

    runner.close_control();
    assert!(runner.wait_for_exit().success());
    assert_processes_exit(
        &[root_process_id, descendant_process_id, manager_process_id],
        std::time::Duration::from_secs(1),
    );
    assert!(!cleanup_directory.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_manager_crash_fallback_restores_the_foreground_terminal() {
    let target = fixture_target(&["ready-then-wait"]);
    let (mut runner, mut master) = Runner::spawn_with_inherited_pty(&target);
    let runner_process_group = runner.process_id() as libc::pid_t;
    let response = runner.request(default_launch(
        /*id*/ 933,
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
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let root_process_id = response["root_process_id"]
        .as_i64()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .expect("macOS root process ID");
    let mut ready = [0];
    master.read_exact(&mut ready).expect("target readiness");
    assert_eq!(ready, *b"R");
    let manager_process_id = wait_for_manager_process(runner.process_id(), root_process_id);
    assert_eq!(unsafe { libc::kill(manager_process_id, libc::SIGKILL) }, 0);

    let terminate = runner.request(json!({
        "type": "terminate",
        "id": 934,
        "protocol_version": support::PROTOCOL_VERSION,
        "deadlines": { "graceful_ms": 0, "force_ms": 2000 },
    }));
    assert_eq!(terminate["type"], "acknowledged", "{terminate}");
    let outcome = runner.request(wait_request(/*id*/ 935));
    assert_eq!(outcome["type"], "final", "{outcome}");
    assert_eq!(
        unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
        runner_process_group
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_keeps_the_root_waitable_until_observed_tree_retirement() {
    let target = fixture_target(&["spawn-descendant-and-sleep", "10000", "100"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 936,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(1000);
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let root_process_id = response["root_process_id"]
        .as_i64()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .expect("macOS root process ID");
    let _descendant_process_id = read_process_id(&mut io.stdout);
    wait_for_phase(&mut runner, "root_exited", /*id*/ 937);
    assert_eq!(unsafe { libc::kill(root_process_id, 0) }, 0);
    assert_eq!(
        runner.request(json!({
            "type": "interrupt",
            "id": 9371,
            "protocol_version": support::PROTOCOL_VERSION,
        }))["type"],
        "acknowledged"
    );

    let outcome = runner.request(wait_request(/*id*/ 938));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(unsafe { libc::kill(root_process_id, 0) }, 0);
    assert_eq!(
        runner.request(json!({
            "type": "interrupt",
            "id": 9381,
            "protocol_version": support::PROTOCOL_VERSION,
        }))["type"],
        "acknowledged"
    );
    runner.close_control();
    assert!(runner.wait_for_exit().success());
    assert_eq!(unsafe { libc::kill(root_process_id, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_terminate_after_root_exit_reaches_a_detached_descendant() {
    let target = fixture_target(&["spawn-session-escaping-signal-aware-descendant", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let mut launch = default_launch(
        /*id*/ 939,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    );
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(5000);
    let response = runner.request(launch);
    assert_eq!(response["type"], "launch_accepted", "{response}");
    let descendant_process_id = read_process_id(&mut io.stdout);
    let mut ready = [0];
    io.stdout
        .read_exact(&mut ready)
        .expect("descendant readiness");
    assert_eq!(ready, *b"C");
    wait_for_phase(&mut runner, "root_exited", /*id*/ 940);

    let terminate = runner.request(json!({
        "type": "terminate",
        "id": 941,
        "protocol_version": support::PROTOCOL_VERSION,
        "deadlines": { "graceful_ms": 500, "force_ms": 2000 },
    }));
    assert_eq!(terminate["type"], "acknowledged", "{terminate}");
    io.stdout
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .expect("set descendant signal timeout");
    let mut terminate_seen = [0];
    io.stdout
        .read_exact(&mut terminate_seen)
        .expect("detached descendant received graceful termination");
    assert_eq!(terminate_seen, *b"T");
    let outcome = runner.request(wait_request(/*id*/ 942));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], true);
    assert_processes_exit(&[descendant_process_id], std::time::Duration::from_secs(2));
}

#[cfg(target_os = "macos")]
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
fn linux_target_output_reaches_eof_when_the_pid_namespace_retires() {
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

    let outcome = runner.request(wait_request(/*id*/ 938));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], false);
    assert!(read_all(&mut io.stdout).is_empty());
    assert!(read_all(&mut io.stderr).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_pid_namespace_retires_a_session_escaping_descendant_with_the_root() {
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

    let outcome = runner.request(wait_request(/*id*/ 942));
    assert_target_exit(&outcome, /*code*/ 0);
    assert_eq!(outcome["outcome"]["retirement"]["complete"], true);
    assert_eq!(outcome["outcome"]["retirement"]["forced"], false);
    assert_eq!(outcome["outcome"]["infrastructure"]["error"], Value::Null);
    assert_eq!(
        outcome["outcome"]["infrastructure"]["cleanup_error"],
        Value::Null
    );
    assert!(io.stdin.write_all(b"D").is_err());
    assert!(read_all(&mut io.stdout).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_runner_sigkill_retires_the_pid_namespace() {
    let target = fixture_target(&["spawn-session-escaping-descendant-and-wait", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 943,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted", "{response}");

    let mut descendant_process_id = [0_u8; 4];
    io.stdout
        .read_exact(&mut descendant_process_id)
        .expect("read descendant process ID");
    assert_ne!(u32::from_be_bytes(descendant_process_id), 0);

    runner.signal(libc::SIGKILL);
    assert_eq!(runner.wait_for_exit().signal(), Some(libc::SIGKILL));
    drop(io.stdin);
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
    let timeout = std::time::Duration::from_secs(2);
    assert_eq!(
        stdout_receiver
            .recv_timeout(timeout)
            .expect("target stdout did not reach EOF after runner SIGKILL"),
        Vec::<u8>::new()
    );
    assert_eq!(
        stderr_receiver
            .recv_timeout(timeout)
            .expect("target stderr did not reach EOF after runner SIGKILL"),
        Vec::<u8>::new()
    );
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
    let target = fixture_target(&["echo-then-sleep", "10000"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let response = runner.request(default_launch(
        /*id*/ 100,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        json!([]),
        json!({ "mode": "denied" }),
        passed_streams(),
    ));
    assert_eq!(response["type"], "launch_accepted");
    io.stdin.write_all(b"R").expect("write target input");
    let mut echoed = [0_u8; 1];
    io.stdout
        .read_exact(&mut echoed)
        .expect("read target output");
    assert_eq!(echoed, *b"R");

    runner.close_control();
    assert!(runner.wait_for_exit().success());
    let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = read_all(&mut io.stdout);
        let _ = stdout_sender.send(result);
    });
    assert_eq!(
        stdout_receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("target stdout did not reach EOF after control loss"),
        Vec::<u8>::new()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn control_loss_exits_unsuccessfully_when_cleanup_cannot_be_claimed() {
    let target = fixture_target(&["ready-then-wait"]);
    let (mut runner, io) = Runner::spawn(&target, &[], /*with_io*/ true);
    let mut io = io.expect("target streams");
    let cleanup_directory = runner.cleanup_dir().to_path_buf();
    let displaced_directory = cleanup_directory.with_extension("control-loss-displaced");
    assert_eq!(
        runner.request(default_launch(
            /*id*/ 116,
            std::env::current_dir()
                .expect("current directory")
                .as_path(),
            json!([]),
            json!({ "mode": "denied" }),
            passed_streams(),
        ))["type"],
        "launch_accepted"
    );
    let mut ready = [0];
    io.stdout.read_exact(&mut ready).expect("target readiness");
    std::fs::rename(&cleanup_directory, &displaced_directory)
        .expect("displace target cleanup directory");
    std::fs::create_dir(&cleanup_directory).expect("replace target cleanup directory");

    runner.close_control();
    assert!(!runner.wait_for_exit().success());
    assert!(cleanup_directory.exists());
    assert!(displaced_directory.exists());
    std::fs::remove_dir(&cleanup_directory).expect("remove replacement cleanup directory");
    std::fs::remove_dir_all(&displaced_directory).expect("remove displaced cleanup directory");
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
    assert_eq!(
        outcome["outcome"]["retirement"]["complete"], true,
        "{outcome}"
    );
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
    assert_eq!(outcome["type"], "final", "{outcome}");
    assert_eq!(outcome["outcome"]["target"]["kind"], "exited");
    assert_eq!(outcome["outcome"]["target"]["code"], code);
    assert_eq!(
        outcome["outcome"]["retirement"]["complete"], true,
        "{outcome}"
    );
}

fn read_all(mut stream: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read stream to EOF");
    bytes
}

#[cfg(target_os = "macos")]
fn read_process_id(stream: &mut impl Read) -> libc::pid_t {
    let mut process_id = [0_u8; 4];
    stream
        .read_exact(&mut process_id)
        .expect("read process identifier");
    libc::pid_t::try_from(u32::from_be_bytes(process_id)).expect("native process identifier")
}

#[cfg(target_os = "macos")]
fn wait_for_phase(runner: &mut Runner, phase: &str, id: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = runner.request(json!({
            "type": "status",
            "id": id,
            "protocol_version": support::PROTOCOL_VERSION,
        }));
        if status["status"]["phase"] == phase {
            return;
        }
        assert_ne!(status["status"]["phase"], "retired", "{status}");
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn wait_for_manager_process(runner_process_id: u32, root_process_id: libc::pid_t) -> libc::pid_t {
    let runner_process_id = libc::pid_t::try_from(runner_process_id).expect("runner process ID");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let manager_processes = child_processes(runner_process_id)
            .into_iter()
            .filter(|process_id| *process_id != root_process_id)
            .collect::<Vec<_>>();
        assert!(
            manager_processes.len() <= 1,
            "multiple lifetime-manager candidates: {manager_processes:?}"
        );
        if let Some(process_id) = manager_processes.into_iter().next() {
            return process_id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "lifetime manager did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn wait_for_process_status(process_id: libc::pid_t, expected: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = unsafe {
            libc::proc_pidinfo(
                process_id,
                libc::PROC_PIDTBSDINFO,
                1,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
            )
        };
        assert_eq!(
            size as usize,
            std::mem::size_of::<libc::proc_bsdinfo>(),
            "inspect process {process_id}: {}",
            std::io::Error::last_os_error()
        );
        if unsafe { info.assume_init() }.pbi_status == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "process {process_id} did not reach status {expected}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn child_processes(parent: libc::pid_t) -> Vec<libc::pid_t> {
    let mut capacity = 8;
    loop {
        let mut children = vec![0; capacity];
        let count = unsafe {
            libc::proc_listchildpids(
                parent,
                children.as_mut_ptr().cast(),
                std::mem::size_of_val(children.as_slice()) as libc::c_int,
            )
        };
        assert!(count >= 0, "list child processes returned {count}");
        let count = count as usize;
        if count < capacity {
            children.truncate(count);
            return children;
        }
        capacity = capacity.saturating_mul(2).max(count + 8);
    }
}

#[cfg(target_os = "macos")]
fn assert_processes_exit(process_ids: &[libc::pid_t], timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let survivors = process_ids
            .iter()
            .copied()
            .filter(|process_id| unsafe { libc::kill(*process_id, 0) } == 0)
            .collect::<Vec<_>>();
        if survivors.is_empty() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            for process_id in &survivors {
                unsafe { libc::kill(*process_id, libc::SIGKILL) };
            }
            panic!("sandbox processes survived cleanup: {survivors:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
