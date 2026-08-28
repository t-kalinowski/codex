#![cfg(windows)]
#![allow(clippy::expect_used)]

mod windows_support;

use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use windows_support::AliasedRunner;
use windows_support::CompanionCompatibilityBehavior;
use windows_support::IncompatibleCompanion;
use windows_support::InputPipe;
use windows_support::OutputPipe;
use windows_support::Runner;
use windows_support::RunnerExecutable;
use windows_support::delete_one_standalone_wfp_filter;
use windows_support::hold_standalone_read_acl_mutex;
use windows_support::native_tests_enabled;
use windows_support::open_process_for_wait;
use windows_support::run_bootstrap;
use windows_support::run_bootstrap_with_duplicated_control_as_passed_stream;
use windows_support::run_bootstrap_with_duplicated_stdout_as_passed_stream;
use windows_support::wait_for_process_exit;

const PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME_SIZE: u64 = 1024 * 1024;

#[test]
fn discovery_reports_windows_contract_and_missing_companions_fail_closed() {
    let executable = RunnerExecutable::without_companions();
    let state = TempDir::new().expect("state directory");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let target = vec![
        fixture.into_os_string(),
        OsString::from("exit"),
        OsString::from("0"),
    ];
    let mut runner = Runner::spawn(&executable, state.path(), &target);
    let response = runner.request(json!({
        "type": "discover",
        "id": 1,
        "protocol_version": PROTOCOL_VERSION,
    }));

    assert_eq!(response["type"], "capabilities");
    assert_eq!(
        response["capabilities"]["protocol_version"],
        PROTOCOL_VERSION
    );
    assert_eq!(
        response["capabilities"]["maximum_frame_size"],
        MAX_FRAME_SIZE
    );
    assert_eq!(
        response["capabilities"]["runner_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(response["capabilities"]["operating_system"], "windows");
    assert_eq!(response["capabilities"]["backend"], "windows_elevated");
    assert_eq!(
        response["capabilities"]["codex_release_tag"],
        "rust-v0.150.1"
    );
    let expected_revision = git_revision();
    assert_eq!(
        response["capabilities"]["codex_source_revision"],
        expected_revision
    );
    assert_eq!(response["capabilities"]["setup"]["state"], "unavailable");
    assert_eq!(
        response["capabilities"]["required_companions"],
        json!([
            {
                "name": "windows sandbox setup",
                "relative_path": "codex-resources/codex-windows-sandbox-setup.exe",
                "required": true,
            },
            {
                "name": "elevated command runner",
                "relative_path": "codex-resources/codex-command-runner.exe",
                "required": true,
            },
        ])
    );

    let setup = setup_request(
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
    );
    let status = runner.request(json!({
        "type": "setup_status",
        "id": 2,
        "protocol_version": PROTOCOL_VERSION,
        "setup": setup,
    }));
    assert_eq!(status["type"], "setup_status");
    assert_eq!(status["setup"]["state"], "unavailable");

    let launch = runner.request(launch_request(
        3,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        null_streams(),
    ));
    assert_eq!(launch["type"], "error");
    assert_eq!(launch["error"]["code"], "companion_missing");
    assert_eq!(launch["error"]["target_started"], false);
}

#[test]
fn platform_minimal_is_reported_unsupported_and_rejected_before_target_start() {
    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let marker = workspace.join("target-started");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let target = vec![
        fixture.into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut runner = Runner::spawn(&executable, &state, &target);

    let discovery = runner.request(json!({
        "type": "discover",
        "id": 1,
        "protocol_version": PROTOCOL_VERSION,
    }));
    assert_eq!(
        discovery["capabilities"]["filesystem"]["platform_minimal"],
        false
    );
    assert_eq!(
        discovery["capabilities"]["filesystem"]["host_read_only"],
        true
    );

    let mut setup = setup_operation(2, "prepare", &workspace);
    setup["setup"]["filesystem"]["base"] = json!("platform_minimal");
    let setup_response = runner.request(setup);
    assert_eq!(setup_response["type"], "error");
    assert_eq!(setup_response["error"]["code"], "unsupported_policy");
    assert_eq!(setup_response["error"]["phase"], "validation");
    assert_eq!(setup_response["error"]["target_started"], false);

    let mut launch = launch_request(3, &workspace, null_streams());
    launch["launch"]["filesystem"]["base"] = json!("platform_minimal");
    let launch_response = runner.request(launch);
    assert_eq!(launch_response["type"], "error");
    assert_eq!(launch_response["error"]["code"], "unsupported_policy");
    assert_eq!(launch_response["error"]["phase"], "validation");
    assert_eq!(launch_response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn incompatible_setup_companion_fails_closed_before_target_start() {
    assert_incompatible_companion_fails_closed(IncompatibleCompanion::Setup);
}

#[test]
fn incompatible_command_runner_companion_fails_closed_before_target_start() {
    assert_incompatible_companion_fails_closed(IncompatibleCompanion::CommandRunner);
}

#[test]
fn companion_compatibility_queries_are_bounded_exact_and_tree_scoped() {
    for (behavior, expected_detail) in [
        (CompanionCompatibilityBehavior::Timeout, "timed out"),
        (CompanionCompatibilityBehavior::NoisyOutput, "incompatible"),
        (
            CompanionCompatibilityBehavior::OversizedOutput,
            "exceeded 1024 bytes",
        ),
        (
            CompanionCompatibilityBehavior::PipeHoldingDescendant,
            "timed out",
        ),
    ] {
        let (executable, descendant_process_id) =
            RunnerExecutable::with_misbehaving_command_runner(behavior);
        let descendant_waiter = matches!(
            behavior,
            CompanionCompatibilityBehavior::PipeHoldingDescendant
        )
        .then(|| {
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if let Ok(process_id) = std::fs::read_to_string(&descendant_process_id) {
                        let process_id = process_id
                            .parse()
                            .expect("compatibility descendant process ID");
                        break open_process_for_wait(process_id)
                            .expect("open compatibility descendant for wait");
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "compatibility helper did not report its descendant"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        });
        let state = TempDir::new().expect("state directory");
        let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        let target = vec![
            fixture.into_os_string(),
            OsString::from("exit"),
            OsString::from("0"),
        ];
        let mut runner = Runner::spawn(&executable, state.path(), &target);
        let started = std::time::Instant::now();
        let response = runner.request(json!({
            "type": "discover",
            "id": 1,
            "protocol_version": PROTOCOL_VERSION,
        }));

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "compatibility query was not bounded for {behavior:?}"
        );
        assert_eq!(response["type"], "capabilities");
        assert_eq!(response["capabilities"]["setup"]["state"], "unavailable");
        assert!(
            response["capabilities"]["setup"]["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(expected_detail)),
            "unexpected compatibility response for {behavior:?}: {response}"
        );
        if let Some(descendant_waiter) = descendant_waiter {
            let descendant = descendant_waiter
                .join()
                .expect("compatibility descendant observer");
            wait_for_process_exit(&descendant, Duration::from_secs(5))
                .expect("compatibility query descendant was not retired");
        }
    }
}

fn assert_incompatible_companion_fails_closed(companion: IncompatibleCompanion) {
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let executable = RunnerExecutable::with_incompatible_companion(companion);
    let root = TempDir::new().expect("policy root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let marker = workspace.join("target-started");
    let target = vec![
        fixture.into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut runner = Runner::spawn(&executable, &state, &target);

    let discovery = runner.request(json!({
        "type": "discover",
        "id": 1,
        "protocol_version": PROTOCOL_VERSION,
    }));
    assert_eq!(discovery["type"], "capabilities");
    assert_eq!(discovery["capabilities"]["setup"]["state"], "unavailable");
    assert_eq!(
        discovery["capabilities"]["filesystem"]["platform_minimal"],
        false
    );
    assert_eq!(discovery["capabilities"]["network"]["denied"], false);
    assert_eq!(discovery["capabilities"]["streams"]["inherited"], false);
    assert_eq!(
        discovery["capabilities"]["lifecycle"]["forced_termination"],
        false
    );
    assert!(
        discovery["capabilities"]["setup"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("incompatible")),
        "unexpected discovery response: {discovery}"
    );

    let launch = runner.request(launch_request_with_policy(
        2,
        &workspace,
        json!([{
            "path": workspace,
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(launch["type"], "error");
    assert_eq!(launch["error"]["code"], "companion_missing");
    assert_eq!(launch["error"]["target_started"], false);
    assert!(
        launch["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("incompatible")),
        "unexpected launch response: {launch}"
    );
    assert!(!marker.exists());
    assert_eq!(
        std::fs::read_dir(&state)
            .expect("read state directory")
            .count(),
        0
    );
}

#[test]
fn invalid_bootstrap_stream_handle_fails_before_protocol_startup() {
    let executable = RunnerExecutable::without_companions();
    let state = TempDir::new().expect("state directory");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let output = run_bootstrap(
        &executable,
        state.path(),
        &[fixture.into_os_string()],
        &[u64::MAX - 1],
        &[],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .expect("Unicode runner diagnostic")
            .contains("invalid native bootstrap endpoint")
    );
}

#[test]
fn duplicate_bootstrap_stream_handle_fails_before_protocol_startup() {
    let executable = RunnerExecutable::without_companions();
    let state = TempDir::new().expect("state directory");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let stream = OutputPipe::new();
    let handle = stream.writer_value();
    let output = run_bootstrap(
        &executable,
        state.path(),
        &[fixture.into_os_string()],
        &[handle, handle],
        &[handle],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .expect("Unicode runner diagnostic")
            .contains("distinct handle")
    );
}

#[test]
fn duplicated_control_object_fails_bootstrap_before_protocol_startup() {
    let executable = RunnerExecutable::without_companions();
    let state = TempDir::new().expect("state directory");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let output = run_bootstrap_with_duplicated_control_as_passed_stream(
        &executable,
        state.path(),
        &[fixture.into_os_string()],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .expect("Unicode runner diagnostic")
            .contains("private control handle cannot be used as a target stream")
    );
}

#[test]
fn duplicated_runner_stdout_object_fails_bootstrap_before_protocol_startup() {
    let executable = RunnerExecutable::without_companions();
    let state = TempDir::new().expect("state directory");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let output = run_bootstrap_with_duplicated_stdout_as_passed_stream(
        &executable,
        state.path(),
        &[fixture.into_os_string()],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .expect("Unicode runner diagnostic")
            .contains("cannot alias a runner standard stream")
    );
}

#[test]
fn replaced_inherited_standard_handle_fails_before_setup() {
    let executable = RunnerExecutable::with_companions();
    let state = TempDir::new().expect("state directory");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let target = vec![
        fixture.into_os_string(),
        OsString::from("exit"),
        OsString::from("0"),
    ];
    let (mut runner, inherited_stdout) =
        AliasedRunner::spawn_with_passed_stdout(&executable, state.path(), &target);

    assert_eq!(
        runner.request(json!({
            "type": "discover",
            "id": 1,
            "protocol_version": PROTOCOL_VERSION,
        }))["type"],
        "capabilities"
    );
    runner.close_inherited_handle(inherited_stdout);

    let mut streams = null_streams();
    streams["stdout"] = json!({"mode": "inherited"});
    let response = runner.request(launch_request(
        2,
        std::env::current_dir()
            .expect("current directory")
            .as_path(),
        streams,
    ));

    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["phase"], "validation");
    assert_eq!(response["error"]["target_started"], false);
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("changed since runner bootstrap")),
        "unexpected response: {response}"
    );
}

#[test]
fn duplicated_control_object_cannot_be_inherited_by_the_target() {
    if !native_tests_enabled() {
        return;
    }

    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let state = root.path().join("state");
    std::fs::create_dir(&state).expect("state directory");
    let marker = root.path().join("target-started");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let target = vec![
        fixture.into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut runner =
        AliasedRunner::spawn_with_duplicated_control_as_stdout(&executable, &state, &target);
    let response = runner.request(launch_request_with_policy(
        1,
        root.path(),
        json!([{
            "path": root.path(),
            "access": "write",
            "missing": "error",
        }]),
        json!({ "mode": "denied" }),
        json!({
            "stdin": { "mode": "null" },
            "stdout": { "mode": "inherited" },
            "stderr": { "mode": "null" },
        }),
    ));

    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["phase"], "validation");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

#[test]
fn filesystem_deny_covering_target_executable_fails_before_setup() {
    if !native_tests_enabled() {
        return;
    }

    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let denied = root.path().join("denied-target");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::create_dir(&workspace).expect("workspace directory");
    std::fs::create_dir(&denied).expect("denied target directory");
    let target_executable = denied.join("sandbox-fixture.exe");
    std::fs::copy(
        cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary"),
        &target_executable,
    )
    .expect("stage denied target executable");
    let marker = workspace.join("target-started");
    let target = vec![
        target_executable.into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut runner = Runner::spawn(&executable, &state, &target);
    let response = runner.request(launch_request_with_policy(
        1,
        &workspace,
        json!([
            {
                "path": workspace,
                "access": "write",
                "missing": "error",
            },
            {
                "path": denied,
                "access": "deny",
                "missing": "error",
            },
        ]),
        json!({ "mode": "denied" }),
        null_streams(),
    ));

    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["phase"], "validation");
    assert_eq!(response["error"]["target_started"], false);
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("target executable")),
        "{response}"
    );
    assert!(!marker.exists());
}

#[test]
fn launch_grants_only_the_exact_target_executable_read() {
    if !native_tests_enabled() {
        return;
    }

    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let target_directory = root.path().join("target");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::create_dir(&workspace).expect("workspace directory");
    std::fs::create_dir(&target_directory).expect("target directory");
    let target_executable = target_directory.join("sandbox-fixture.exe");
    std::fs::copy(
        cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary"),
        &target_executable,
    )
    .expect("stage target executable outside readable roots");
    let marker = workspace.join("target-started");
    let target = vec![
        target_executable.into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let rules = json!([{
        "path": workspace,
        "access": "write",
        "missing": "error",
    }]);
    let mut runner = Runner::spawn(&executable, &state, &target);
    let setup = runner.request(setup_operation_with_policy(
        1,
        "prepare",
        &workspace,
        rules.clone(),
        json!({ "mode": "denied" }),
    ));
    assert_eq!(setup["type"], "setup_completed", "{setup}");
    let launch = runner.request(launch_request_with_policy(
        2,
        &workspace,
        rules,
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(launch["type"], "launch_accepted");
    assert_successful_outcome(&runner.request(wait_request(3)), 0);
    assert_eq!(
        std::fs::read_to_string(marker).expect("read marker"),
        "started"
    );
}

#[test]
fn helper_ready_loss_consumes_the_generation_without_executing_the_target() {
    if !native_tests_enabled() {
        return;
    }

    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let executable = RunnerExecutable::with_ready_loss_companion(&fixture);
    let root = TempDir::new().expect("policy root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let marker = workspace.join("target-started");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut runner = Runner::spawn(&executable, &state, &target);
    prepare_policy(
        &mut runner,
        &workspace,
        default_filesystem_rules(&workspace),
        json!({ "mode": "denied" }),
    );
    let response = runner.request(launch_request(1, &workspace, null_streams()));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "launch_failed");
    assert_eq!(response["error"]["phase"], "launch");
    assert_eq!(response["error"]["target_started"], true);
    assert!(!marker.exists());

    let second = runner.request(launch_request(2, &workspace, null_streams()));
    assert_eq!(second["type"], "error");
    assert_eq!(second["error"]["code"], "invalid_state");
    let outcome = runner.request(wait_request(3));
    assert_eq!(outcome["type"], "final");
    assert!(outcome["outcome"]["infrastructure"]["error"].is_string());
    assert!(!marker.exists());
}

#[test]
fn runner_owned_undeclared_handle_is_rejected_before_policy_preparation() {
    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let state = root.path().join("state");
    std::fs::create_dir(&state).expect("state directory");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let target = vec![
        fixture.into_os_string(),
        OsString::from("exit"),
        OsString::from("0"),
    ];
    let (mut runner, handle) =
        AliasedRunner::spawn_with_passed_stdout(&executable, &state, &target);
    let missing = root.path().join("missing-policy-path");
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": handle },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(launch_request_with_policy(
        4,
        root.path(),
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
            .is_some_and(|message| message.contains("was not declared at runner bootstrap")),
        "{response}"
    );
}

#[test]
fn native_setup_launch_streams_and_retirement() {
    if !native_tests_enabled() {
        return;
    }

    let executable = RunnerExecutable::with_companions();
    let policy_root = TempDir::new().expect("sandbox policy root");
    let workspace = policy_root.path().join("workspace");
    let state = policy_root.path().join("state");
    std::fs::create_dir_all(&workspace).expect("sandbox workspace");
    std::fs::create_dir_all(&state).expect("sandbox state directory");
    let fixture = workspace.join("sandbox-fixture.exe");
    std::fs::copy(
        cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary"),
        &fixture,
    )
    .expect("stage fixture");

    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("exit"),
        OsString::from("17"),
    ];
    let mut runner = Runner::spawn(&executable, &state, &target);
    let capabilities = runner.request(json!({
        "type": "discover",
        "id": 9,
        "protocol_version": PROTOCOL_VERSION,
    }));
    assert_eq!(capabilities["type"], "capabilities");
    assert_eq!(capabilities["capabilities"]["network"]["full_access"], true);
    assert_eq!(
        capabilities["capabilities"]["network"]["limited_access"],
        true
    );
    assert_eq!(
        capabilities["capabilities"]["lifecycle"],
        json!({
            "interrupt": false,
            "graceful_termination": false,
            "forced_termination": true,
            "root_exit_observation": true,
            "process_tree_supervision": true,
            "full_tree_retirement": true,
            "cleanup_after_root_exit": true,
            "control_loss_retires_target": true,
        })
    );
    assert_eq!(
        capabilities["capabilities"]["terminal"],
        json!({
            "inherited_terminal": false,
            "caller_supplied_pty": false,
            "controlling_terminal_reopen": false,
            "pty_creation_inside_sandbox": false,
            "host_device_isolation": false,
        })
    );
    let setup = setup_request(&workspace);
    let initial = runner.request(json!({
        "type": "setup_status",
        "id": 10,
        "protocol_version": PROTOCOL_VERSION,
        "setup": setup,
    }));
    assert_eq!(initial["type"], "setup_status");
    assert!(matches!(
        initial["setup"]["state"].as_str(),
        Some("ready" | "administrative_action_required")
    ));
    let prepared = runner.request(setup_operation(11, "prepare", &workspace));
    assert_eq!(prepared["type"], "setup_completed", "{prepared}");
    assert!(matches!(
        prepared["operation"].as_str(),
        Some("prepared" | "already_ready")
    ));
    let idempotent = runner.request(setup_operation(12, "prepare", &workspace));
    assert_eq!(idempotent["type"], "setup_completed", "{idempotent}");
    assert_eq!(idempotent["operation"], "already_ready");
    let refreshed = runner.request(setup_operation(13, "refresh", &workspace));
    assert_eq!(refreshed["type"], "setup_completed", "{refreshed}");
    assert_eq!(refreshed["operation"], "refreshed");
    let accepted = runner.request(launch_request(14, &workspace, null_streams()));
    assert_eq!(accepted["type"], "launch_accepted");
    assert_eq!(accepted["backend"], "windows_elevated");
    let outcome = runner.request(wait_request(15));
    assert_successful_outcome(&outcome, 17);
    drop(runner);

    let stdout = OutputPipe::new();
    let stderr = OutputPipe::new();
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("emit-large"),
        OsString::from("262144"),
    ];
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "passed_handle", "handle": stderr.writer_value() },
    });
    let mut runner = Runner::spawn_with_handles(
        &executable,
        &state,
        &target,
        &[stdout.writer_value(), stderr.writer_value()],
    );
    let mut stdout = stdout.into_reader();
    let mut stderr = stderr.into_reader();
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).expect("read target stdout");
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).expect("read target stderr");
        bytes
    });
    let accepted = runner.request(launch_request(20, &workspace, streams));
    assert_eq!(accepted["type"], "launch_accepted");
    let outcome = runner.request(wait_request(21));
    assert_successful_outcome(&outcome, 0);
    assert_eq!(
        stdout_reader.join().expect("stdout reader"),
        vec![b'o'; 262_144]
    );
    assert_eq!(
        stderr_reader.join().expect("stderr reader"),
        vec![b'e'; 262_144]
    );
    drop(runner);

    let stdout = OutputPipe::new();
    let native_argument = OsString::from_wide(&[0xd800, b'x' as u16]);
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("argv"),
        OsString::from("-leading"),
        OsString::from("with spaces"),
        OsString::from("key=value"),
        OsString::new(),
        OsString::from("&echo not-a-shell"),
        native_argument.clone(),
    ];
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let mut runner =
        Runner::spawn_with_handles(&executable, &state, &target, &[stdout.writer_value()]);
    let mut output = stdout.into_reader();
    assert_eq!(
        runner.request(launch_request(30, &workspace, streams))["type"],
        "launch_accepted"
    );
    assert_successful_outcome(&runner.request(wait_request(31)), 0);
    let mut bytes = Vec::new();
    output
        .read_to_end(&mut bytes)
        .expect("read native arguments");
    assert_eq!(
        decode_native_values(&bytes),
        vec![
            native_bytes(OsStr::new("-leading")),
            native_bytes(OsStr::new("with spaces")),
            native_bytes(OsStr::new("key=value")),
            Vec::new(),
            native_bytes(OsStr::new("&echo not-a-shell")),
            native_bytes(&native_argument),
        ]
    );
    drop(runner);

    let stdout = OutputPipe::new();
    let environment_value = OsString::from_wide(&[0xd800, b'e' as u16, b'n' as u16, b'v' as u16]);
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("environment"),
        OsString::from("TARGET_NATIVE_ENV_VALUE"),
    ];
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let mut runner = Runner::spawn_with_environment(
        &executable,
        &state,
        &target,
        &[(
            OsString::from("TARGET_NATIVE_ENV_VALUE"),
            environment_value.clone(),
        )],
        &[stdout.writer_value()],
    );
    let mut output = stdout.into_reader();
    assert_eq!(
        runner.request(launch_request(40, &workspace, streams))["type"],
        "launch_accepted"
    );
    assert_successful_outcome(&runner.request(wait_request(41)), 0);
    let mut bytes = Vec::new();
    output
        .read_to_end(&mut bytes)
        .expect("read native environment value");
    assert_eq!(
        decode_native_values(&bytes),
        vec![native_bytes(&environment_value)]
    );
    drop(runner);

    let stdout = OutputPipe::new();
    let private_keys = [
        OsString::from("cOdEx_NeTwOrK_PrOxY_AcTiVe"),
        OsString::from("CoDeX_WiNdOwS_SaNdBoX_PrOxY_PoRtS"),
        OsString::from("cOdEx_Ca_CeRtIfIcAtE"),
    ];
    let target = std::iter::once(fixture.as_os_str().to_owned())
        .chain(std::iter::once(OsString::from("environment")))
        .chain(private_keys.iter().cloned())
        .collect::<Vec<_>>();
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let private_environment = private_keys
        .iter()
        .cloned()
        .map(|key| (key, OsString::from("private")))
        .collect::<Vec<_>>();
    let mut runner = Runner::spawn_with_environment(
        &executable,
        &state,
        &target,
        &private_environment,
        &[stdout.writer_value()],
    );
    let mut output = stdout.into_reader();
    assert_eq!(
        runner.request(launch_request(42, &workspace, streams))["type"],
        "launch_accepted"
    );
    assert_successful_outcome(&runner.request(wait_request(43)), 0);
    let mut bytes = Vec::new();
    output
        .read_to_end(&mut bytes)
        .expect("read private environment projection");
    assert_eq!(
        decode_optional_native_values(&bytes),
        vec![None, None, None]
    );
    drop(runner);

    exercise_invalid_passed_stream(&executable, &state, &workspace, &fixture);
    exercise_control_stream_collision(&executable, &state, &workspace, &fixture);
    exercise_passed_inherited_stream_collision(&executable, &state, &workspace, &fixture);
    exercise_inherited_stream_ownership(&executable, &state, &workspace, &fixture);
    exercise_standard_input_contracts(&executable, &state, &workspace, &fixture);
    exercise_private_desktop(&executable, &state, &workspace, &fixture);

    exercise_global_policy_lease(&executable, &state, &workspace, &fixture);
    exercise_launch_revalidates_ignored_setup_paths(&executable, &state, &workspace, &fixture);
    exercise_network_contracts(&executable, &state, &workspace, &fixture);
    exercise_filesystem_contracts(
        &executable,
        policy_root.path(),
        &state,
        &workspace,
        &fixture,
    );
    exercise_lifecycle_contracts(&executable, &state, &workspace, &fixture);
}

#[test]
fn setup_detects_credentials_rotated_by_another_state_directory() {
    if !native_tests_enabled() {
        return;
    }

    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let workspace = root.path().join("workspace");
    let state_a = root.path().join("state-a");
    let state_b = root.path().join("state-b");
    std::fs::create_dir(&workspace).expect("workspace directory");
    std::fs::create_dir(&state_a).expect("first state directory");
    std::fs::create_dir(&state_b).expect("second state directory");
    let fixture = workspace.join("sandbox-fixture.exe");
    std::fs::copy(
        cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary"),
        &fixture,
    )
    .expect("stage target fixture");
    let target = vec![
        fixture.into_os_string(),
        OsString::from("exit"),
        OsString::from("0"),
    ];

    let mut runner_a = Runner::spawn(&executable, &state_a, &target);
    let prepared_a = runner_a.request(setup_operation(1, "prepare", &workspace));
    assert_eq!(prepared_a["type"], "setup_completed", "{prepared_a}");

    let mut runner_b = Runner::spawn(&executable, &state_b, &target);
    let prepared_b = runner_b.request(setup_operation(1, "prepare", &workspace));
    assert_eq!(prepared_b["type"], "setup_completed", "{prepared_b}");
    drop(runner_b);

    let status = runner_a.request(json!({
        "type": "setup_status",
        "id": 2,
        "protocol_version": PROTOCOL_VERSION,
        "setup": setup_request(&workspace),
    }));
    assert_eq!(status["type"], "setup_status", "{status}");
    assert_eq!(
        status["setup"]["state"],
        "administrative_action_required",
        "{status}"
    );
    assert!(
        status["setup"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("stale")),
        "{status}"
    );

    let repaired = runner_a.request(setup_operation(3, "prepare", &workspace));
    assert_eq!(repaired["type"], "setup_completed", "{repaired}");
    assert_eq!(repaired["operation"], "prepared");
    assert_eq!(
        runner_a.request(launch_request(4, &workspace, null_streams()))["type"],
        "launch_accepted"
    );
    assert_successful_outcome(&runner_a.request(wait_request(5)), 0);
}

#[test]
fn read_acl_contention_fails_before_target_start() {
    if !native_tests_enabled() {
        return;
    }

    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace directory");
    std::fs::create_dir(&state).expect("state directory");
    let marker = workspace.join("target-started");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let rules = json!([{
        "path": workspace,
        "access": "write",
        "missing": "error",
    }]);
    let _read_acl_mutex = hold_standalone_read_acl_mutex();
    let mut runner = Runner::spawn(&executable, &state, &target);

    let setup = runner.request(setup_operation_with_policy(
        1,
        "prepare",
        &workspace,
        rules.clone(),
        json!({ "mode": "denied" }),
    ));
    assert_eq!(setup["type"], "error", "{setup}");
    assert_eq!(setup["error"]["code"], "setup_failed", "{setup}");
    assert_eq!(setup["error"]["target_started"], false, "{setup}");

    let launch = runner.request(launch_request_with_policy(
        2,
        &workspace,
        rules,
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(launch["type"], "error", "{launch}");
    assert_eq!(launch["error"]["code"], "setup_failed", "{launch}");
    assert_eq!(launch["error"]["target_started"], false, "{launch}");
    assert!(!marker.exists());
}

#[test]
fn missing_standalone_wfp_filter_prevents_launch() {
    if !native_tests_enabled() {
        return;
    }

    let executable = RunnerExecutable::with_companions();
    let root = TempDir::new().expect("policy root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace directory");
    std::fs::create_dir(&state).expect("state directory");
    let marker = workspace.join("target-started");
    let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut runner = Runner::spawn(&executable, &state, &target);
    let prepared = runner.request(setup_operation(1, "prepare", &workspace));
    assert_eq!(prepared["type"], "setup_completed", "{prepared}");
    delete_one_standalone_wfp_filter();

    let status = runner.request(json!({
        "type": "setup_status",
        "id": 2,
        "protocol_version": PROTOCOL_VERSION,
        "setup": setup_request(&workspace),
    }));
    assert_eq!(status["type"], "setup_status", "{status}");
    assert_eq!(
        status["setup"]["state"],
        "administrative_action_required",
        "{status}"
    );

    let launch = runner.request(launch_request(3, &workspace, null_streams()));
    if launch["type"] == "launch_accepted" {
        let _ = runner.request(wait_request(4));
    }
    let namespace = codex_windows_sandbox::WindowsSandboxPolicyNamespace::McpConsole;
    let restored = codex_windows_sandbox::install_wfp_filters_for_account_in_namespace(
        namespace.offline_username(),
        namespace,
    );
    assert!(restored.is_ok(), "restore standalone WFP filters: {restored:?}");
    assert_eq!(launch["type"], "error", "{launch}");
    assert_eq!(launch["error"]["code"], "launch_failed", "{launch}");
    assert_eq!(launch["error"]["target_started"], false, "{launch}");
    assert!(!marker.exists());
}

fn exercise_global_policy_lease(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let sleeping_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("sleep"),
        OsString::from("30000"),
    ];
    let mut owner = Runner::spawn(executable, state, &sleeping_target);
    prepare_policy(
        &mut owner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    assert_eq!(
        owner.request(launch_request(2, workspace, null_streams()))["type"],
        "launch_accepted"
    );

    let drift_marker = workspace.join("stale-global-policy-target-started");
    let waiting_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        drift_marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut waiting = Runner::spawn(executable, state, &waiting_target);
    let mut local_binding = managed_network(json!([]), json!([]));
    local_binding["local_binding"] = json!(true);
    local_binding["loopback"] = json!("allow");
    let setup_request = setup_operation_with_policy(
        1,
        "prepare",
        workspace,
        default_filesystem_rules(workspace),
        local_binding.clone(),
    );
    let busy = waiting.request(setup_request.clone());
    assert_eq!(busy["type"], "error");
    assert_eq!(busy["error"]["code"], "setup_failed");
    assert_eq!(busy["error"]["phase"], "setup");
    assert_eq!(busy["error"]["target_started"], false);
    assert!(
        busy["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Windows sandbox policy generation is active")),
        "{busy}"
    );

    assert_eq!(
        owner.request(json!({
            "type": "terminate",
            "id": 3,
            "protocol_version": PROTOCOL_VERSION,
            "deadlines": { "graceful_ms": 0, "force_ms": 5000 },
        }))["type"],
        "acknowledged"
    );
    assert_eq!(owner.request(wait_request(4))["type"], "final");

    let prepared = waiting.request(setup_request);
    assert_eq!(prepared["type"], "setup_completed");
    assert!(matches!(
        prepared["operation"].as_str(),
        Some("prepared" | "already_ready")
    ));

    let foreign_state = state
        .parent()
        .expect("state parent directory")
        .join("foreign-policy-state");
    std::fs::create_dir(&foreign_state).expect("foreign policy state directory");
    let foreign_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("exit"),
        OsString::from("0"),
    ];
    let mut foreign = Runner::spawn(executable, &foreign_state, &foreign_target);
    prepare_policy(
        &mut foreign,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );

    let status = waiting.request(json!({
        "type": "setup_status",
        "id": 2,
        "protocol_version": PROTOCOL_VERSION,
        "setup": setup_request_with_policy(
            workspace,
            default_filesystem_rules(workspace),
            local_binding.clone(),
        ),
    }));
    assert_eq!(status["type"], "setup_status");
    assert_eq!(status["setup"]["state"], "administrative_action_required");
    assert!(
        status["setup"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("firewall policy requires refresh")),
        "{status}"
    );

    let launch = waiting.request(launch_request_with_policy(
        3,
        workspace,
        default_filesystem_rules(workspace),
        local_binding,
        null_streams(),
    ));
    assert_eq!(launch["type"], "error");
    assert_eq!(launch["error"]["target_started"], false);
    assert!(
        launch["error"]["message"].as_str().is_some_and(
            |message| message.contains("machine-global Windows sandbox firewall policy")
        ),
        "{launch}"
    );
    assert!(!drift_marker.exists());
}

fn exercise_invalid_passed_stream(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let missing = workspace.join("invalid-handle-missing-policy-path");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("exit"),
        OsString::from("0"),
    ];
    let mut runner = Runner::spawn(executable, state, &target);
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": u64::MAX - 1 },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(launch_request_with_policy(
        44,
        workspace,
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

fn exercise_passed_inherited_stream_collision(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let marker = workspace.join("passed-inherited-target-started");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let (mut runner, stdout_handle) =
        AliasedRunner::spawn_with_passed_stdout(executable, state, &target);
    let rules = json!([{
        "path": workspace,
        "access": "write",
        "missing": "error",
    }]);
    let setup = runner.request(setup_operation_with_policy(
        46,
        "prepare",
        workspace,
        rules.clone(),
        json!({ "mode": "denied" }),
    ));
    assert_eq!(setup["type"], "setup_completed");
    assert!(matches!(
        setup["operation"].as_str(),
        Some("prepared" | "already_ready")
    ));
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "passed_handle", "handle": stdout_handle },
    });
    let response = runner.request(launch_request_with_policy(
        47,
        workspace,
        rules,
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

fn exercise_control_stream_collision(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let marker = workspace.join("control-stream-target-started");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let mut runner = AliasedRunner::spawn_with_control_as_stdout(executable, state, &target);
    let rules = json!([{
        "path": workspace,
        "access": "write",
        "missing": "error",
    }]);
    let setup = runner.request(setup_operation_with_policy(
        44,
        "prepare",
        workspace,
        rules.clone(),
        json!({ "mode": "denied" }),
    ));
    assert_eq!(setup["type"], "setup_completed");
    assert!(matches!(
        setup["operation"].as_str(),
        Some("prepared" | "already_ready")
    ));
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "null" },
    });
    let response = runner.request(launch_request_with_policy(
        45,
        workspace,
        rules,
        json!({ "mode": "denied" }),
        streams,
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
    assert_eq!(response["error"]["target_started"], false);
    assert!(!marker.exists());
}

fn exercise_inherited_stream_ownership(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("emit-large"),
        OsString::from("1"),
    ];
    let (mut runner, stdout, stderr) =
        Runner::spawn_with_inherited_output(executable, state, &target);
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "inherited" },
    });
    assert_eq!(
        runner.request(launch_request(50, workspace, streams))["type"],
        "launch_accepted"
    );
    assert_successful_outcome(&runner.request(wait_request(51)), 0);

    let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stdout_sender.send(result);
    });
    let (stderr_sender, stderr_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stderr_sender.send(result);
    });
    let timeout = Duration::from_secs(2);
    let stdout = stdout_receiver.recv_timeout(timeout);
    let stderr = stderr_receiver.recv_timeout(timeout);
    if stdout.is_err() || stderr.is_err() {
        runner.close_control();
        let _ = runner.wait_for_exit(Duration::from_secs(10));
    }
    assert_eq!(
        stdout
            .expect("inherited stdout did not reach EOF while runner remained resident")
            .expect("read inherited stdout"),
        b"o"
    );
    assert_eq!(
        stderr
            .expect("inherited stderr did not reach EOF while runner remained resident")
            .expect("read inherited stderr"),
        b"e"
    );
    assert_eq!(
        runner.request(json!({
            "type": "status",
            "id": 52,
            "protocol_version": PROTOCOL_VERSION,
        }))["status"]["phase"],
        "retired"
    );
}

fn exercise_standard_input_contracts(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let expected = b"\0passed\xffstdin\r\n";
    let stdin = InputPipe::new();
    let stdout = OutputPipe::new();
    let target = vec![fixture.as_os_str().to_owned(), OsString::from("copy")];
    let streams = json!({
        "stdin": { "mode": "passed_handle", "handle": stdin.reader_value() },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let mut runner = Runner::spawn_with_handles(
        executable,
        state,
        &target,
        &[stdin.reader_value(), stdout.writer_value()],
    );
    let mut target_stdin = stdin.into_writer();
    let mut target_stdout = stdout.into_reader();
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    assert_eq!(
        runner.request(launch_request(2, workspace, streams))["type"],
        "launch_accepted"
    );
    target_stdin
        .write_all(expected)
        .expect("write passed target stdin");
    drop(target_stdin);
    assert_successful_outcome(&runner.request(wait_request(3)), 0);
    let mut output = Vec::new();
    target_stdout
        .read_to_end(&mut output)
        .expect("read passed target stdout");
    assert_eq!(output, expected);
    drop(runner);

    let expected = b"\0inherited\xffstdin\r\n";
    let (mut runner, mut target_stdin, mut target_stdout) =
        Runner::spawn_with_inherited_input_and_output(executable, state, &target);
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    let streams = json!({
        "stdin": { "mode": "inherited" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "null" },
    });
    assert_eq!(
        runner.request(launch_request(4, workspace, streams))["type"],
        "launch_accepted"
    );
    target_stdin
        .write_all(expected)
        .expect("write inherited target stdin");
    drop(target_stdin);
    assert_successful_outcome(&runner.request(wait_request(5)), 0);
    let mut output = Vec::new();
    target_stdout
        .read_to_end(&mut output)
        .expect("read inherited target stdout");
    assert_eq!(output, expected);
}

fn exercise_private_desktop(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("exit"),
        OsString::from("0"),
    ];
    let mut runner = Runner::spawn(executable, state, &target);
    let mut setup = setup_operation(1, "prepare", workspace);
    setup["setup"]["platform_extensions"] = json!({
        "windows": { "private_desktop": true },
    });
    let response = runner.request(setup);
    assert_eq!(response["type"], "setup_completed", "{response}");
    let mut launch = launch_request(2, workspace, null_streams());
    launch["launch"]["platform_extensions"] = json!({
        "windows": { "private_desktop": true },
    });
    assert_eq!(runner.request(launch)["type"], "launch_accepted");
    assert_successful_outcome(&runner.request(wait_request(3)), 0);
}

fn exercise_network_contracts(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let denied_listener = TcpListener::bind("127.0.0.1:0").expect("denied-network listener");
    let denied_port = denied_listener
        .local_addr()
        .expect("denied-network address")
        .port()
        .to_string();
    let denied_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("connect"),
        OsString::from("127.0.0.1"),
        OsString::from(denied_port),
    ];
    let denied_outcome = run_null_target(
        executable,
        state,
        &denied_target,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    assert_successful_outcome(&denied_outcome, 73);
    denied_listener
        .set_nonblocking(true)
        .expect("make denied-network listener nonblocking");
    assert_eq!(
        denied_listener
            .accept()
            .expect_err("denied target must not reach listener")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );

    let unrestricted_listener =
        TcpListener::bind("127.0.0.1:0").expect("unrestricted-network listener");
    let unrestricted_address = unrestricted_listener
        .local_addr()
        .expect("unrestricted-network address");
    let unrestricted_port = unrestricted_address.port().to_string();
    let unrestricted_accept = std::thread::spawn(move || unrestricted_listener.accept());
    let unrestricted_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("connect"),
        OsString::from("127.0.0.1"),
        OsString::from(unrestricted_port),
    ];
    let unrestricted_outcome = run_null_target(
        executable,
        state,
        &unrestricted_target,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "unrestricted" }),
    );
    if target_exit_code(&unrestricted_outcome) != Some(0) {
        let _ = TcpStream::connect(unrestricted_address);
    }
    unrestricted_accept
        .join()
        .expect("unrestricted accept thread")
        .expect("unrestricted target connection");
    assert_successful_outcome(&unrestricted_outcome, 0);

    let origin = TcpListener::bind("127.0.0.1:0").expect("managed-network origin");
    let origin_address = origin.local_addr().expect("managed-network origin address");
    let origin_port = origin_address.port().to_string();
    let origin_server = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().expect("accept managed proxy request");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
    });
    let managed_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("http-get"),
        OsString::from("localhost"),
        OsString::from(origin_port),
    ];
    let managed_outcome = run_null_target(
        executable,
        state,
        &managed_target,
        workspace,
        default_filesystem_rules(workspace),
        managed_network(json!(["localhost"]), json!([])),
    );
    if target_exit_code(&managed_outcome) != Some(0) {
        let _ = TcpStream::connect(origin_address);
    }
    origin_server.join().expect("managed-network origin server");
    assert_successful_outcome(&managed_outcome, 0);

    let denied_domain_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("http-get"),
        OsString::from("localhost"),
        OsString::from("9"),
    ];
    let denied_domain_outcome = run_null_target(
        executable,
        state,
        &denied_domain_target,
        workspace,
        default_filesystem_rules(workspace),
        managed_network(json!(["localhost"]), json!(["localhost"])),
    );
    assert_successful_outcome(&denied_domain_outcome, 73);

    let direct_listener = TcpListener::bind("127.0.0.1:0").expect("managed direct-egress listener");
    let direct_port = direct_listener
        .local_addr()
        .expect("managed direct-egress address")
        .port()
        .to_string();
    let direct_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("connect"),
        OsString::from("127.0.0.1"),
        OsString::from(direct_port),
    ];
    let direct_outcome = run_null_target(
        executable,
        state,
        &direct_target,
        workspace,
        default_filesystem_rules(workspace),
        managed_network(json!(["localhost"]), json!([])),
    );
    direct_listener
        .set_nonblocking(true)
        .expect("make direct-egress listener nonblocking");
    assert_eq!(
        direct_listener
            .accept()
            .expect_err("managed target must not bypass the proxy")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_successful_outcome(&direct_outcome, 73);

    let bind_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("bind"),
        OsString::from("127.0.0.1"),
        OsString::from("0"),
    ];
    let bind_outcome = run_null_target(
        executable,
        state,
        &bind_target,
        workspace,
        default_filesystem_rules(workspace),
        managed_network(json!(["localhost"]), json!([])),
    );
    assert_successful_outcome(&bind_outcome, 73);

    exercise_managed_environment(executable, state, workspace, fixture);

    exercise_managed_limited_access(executable, state, workspace, fixture);
    exercise_managed_redirect_policy(executable, state, workspace, fixture);
    exercise_managed_socks(executable, state, workspace, fixture);
    exercise_managed_upstream_proxy(executable, state, workspace, fixture);
    exercise_managed_loopback_policy(executable, state, workspace, fixture);
    exercise_unsupported_managed_network_policy(executable, state, workspace, fixture);
}

fn exercise_managed_environment(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    const HTTP_PROXY_KEYS: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "YARN_HTTP_PROXY",
        "YARN_HTTPS_PROXY",
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
    ];
    const NO_PROXY_KEYS: &[&str] = &[
        "NO_PROXY",
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
        "FTP_PROXY",
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
    let environment_target = std::iter::once(fixture.as_os_str().to_owned())
        .chain(std::iter::once(OsString::from("environment-entries")))
        .chain(keys.iter().map(OsString::from))
        .collect::<Vec<_>>();
    let stdout = OutputPipe::new();
    let streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let mut runner = Runner::spawn_with_environment(
        executable,
        state,
        &environment_target,
        &[
            (
                OsString::from("HTTP_PROXY"),
                OsString::from("http://stale.invalid:1"),
            ),
            (OsString::from("NO_PROXY"), OsString::from("*")),
            (
                OsString::from("ALL_PROXY"),
                OsString::from("socks5h://stale.invalid:2"),
            ),
            (
                OsString::from("SSL_CERT_FILE"),
                OsString::from(r"C:\trusted\caller-ca.pem"),
            ),
            (
                OsString::from("CODEX_CA_CERTIFICATE"),
                OsString::from(r"C:\private\managed-ca.pem"),
            ),
            (
                OsString::from("CODEX_WINDOWS_SANDBOX_PROXY_PORTS"),
                OsString::from("1234"),
            ),
            (
                OsString::from("CODEX_HOME"),
                OsString::from(r"C:\private\codex-home"),
            ),
            (
                OsString::from("MCP_CONSOLE_SANDBOX_CONTROL"),
                OsString::from("private"),
            ),
            (
                OsString::from("CARGO_BIN_EXE_PRIVATE_HELPER"),
                OsString::from(r"C:\private\helper.exe"),
            ),
        ],
        &[stdout.writer_value()],
    );
    let mut output = stdout.into_reader();
    let rules = default_filesystem_rules(workspace);
    let mut network = managed_network(json!(["localhost"]), json!([]));
    network["socks"] = json!(true);
    prepare_policy(&mut runner, workspace, rules.clone(), network.clone());
    assert_eq!(
        runner.request(launch_request_with_policy(
            2, workspace, rules, network, streams,
        ))["type"],
        "launch_accepted"
    );
    assert_successful_outcome(&runner.request(wait_request(3)), 0);
    let mut bytes = Vec::new();
    output
        .read_to_end(&mut bytes)
        .expect("read managed proxy environment");
    let values = decode_native_values(&bytes);
    assert_eq!(values.len() % 2, 0);
    let entry_count = values.len() / 2;
    let environment = values
        .chunks_exact(2)
        .map(|entry| {
            let key = decode_native_string(&entry[0]);
            assert_eq!(key, key.to_ascii_uppercase(), "non-canonical Windows key");
            (key, decode_native_string(&entry[1]))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(entry_count, environment.len(), "duplicate Windows aliases");

    let http_proxy = environment.get("HTTP_PROXY").expect("managed HTTP proxy");
    assert!(http_proxy.starts_with("http://"));
    for key in HTTP_PROXY_KEYS {
        assert_eq!(environment.get(*key), Some(http_proxy), "{key}");
    }
    for key in NO_PROXY_KEYS {
        assert_eq!(environment.get(*key).map(String::as_str), Some(""), "{key}");
    }
    assert_eq!(
        environment
            .get("CODEX_NETWORK_PROXY_ACTIVE")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        environment
            .get("CODEX_NETWORK_ALLOW_LOCAL_BINDING")
            .map(String::as_str),
        Some("0")
    );
    assert_eq!(
        environment
            .get("ELECTRON_GET_USE_PROXY")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        environment.get("NODE_USE_ENV_PROXY").map(String::as_str),
        Some("1")
    );
    let socks_proxy = environment.get("ALL_PROXY").expect("managed SOCKS proxy");
    assert!(socks_proxy.starts_with("socks5h://"));
    assert_eq!(environment.get("FTP_PROXY"), Some(socks_proxy));
    assert_eq!(
        environment.get("SSL_CERT_FILE").map(String::as_str),
        Some(r"C:\trusted\caller-ca.pem")
    );
    for key in [
        "CODEX_CA_CERTIFICATE",
        "CODEX_WINDOWS_SANDBOX_PROXY_PORTS",
        "CODEX_HOME",
        "MCP_CONSOLE_SANDBOX_CONTROL",
        "CARGO_BIN_EXE_PRIVATE_HELPER",
    ] {
        assert!(!environment.contains_key(key), "private key leaked: {key}");
    }

    let http_proxy_address = http_proxy
        .strip_prefix("http://")
        .expect("HTTP proxy URL")
        .parse::<SocketAddr>()
        .expect("HTTP proxy socket address");
    let socks_proxy_address = socks_proxy
        .strip_prefix("socks5h://")
        .expect("SOCKS proxy URL")
        .parse::<SocketAddr>()
        .expect("SOCKS proxy socket address");
    for (label, address) in [("HTTP", http_proxy_address), ("SOCKS", socks_proxy_address)] {
        assert!(
            TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_err(),
            "managed {label} proxy listener remained reachable after the final outcome"
        );
    }
}

fn exercise_managed_limited_access(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let post_origin = TcpListener::bind("127.0.0.1:0").expect("limited POST origin");
    post_origin
        .set_nonblocking(true)
        .expect("make limited POST origin nonblocking");
    let post_port = post_origin
        .local_addr()
        .expect("limited POST origin address")
        .port()
        .to_string();
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("http-request"),
        OsString::from("POST"),
        OsString::from("localhost"),
        OsString::from(post_port),
    ];
    let outcome = run_null_target(
        executable,
        state,
        &target,
        workspace,
        default_filesystem_rules(workspace),
        managed_network(json!(["localhost"]), json!([])),
    );

    assert_successful_outcome(&outcome, 73);
    assert_eq!(
        post_origin
            .accept()
            .expect_err("limited POST must not reach the allowlisted origin")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
}

fn exercise_managed_redirect_policy(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let redirect_origin = TcpListener::bind("127.0.0.1:0").expect("allowed redirect origin");
    let final_origin = TcpListener::bind("127.0.0.1:0").expect("allowed final origin");
    let redirect_address = redirect_origin
        .local_addr()
        .expect("allowed redirect address");
    let final_address = final_origin.local_addr().expect("allowed final address");
    let location = format!("http://localhost:{}/final", final_address.port());
    let redirect_server = spawn_http_server(
        redirect_origin,
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes(),
    );
    let final_server = spawn_http_server(
        final_origin,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK".to_vec(),
    );
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("http-follow"),
        OsString::from("localhost"),
        OsString::from(redirect_address.port().to_string()),
    ];
    let outcome = run_null_target(
        executable,
        state,
        &target,
        workspace,
        default_filesystem_rules(workspace),
        managed_network(json!(["localhost"]), json!([])),
    );
    if target_exit_code(&outcome) != Some(0) {
        let _ = TcpStream::connect(redirect_address);
        let _ = TcpStream::connect(final_address);
    }
    assert!(
        redirect_server
            .join()
            .expect("allowed redirect server")
            .starts_with(b"GET ")
    );
    assert!(
        final_server
            .join()
            .expect("allowed final server")
            .starts_with(b"GET ")
    );
    assert_successful_outcome(&outcome, 0);

    let redirect_origin = TcpListener::bind("127.0.0.1:0").expect("denied redirect origin");
    let final_origin = TcpListener::bind("127.0.0.1:0").expect("denied final origin");
    final_origin
        .set_nonblocking(true)
        .expect("make denied final origin nonblocking");
    let redirect_address = redirect_origin
        .local_addr()
        .expect("denied redirect address");
    let final_port = final_origin
        .local_addr()
        .expect("denied final address")
        .port();
    let location = format!("http://127.0.0.1:{final_port}/final");
    let redirect_server = spawn_http_server(
        redirect_origin,
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes(),
    );
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("http-follow"),
        OsString::from("localhost"),
        OsString::from(redirect_address.port().to_string()),
    ];
    let outcome = run_null_target(
        executable,
        state,
        &target,
        workspace,
        default_filesystem_rules(workspace),
        managed_network(json!(["localhost", "127.0.0.1"]), json!(["127.0.0.1"])),
    );
    let _ = TcpStream::connect(redirect_address);
    assert!(
        redirect_server
            .join()
            .expect("denied redirect server")
            .starts_with(b"GET "),
        "the allowed redirect origin was not reached"
    );
    assert_eq!(
        final_origin
            .accept()
            .expect_err("denied redirect destination must not be reached")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_successful_outcome(&outcome, 73);
}

fn exercise_managed_socks(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let origin = TcpListener::bind("127.0.0.1:0").expect("SOCKS origin");
    let origin_address = origin.local_addr().expect("SOCKS origin address");
    let accept = std::thread::spawn(move || origin.accept().expect("accept SOCKS connection"));
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("socks-connect"),
        OsString::from("localhost"),
        OsString::from(origin_address.port().to_string()),
    ];
    let mut network = managed_network(json!(["localhost"]), json!([]));
    network["access"] = json!("full");
    network["socks"] = json!(true);
    let outcome = run_null_target(
        executable,
        state,
        &target,
        workspace,
        default_filesystem_rules(workspace),
        network,
    );
    if target_exit_code(&outcome) != Some(0) {
        let _ = TcpStream::connect(origin_address);
    }
    accept.join().expect("SOCKS origin thread");
    assert_successful_outcome(&outcome, 0);
}

fn exercise_managed_upstream_proxy(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let upstream = TcpListener::bind("127.0.0.1:0").expect("upstream proxy listener");
    let upstream_address = upstream.local_addr().expect("upstream proxy address");
    let upstream_url = format!("http://{upstream_address}");
    let server = spawn_http_server(
        upstream,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK".to_vec(),
    );
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("http-get"),
        OsString::from("example.com"),
        OsString::from("80"),
    ];
    let mut runner = Runner::spawn_with_environment(
        executable,
        state,
        &target,
        &[(OsString::from("HTTP_PROXY"), OsString::from(upstream_url))],
        &[],
    );
    let rules = default_filesystem_rules(workspace);
    let mut network = managed_network(json!(["example.com"]), json!([]));
    network["upstream_proxy"] = json!(true);
    prepare_policy(&mut runner, workspace, rules.clone(), network.clone());
    assert_eq!(
        runner.request(launch_request_with_policy(
            2,
            workspace,
            rules,
            network,
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    let outcome = runner.request(wait_request(3));
    if target_exit_code(&outcome) != Some(0) {
        let _ = TcpStream::connect(upstream_address);
    }
    assert!(
        server
            .join()
            .expect("upstream proxy server")
            .starts_with(b"GET ")
    );
    assert_successful_outcome(&outcome, 0);
}

fn exercise_managed_loopback_policy(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("bind"),
        OsString::from("127.0.0.1"),
        OsString::from("0"),
    ];
    let mut network = managed_network(json!([]), json!([]));
    network["local_binding"] = json!(true);
    network["loopback"] = json!("allow");
    let outcome = run_null_target(
        executable,
        state,
        &target,
        workspace,
        default_filesystem_rules(workspace),
        network,
    );
    assert_successful_outcome(&outcome, 0);
}

fn exercise_unsupported_managed_network_policy(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let marker = workspace.join("unsupported-network-target-started");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        marker.as_os_str().to_owned(),
        OsString::from("started"),
    ];
    let rules = json!([{
        "path": workspace,
        "access": "write",
        "missing": "error",
    }]);
    let mut runner = Runner::spawn(executable, state, &target);
    let mut local_binding_without_loopback = managed_network(json!([]), json!([]));
    local_binding_without_loopback["local_binding"] = json!(true);
    assert_unsupported_network_policy(
        &mut runner,
        1,
        workspace,
        rules.clone(),
        local_binding_without_loopback,
    );

    let mut loopback_without_local_binding = managed_network(json!([]), json!([]));
    loopback_without_local_binding["loopback"] = json!("allow");
    assert_unsupported_network_policy(
        &mut runner,
        2,
        workspace,
        rules.clone(),
        loopback_without_local_binding,
    );

    let mut socks_udp = managed_network(json!([]), json!([]));
    socks_udp["socks"] = json!(true);
    socks_udp["socks_udp"] = json!(true);
    assert_unsupported_network_policy(&mut runner, 3, workspace, rules, socks_udp);
    assert!(!marker.exists());
}

fn assert_unsupported_network_policy(
    runner: &mut Runner,
    id: u64,
    workspace: &Path,
    filesystem_rules: Value,
    network: Value,
) {
    let response = runner.request(launch_request_with_policy(
        id,
        workspace,
        filesystem_rules,
        network,
        null_streams(),
    ));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["phase"], "validation");
    assert_eq!(response["error"]["target_started"], false);
}

fn spawn_http_server(listener: TcpListener, response: Vec<u8>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept HTTP request");
        let mut request = [0_u8; 4096];
        let length = stream.read(&mut request).expect("read HTTP request");
        let _ = stream.write_all(&response);
        request[..length].to_vec()
    })
}

fn exercise_launch_revalidates_ignored_setup_paths(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let denied = workspace.join("appears-after-setup-status");
    assert!(!denied.exists());
    let rules = json!([
        {
            "path": workspace,
            "access": "read",
            "missing": "error",
        },
        {
            "path": denied,
            "access": "deny",
            "missing": "ignore",
        },
    ]);
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("reopen"),
        denied.join("secret").as_os_str().to_owned(),
    ];
    let mut runner = Runner::spawn(executable, state, &target);
    let status = runner.request(json!({
        "type": "setup_status",
        "id": 1,
        "protocol_version": PROTOCOL_VERSION,
        "setup": setup_request_with_policy(
            workspace,
            rules.clone(),
            json!({ "mode": "denied" }),
        ),
    }));
    assert_eq!(status["type"], "setup_status");
    assert_eq!(status["setup"]["state"], "ready");

    std::fs::create_dir(&denied).expect("create late deny directory");
    std::fs::write(denied.join("secret"), b"secret").expect("create late deny fixture");
    let launch = runner.request(launch_request_with_policy(
        2,
        workspace,
        rules,
        json!({ "mode": "denied" }),
        null_streams(),
    ));
    assert_eq!(launch["type"], "launch_accepted");
    assert_successful_outcome(&runner.request(wait_request(3)), 73);
}

fn exercise_filesystem_contracts(
    executable: &RunnerExecutable,
    policy_root: &Path,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let protected = policy_root.join("protected");
    std::fs::create_dir(&protected).expect("protected policy directory");
    let secret = protected.join("secret");
    std::fs::write(&secret, b"secret").expect("protected read fixture");
    let rules = json!([
        { "path": policy_root, "access": "write", "missing": "error" },
        { "path": protected, "access": "deny", "missing": "error" },
    ]);

    let allowed = workspace.join("allowed-write");
    let allowed_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        allowed.as_os_str().to_owned(),
        OsString::from("contents"),
    ];
    let allowed_outcome = run_null_target(
        executable,
        state,
        &allowed_target,
        workspace,
        rules.clone(),
        json!({ "mode": "denied" }),
    );
    assert_successful_outcome(&allowed_outcome, 0);
    assert_eq!(
        std::fs::read(&allowed).expect("allowed target write"),
        native_bytes(OsStr::new("contents"))
    );

    let denied_write = protected.join("blocked-write");
    let denied_write_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        denied_write.as_os_str().to_owned(),
        OsString::from("contents"),
    ];
    let denied_write_outcome = run_null_target(
        executable,
        state,
        &denied_write_target,
        workspace,
        rules.clone(),
        json!({ "mode": "denied" }),
    );
    assert_successful_outcome(&denied_write_outcome, 73);
    assert!(!denied_write.exists());

    let denied_read_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("reopen"),
        secret.as_os_str().to_owned(),
    ];
    let denied_read_outcome = run_null_target(
        executable,
        state,
        &denied_read_target,
        workspace,
        rules.clone(),
        json!({ "mode": "denied" }),
    );
    assert_successful_outcome(&denied_read_outcome, 73);

    let state_write = state.join("target-write");
    let state_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        state_write.as_os_str().to_owned(),
        OsString::from("contents"),
    ];
    let state_outcome = run_null_target(
        executable,
        state,
        &state_target,
        workspace,
        rules,
        json!({ "mode": "denied" }),
    );
    assert_successful_outcome(&state_outcome, 73);
    assert!(!state_write.exists());

    let host_read_only_write = workspace.join("host-read-only-write");
    let target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("write"),
        host_read_only_write.as_os_str().to_owned(),
        OsString::from("contents"),
    ];
    let mut runner = Runner::spawn(executable, state, &target);
    let mut setup = setup_operation_with_policy(
        1,
        "prepare",
        workspace,
        json!([]),
        json!({ "mode": "denied" }),
    );
    setup["setup"]["filesystem"]["base"] = json!("host_read_only");
    let response = runner.request(setup);
    assert_eq!(response["type"], "setup_completed");
    let mut launch = launch_request_with_policy(
        2,
        workspace,
        json!([]),
        json!({ "mode": "denied" }),
        null_streams(),
    );
    launch["launch"]["filesystem"]["base"] = json!("host_read_only");
    assert_eq!(runner.request(launch)["type"], "launch_accepted");
    assert_successful_outcome(&runner.request(wait_request(3)), 73);
    assert!(!host_read_only_write.exists());
}

fn exercise_lifecycle_contracts(
    executable: &RunnerExecutable,
    state: &Path,
    workspace: &Path,
    fixture: &Path,
) {
    let exit_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("exit"),
        OsString::from("0"),
    ];
    let mut runner = Runner::spawn(executable, state, &exit_target);
    let mut graceful_launch = launch_request(1, workspace, null_streams());
    graceful_launch["launch"]["lifecycle"]["terminate_grace_ms"] = json!(1);
    let response = runner.request(graceful_launch);
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);
    drop(runner);

    let breakaway_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("attempt-windows-job-breakaway"),
    ];
    let breakaway_outcome = run_null_target(
        executable,
        state,
        &breakaway_target,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    assert_successful_outcome(&breakaway_outcome, 0);

    let mut runner = Runner::spawn(executable, state, &exit_target);
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    let mut isolated_terminal = launch_request(2, workspace, null_streams());
    isolated_terminal["launch"]["terminal"] = json!("isolate_host_devices");
    let response = runner.request(isolated_terminal);
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unsupported_policy");
    assert_eq!(response["error"]["target_started"], false);
    drop(runner);

    let mut runner = Runner::spawn(executable, state, &exit_target);
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    let inherited = json!({
        "stdin": { "mode": "inherited" },
        "stdout": { "mode": "inherited" },
        "stderr": { "mode": "inherited" },
    });
    assert_eq!(
        runner.request(launch_request(2, workspace, inherited))["type"],
        "launch_accepted"
    );
    assert_successful_outcome(&runner.request(wait_request(3)), 0);
    drop(runner);

    let descendant_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("spawn-descendant"),
        OsString::from("10000"),
    ];
    let stdout = OutputPipe::new();
    let descendant_streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let mut runner = Runner::spawn_with_handles(
        executable,
        state,
        &descendant_target,
        &[stdout.writer_value()],
    );
    let mut descendant_stdout = stdout.into_reader();
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    assert_eq!(
        runner.request(launch_request(2, workspace, descendant_streams))["type"],
        "launch_accepted"
    );
    let descendant_outcome = runner.request(wait_request(3));
    assert_successful_outcome(&descendant_outcome, 0);
    assert_eq!(descendant_outcome["outcome"]["retirement"]["forced"], true);
    let mut descendant_bytes = Vec::new();
    descendant_stdout
        .read_to_end(&mut descendant_bytes)
        .expect("read descendant-retirement stdout to EOF");
    assert!(descendant_bytes.is_empty());
    drop(runner);

    let sleeping_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("sleep"),
        OsString::from("10000"),
    ];
    let mut runner = Runner::spawn(executable, state, &sleeping_target);
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    assert_eq!(
        runner.request(launch_request(2, workspace, null_streams()))["type"],
        "launch_accepted"
    );
    let interrupt = runner.request(json!({
        "type": "interrupt",
        "id": 3,
        "protocol_version": PROTOCOL_VERSION,
    }));
    assert_eq!(interrupt["type"], "error");
    assert_eq!(interrupt["error"]["code"], "unsupported_policy");
    assert_eq!(interrupt["error"]["target_started"], true);
    let graceful = runner.request(json!({
        "type": "terminate",
        "id": 4,
        "protocol_version": PROTOCOL_VERSION,
        "deadlines": { "graceful_ms": 1, "force_ms": 5000 },
    }));
    assert_eq!(graceful["type"], "error");
    assert_eq!(graceful["error"]["code"], "unsupported_policy");
    assert_eq!(graceful["error"]["target_started"], true);
    assert_eq!(
        runner.request(json!({
            "type": "terminate",
            "id": 5,
            "protocol_version": PROTOCOL_VERSION,
            "deadlines": { "graceful_ms": 0, "force_ms": 5000 },
        }))["type"],
        "acknowledged"
    );
    let forced = runner.request(wait_request(6));
    assert_eq!(forced["type"], "final");
    assert_eq!(forced["outcome"]["target"]["kind"], "exited");
    assert_eq!(forced["outcome"]["retirement"]["complete"], true);
    assert_eq!(forced["outcome"]["retirement"]["forced"], true);
    assert_eq!(forced["outcome"]["infrastructure"]["error"], Value::Null);
    assert_eq!(
        forced["outcome"]["infrastructure"]["cleanup_error"],
        Value::Null
    );
    drop(runner);

    let control_loss_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("spawn-descendant-and-sleep"),
        OsString::from("10000"),
        OsString::from("10000"),
    ];
    let stdout = OutputPipe::new();
    let control_loss_streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let mut runner = Runner::spawn_with_handles(
        executable,
        state,
        &control_loss_target,
        &[stdout.writer_value()],
    );
    let mut target_stdout = stdout.into_reader();
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    let mut launch = launch_request(2, workspace, control_loss_streams);
    launch["launch"]["lifecycle"]["root_exit_grace_ms"] = json!(10000);
    launch["launch"]["lifecycle"]["force_timeout_ms"] = json!(5000);
    let launched = runner.request(launch);
    assert_eq!(launched["type"], "launch_accepted");
    let root_process_id = u32::try_from(
        launched["root_process_id"]
            .as_u64()
            .expect("Windows launch root process ID"),
    )
    .expect("Windows launch root process ID fits u32");
    let mut descendant_process_id = [0_u8; 4];
    target_stdout
        .read_exact(&mut descendant_process_id)
        .expect("read control-loss descendant process ID");
    let descendant_process_id = u32::from_be_bytes(descendant_process_id);
    let root_process = open_process_for_wait(root_process_id).expect("open target root for wait");
    let descendant_process =
        open_process_for_wait(descendant_process_id).expect("open target descendant for wait");
    runner.close_control();
    assert!(
        runner.wait_for_exit(Duration::from_secs(15)).success(),
        "runner did not report successful fail-safe tree retirement"
    );
    wait_for_process_exit(&root_process, Duration::from_secs(5))
        .expect("target root retired after control loss");
    wait_for_process_exit(&descendant_process, Duration::from_secs(5))
        .expect("target descendant retired after control loss");

    let abrupt_target = vec![
        fixture.as_os_str().to_owned(),
        OsString::from("spawn-descendant-and-sleep"),
        OsString::from("10000"),
        OsString::from("10000"),
    ];
    let stdout = OutputPipe::new();
    let abrupt_streams = json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "passed_handle", "handle": stdout.writer_value() },
        "stderr": { "mode": "null" },
    });
    let mut runner =
        Runner::spawn_with_handles(executable, state, &abrupt_target, &[stdout.writer_value()]);
    let mut target_stdout = stdout.into_reader();
    prepare_policy(
        &mut runner,
        workspace,
        default_filesystem_rules(workspace),
        json!({ "mode": "denied" }),
    );
    let launched = runner.request(launch_request(2, workspace, abrupt_streams));
    assert_eq!(launched["type"], "launch_accepted");
    let root_process_id = u32::try_from(
        launched["root_process_id"]
            .as_u64()
            .expect("Windows launch root process ID"),
    )
    .expect("Windows launch root process ID fits u32");
    let mut descendant_process_id = [0_u8; 4];
    target_stdout
        .read_exact(&mut descendant_process_id)
        .expect("read descendant process ID");
    let descendant_process_id = u32::from_be_bytes(descendant_process_id);
    let root_process = open_process_for_wait(root_process_id).expect("open target root for wait");
    let descendant_process =
        open_process_for_wait(descendant_process_id).expect("open target descendant for wait");

    assert!(!runner.kill().success());
    wait_for_process_exit(&root_process, Duration::from_secs(15))
        .expect("target root retired after abrupt runner termination");
    wait_for_process_exit(&descendant_process, Duration::from_secs(15))
        .expect("target descendant retired after abrupt runner termination");
}

fn run_null_target(
    executable: &RunnerExecutable,
    state: &Path,
    target: &[OsString],
    working_directory: &Path,
    filesystem_rules: Value,
    network: Value,
) -> Value {
    let mut runner = Runner::spawn(executable, state, target);
    prepare_policy(
        &mut runner,
        working_directory,
        filesystem_rules.clone(),
        network.clone(),
    );
    assert_eq!(
        runner.request(launch_request_with_policy(
            2,
            working_directory,
            filesystem_rules,
            network,
            null_streams(),
        ))["type"],
        "launch_accepted"
    );
    runner.request(wait_request(3))
}

fn prepare_policy(
    runner: &mut Runner,
    working_directory: &Path,
    filesystem_rules: Value,
    network: Value,
) {
    let response = runner.request(setup_operation_with_policy(
        1,
        "prepare",
        working_directory,
        filesystem_rules,
        network,
    ));
    assert_eq!(response["type"], "setup_completed");
    assert!(matches!(
        response["operation"].as_str(),
        Some("prepared" | "already_ready")
    ));
}

fn target_exit_code(outcome: &Value) -> Option<i64> {
    outcome["outcome"]["target"]["code"].as_i64()
}

fn setup_request(working_directory: &Path) -> Value {
    setup_request_with_policy(
        working_directory,
        default_filesystem_rules(working_directory),
        json!({ "mode": "denied" }),
    )
}

fn setup_request_with_policy(
    working_directory: &Path,
    filesystem_rules: Value,
    network: Value,
) -> Value {
    json!({
        "working_directory": working_directory,
        "policy_base_directory": working_directory,
        "filesystem": {
            "base": "host_read_only",
            "rules": filesystem_rules,
        },
        "network": network,
        "platform_extensions": {},
    })
}

fn setup_operation(id: u64, operation: &str, working_directory: &Path) -> Value {
    setup_operation_with_policy(
        id,
        operation,
        working_directory,
        default_filesystem_rules(working_directory),
        json!({ "mode": "denied" }),
    )
}

fn setup_operation_with_policy(
    id: u64,
    operation: &str,
    working_directory: &Path,
    filesystem_rules: Value,
    network: Value,
) -> Value {
    json!({
        "type": "setup",
        "id": id,
        "protocol_version": PROTOCOL_VERSION,
        "operation": operation,
        "setup": setup_request_with_policy(working_directory, filesystem_rules, network),
    })
}

fn launch_request(id: u64, working_directory: &Path, streams: Value) -> Value {
    launch_request_with_policy(
        id,
        working_directory,
        default_filesystem_rules(working_directory),
        json!({ "mode": "denied" }),
        streams,
    )
}

fn launch_request_with_policy(
    id: u64,
    working_directory: &Path,
    filesystem_rules: Value,
    network: Value,
    streams: Value,
) -> Value {
    json!({
        "type": "launch",
        "id": id,
        "protocol_version": PROTOCOL_VERSION,
        "launch": {
            "working_directory": working_directory,
            "policy_base_directory": working_directory,
            "filesystem": {
                "base": "host_read_only",
                "rules": filesystem_rules,
            },
            "network": network,
            "streams": streams,
            "terminal": "preserve",
            "lifecycle": {
                "kind": "command",
                "root_exit_grace_ms": 100,
                "terminate_grace_ms": 0,
                "force_timeout_ms": 10000,
            },
            "platform_extensions": {},
        },
    })
}

fn default_filesystem_rules(working_directory: &Path) -> Value {
    json!([
        {
            "path": working_directory,
            "access": "read",
            "missing": "error",
        },
    ])
}

fn managed_network(allowed_domains: Value, denied_domains: Value) -> Value {
    json!({
        "mode": "managed_proxy",
        "access": "limited",
        "allowed_domains": allowed_domains,
        "denied_domains": denied_domains,
        "socks": false,
        "socks_udp": false,
        "upstream_proxy": false,
        "local_binding": false,
        "loopback": "proxy_only",
        "local_ports": [],
        "unix_sockets": [],
    })
}

fn null_streams() -> Value {
    json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "null" },
        "stderr": { "mode": "null" },
    })
}

fn wait_request(id: u64) -> Value {
    json!({
        "type": "wait",
        "id": id,
        "protocol_version": PROTOCOL_VERSION,
        "retirement_timeout_ms": 15000,
    })
}

fn assert_successful_outcome(response: &Value, code: i64) {
    assert_eq!(response["type"], "final");
    assert_eq!(response["outcome"]["target"]["kind"], "exited");
    assert_eq!(response["outcome"]["target"]["code"], code);
    assert_eq!(response["outcome"]["retirement"]["complete"], true);
    assert_eq!(response["outcome"]["infrastructure"]["error"], Value::Null);
    assert_eq!(
        response["outcome"]["infrastructure"]["cleanup_error"],
        Value::Null
    );
}

fn git_revision() -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("query Git revision");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("UTF-8 Git revision")
        .trim()
        .to_string()
}

fn native_bytes(value: &OsStr) -> Vec<u8> {
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

fn decode_native_values(mut bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut values = Vec::new();
    while !bytes.is_empty() {
        let mut length = [0_u8; 4];
        bytes.read_exact(&mut length).expect("native value length");
        let mut value = vec![0; u32::from_be_bytes(length) as usize];
        bytes.read_exact(&mut value).expect("native value bytes");
        values.push(value);
    }
    values
}

fn decode_optional_native_values(mut bytes: &[u8]) -> Vec<Option<Vec<u8>>> {
    let mut values = Vec::new();
    while !bytes.is_empty() {
        let mut length = [0_u8; 4];
        bytes.read_exact(&mut length).expect("native value length");
        let length = u32::from_be_bytes(length);
        if length == u32::MAX {
            values.push(None);
            continue;
        }
        let length = length as usize;
        let (value, remaining) = bytes.split_at(length);
        values.push(Some(value.to_vec()));
        bytes = remaining;
    }
    values
}

fn decode_native_string(bytes: &[u8]) -> String {
    assert_eq!(bytes.len() % 2, 0, "native UTF-16 value has an odd length");
    let wide = bytes
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&wide).expect("native value is valid UTF-16")
}
