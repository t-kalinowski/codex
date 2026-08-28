#![allow(clippy::expect_used)]

use codex_utils_cargo_bin::cargo_bin;
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::process::Child;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use tempfile::TempDir;

#[cfg(unix)]
#[path = "support/runner_executable.rs"]
mod runner_executable;

#[cfg(unix)]
const PROTOCOL_VERSION: u64 = 1;
#[cfg(unix)]
const MAX_FRAME_SIZE: u32 = 1024 * 1024;

#[test]
fn focused_runner_binary_is_built_for_contract_tests() {
    let runner = cargo_bin("mcp-console-sandbox").expect("runner binary");
    assert!(runner.is_file());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::runner_executable::RunnerExecutable;
    use crate::runner_executable::apply_sanitized_environment;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::Output;

    const CHILD_CONTROL_FD: i32 = 198;
    const TARGET_STDIN_FD: i32 = 190;
    const TARGET_STDOUT_FD: i32 = 191;
    const TARGET_STDERR_FD: i32 = 192;

    struct Runner {
        child: Child,
        control: UnixStream,
        _state_dir: TempDir,
        _runner_executable: RunnerExecutable,
    }

    struct TargetIo {
        _stdin: UnixStream,
        stdout: UnixStream,
        stderr: UnixStream,
    }

    impl Runner {
        fn spawn(target: &[&str]) -> Self {
            let target = target.iter().map(OsString::from).collect::<Vec<_>>();
            Self::spawn_native(&target, &[], None).0
        }

        fn spawn_native(
            target: &[OsString],
            environment: &[(&str, &str)],
            target_io: Option<(UnixStream, UnixStream, UnixStream)>,
        ) -> (Self, Option<TargetIo>) {
            let (control, child_control) = UnixStream::pair().expect("control socket pair");
            let child_control_fd = child_control.as_raw_fd();
            let state_dir = TempDir::new().expect("state directory");
            let runner_executable = RunnerExecutable::packaged();
            let mut command = Command::new(runner_executable.path());
            command
                .arg("--state-dir")
                .arg(state_dir.path())
                .arg("--control-fd")
                .arg(CHILD_CONTROL_FD.to_string());
            apply_sanitized_environment(&mut command, environment);
            let (child_stream_fds, parent_io, child_streams) = match target_io {
                Some((stdin, stdout, stderr)) => {
                    let (parent_stdin, child_stdin) = UnixStream::pair().expect("stdin pair");
                    let (child_stdout, parent_stdout) = UnixStream::pair().expect("stdout pair");
                    let (child_stderr, parent_stderr) = UnixStream::pair().expect("stderr pair");
                    let child_stream_fds = Some((
                        child_stdin.as_raw_fd(),
                        child_stdout.as_raw_fd(),
                        child_stderr.as_raw_fd(),
                    ));
                    drop((stdin, stdout, stderr));
                    (
                        child_stream_fds,
                        Some(TargetIo {
                            _stdin: parent_stdin,
                            stdout: parent_stdout,
                            stderr: parent_stderr,
                        }),
                        Some((child_stdin, child_stdout, child_stderr)),
                    )
                }
                None => (None, None, None),
            };
            if child_stream_fds.is_some() {
                for descriptor in [TARGET_STDIN_FD, TARGET_STDOUT_FD, TARGET_STDERR_FD] {
                    command.arg("--stream-fd").arg(descriptor.to_string());
                }
            }
            if !target.is_empty() {
                command.arg("--").args(target);
            }
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some((stdin, stdout, stderr)) = child_stream_fds {
                        for (source, destination) in [
                            (stdin, TARGET_STDIN_FD),
                            (stdout, TARGET_STDOUT_FD),
                            (stderr, TARGET_STDERR_FD),
                        ] {
                            if libc::dup2(source, destination) == -1 {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                    }
                    Ok(())
                });
            }
            let child = command.spawn().expect("spawn runner");
            drop(child_control);
            drop(child_streams);
            (
                Self {
                    child,
                    control,
                    _state_dir: state_dir,
                    _runner_executable: runner_executable,
                },
                parent_io,
            )
        }

        fn request(&mut self, request: Value) -> Value {
            write_json_frame(&mut self.control, request);
            read_json_frame(&mut self.control)
        }

        fn write_bytes(&mut self, bytes: &[u8]) {
            self.control.write_all(bytes).expect("write control bytes");
        }
    }

    impl Drop for Runner {
        fn drop(&mut self) {
            drop(self.control.shutdown(std::net::Shutdown::Both));
            if self
                .child
                .try_wait()
                .expect("query runner status")
                .is_none()
            {
                self.child.kill().expect("kill runner");
            }
            self.child.wait().expect("wait for runner");
        }
    }

    fn run_with_closed_control(
        runner_executable: &std::path::Path,
        state_directory: &std::path::Path,
    ) -> Output {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let mut command = Command::new(runner_executable);
        command
            .arg("--state-dir")
            .arg(state_directory)
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string());
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop((control, child_control));
        child.wait_with_output().expect("wait for runner")
    }

    #[test]
    fn non_unicode_infrastructure_paths_are_rejected_before_state_creation() {
        let parent = TempDir::new().expect("state parent directory");
        let state_directory = parent
            .path()
            .join(OsString::from_vec(vec![b's', b't', b'a', b't', b'e', 0xff]));
        let runner_executable = RunnerExecutable::packaged();
        let state_output = run_with_closed_control(runner_executable.path(), &state_directory);

        assert!(!state_output.status.success());
        assert!(!state_directory.exists());

        #[cfg(target_os = "linux")]
        {
            let executable_parent = TempDir::new().expect("runner parent directory");
            let non_unicode_executable = executable_parent.path().join(OsString::from_vec(vec![
                b'm', b'c', b'p', b'-', 0xff, b'-', b's', b'a', b'n', b'd', b'b', b'o', b'x',
            ]));
            std::fs::copy(runner_executable.path(), &non_unicode_executable)
                .expect("copy runner to non-Unicode path");
            let state_directory = TempDir::new().expect("state directory");
            let executable_output =
                run_with_closed_control(&non_unicode_executable, state_directory.path());

            assert!(!executable_output.status.success());
        }
    }

    #[test]
    fn discovery_reports_version_revision_backend_and_contract_limits() {
        let mut runner = Runner::spawn(&[]);
        let response = runner.request(json!({
            "type": "discover",
            "id": 7,
            "protocol_version": PROTOCOL_VERSION,
        }));

        assert_eq!(response["type"], "capabilities");
        assert_eq!(response["id"], 7);
        assert_eq!(
            response["capabilities"]["protocol_version"],
            PROTOCOL_VERSION
        );
        assert_eq!(response["capabilities"]["runner_version"], "0.150.1");
        assert_eq!(
            response["capabilities"]["maximum_frame_size"],
            MAX_FRAME_SIZE
        );
        let revision = response["capabilities"]["codex_source_revision"]
            .as_str()
            .expect("source revision string");
        assert_eq!(revision.len(), 40);
        assert!(revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if let Ok(expected_revision) = Command::new("git").args(["rev-parse", "HEAD"]).output()
            && expected_revision.status.success()
        {
            assert_eq!(
                revision,
                String::from_utf8(expected_revision.stdout)
                    .expect("UTF-8 Git revision")
                    .trim()
            );
        }
        assert_eq!(
            response["capabilities"]["codex_release_tag"],
            "rust-v0.150.1"
        );
        assert_ne!(response["capabilities"]["backend"], "unsupported");
        assert_eq!(
            response["capabilities"]["streams"]["application_bytes_on_control_channel"],
            false
        );
        assert_eq!(response["capabilities"]["network"]["full_access"], true);
        assert_eq!(response["capabilities"]["network"]["limited_access"], true);
        assert_eq!(
            response["capabilities"]["network"]["unix_socket_policy"],
            cfg!(target_os = "macos")
        );
        assert_eq!(
            response["capabilities"]["network"]["unix_socket_allow_rules"],
            cfg!(target_os = "macos")
        );
        assert_eq!(
            response["capabilities"]["network"]["unix_socket_deny_rules"],
            false
        );
        assert_eq!(
            response["capabilities"]["terminal"]["controlling_terminal_reopen"],
            cfg!(target_os = "macos")
        );
        assert_eq!(
            response["capabilities"]["lifecycle"]["process_tree_supervision"],
            cfg!(target_os = "linux")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn packaged_bwrap_reports_the_workspace_release_token() {
        let output = Command::new(codex_utils_cargo_bin::cargo_bin("bwrap").expect("bwrap binary"))
            .arg("--codex-mcp-console-sandbox-bwrap-compatibility-v2")
            .output()
            .expect("query packaged bwrap compatibility");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            output.stdout,
            b"mcp-console-sandbox-bwrap/2 codex/0.150.1\n"
        );
        assert_eq!(output.stderr, b"");
    }

    #[test]
    fn absolute_native_arguments_are_preserved_without_a_shell() {
        let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        let native_argument = OsString::from_vec(vec![b'n', b'o', b'n', 0xff, b'u', b't', b'f']);
        let target = vec![
            fixture.into_os_string(),
            OsString::from("argv"),
            OsString::from("-leading"),
            OsString::from("with spaces"),
            OsString::from("key=value"),
            OsString::new(),
            OsString::from("$(printf shell-was-used)"),
            native_argument.clone(),
        ];
        let dummy = UnixStream::pair().expect("dummy pair");
        let (mut runner, io) = Runner::spawn_native(
            &target,
            &[],
            Some((
                dummy.0.try_clone().expect("clone dummy"),
                dummy.0.try_clone().expect("clone dummy"),
                dummy.1,
            )),
        );
        let mut io = io.expect("target streams");
        let response = runner.request(launch_request(
            31,
            stream_spec_passed(),
            json!({
                "mode": "denied"
            }),
        ));
        assert_eq!(response["type"], "launch_accepted");
        let outcome = runner.request(json!({
            "type": "wait",
            "id": 32,
            "protocol_version": PROTOCOL_VERSION,
            "retirement_timeout_ms": 5000,
        }));
        assert_eq!(outcome["type"], "final");
        assert_eq!(outcome["outcome"]["target"]["kind"], "exited");
        assert_eq!(outcome["outcome"]["target"]["code"], 0);
        let mut output = Vec::new();
        io.stdout
            .read_to_end(&mut output)
            .expect("read target stdout");
        assert_eq!(
            decode_values(&output),
            vec![
                b"-leading".to_vec(),
                b"with spaces".to_vec(),
                b"key=value".to_vec(),
                Vec::new(),
                b"$(printf shell-was-used)".to_vec(),
                native_argument.into_vec(),
            ]
        );
        let mut stderr = Vec::new();
        io.stderr
            .read_to_end(&mut stderr)
            .expect("read target stderr");
        assert_eq!(stderr, Vec::<u8>::new());
    }

    #[test]
    fn target_environment_is_complete_except_runner_private_values() {
        let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        for (request_id, network) in [
            (41, json!({ "mode": "denied" })),
            (43, json!({ "mode": "unrestricted" })),
        ] {
            let target = vec![
                fixture.clone().into_os_string(),
                OsString::from("environment"),
                OsString::from("MCP_CONSOLE_CONTRACT_VALUE"),
                OsString::from("MCP_CONSOLE_SANDBOX_RESOURCE_DIR"),
                OsString::from("CODEX_HOME"),
                OsString::from("CARGO_BIN_EXE_bwrap"),
                OsString::from("CODEX_NETWORK_PROXY_ACTIVE"),
                OsString::from("CODEX_NETWORK_ALLOW_LOCAL_BINDING"),
                OsString::from("CODEX_WINDOWS_SANDBOX_PROXY_PORTS"),
                OsString::from("CODEX_CA_CERTIFICATE"),
                OsString::from("CODEX_NETWORK_POLICY_VIOLATION"),
            ];
            let dummy = UnixStream::pair().expect("dummy pair");
            let (mut runner, io) = Runner::spawn_native(
                &target,
                &[
                    ("MCP_CONSOLE_CONTRACT_VALUE", "preserved"),
                    ("MCP_CONSOLE_SANDBOX_RESOURCE_DIR", "/private/helper"),
                    ("CODEX_HOME", "/private/codex-home"),
                    ("CARGO_BIN_EXE_bwrap", "/private/test-helper"),
                    ("CODEX_NETWORK_PROXY_ACTIVE", "1"),
                    ("CODEX_NETWORK_ALLOW_LOCAL_BINDING", "1"),
                    ("CODEX_WINDOWS_SANDBOX_PROXY_PORTS", "1234"),
                    ("CODEX_CA_CERTIFICATE", "/private/managed-ca.pem"),
                    ("CODEX_NETWORK_POLICY_VIOLATION", "1"),
                ],
                Some((
                    dummy.0.try_clone().expect("clone dummy"),
                    dummy.0.try_clone().expect("clone dummy"),
                    dummy.1,
                )),
            );
            let mut io = io.expect("target streams");
            assert_eq!(
                runner.request(launch_request(request_id, stream_spec_passed(), network,))["type"],
                "launch_accepted"
            );
            let outcome = runner.request(wait_request(request_id + 1));
            assert_eq!(outcome["outcome"]["target"]["code"], 0);
            let mut output = Vec::new();
            io.stdout
                .read_to_end(&mut output)
                .expect("read target stdout");
            assert_eq!(
                decode_optional_values(&output),
                vec![
                    Some(b"preserved".to_vec()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None
                ]
            );
        }
    }

    #[test]
    fn observable_loader_variables_are_rejected_before_target_launch() {
        let loader_key = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        let target = vec![
            fixture.into_os_string(),
            OsString::from("exit"),
            OsString::from("0"),
        ];
        let (mut runner, _) =
            Runner::spawn_native(&target, &[(loader_key, "/untrusted/loader-path")], None);
        let response = runner.request(launch_request(
            43,
            stream_spec_null(),
            json!({ "mode": "denied" }),
        ));
        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "invalid_request");
        assert_eq!(response["error"]["target_started"], false);
        assert!(
            !response["error"]["message"]
                .as_str()
                .expect("error message")
                .contains("/untrusted/loader-path")
        );
    }

    #[test]
    fn version_mismatch_is_structured_and_does_not_start_the_target() {
        let mut runner = Runner::spawn(&["/definitely/not/a/target"]);
        let response = runner.request(json!({
            "type": "discover",
            "id": 4,
            "protocol_version": 999,
        }));

        assert_eq!(response["type"], "error");
        assert_eq!(response["id"], 4);
        assert_eq!(response["error"]["code"], "version_mismatch");
        assert_eq!(response["error"]["target_started"], false);
    }

    #[test]
    fn unknown_required_field_is_rejected() {
        let mut runner = Runner::spawn(&[]);
        let response = runner.request(json!({
            "type": "discover",
            "id": 9,
            "protocol_version": PROTOCOL_VERSION,
            "authority_widening": true,
        }));

        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "malformed_json");
        assert_eq!(response["error"]["target_started"], false);
    }

    #[test]
    fn setup_status_requires_the_policy_it_inspects() {
        let mut missing_runner = Runner::spawn(&[]);
        let missing_policy = missing_runner.request(json!({
            "type": "setup_status",
            "id": 10,
            "protocol_version": PROTOCOL_VERSION,
        }));
        assert_eq!(missing_policy["type"], "error");
        assert_eq!(missing_policy["error"]["code"], "malformed_json");

        let mut runner = Runner::spawn(&[]);
        let response = runner.request(json!({
            "type": "setup_status",
            "id": 11,
            "protocol_version": PROTOCOL_VERSION,
            "setup": setup_request(),
        }));
        assert_eq!(response["type"], "setup_status");
        assert_eq!(response["setup"]["state"], "not_required");
    }

    #[test]
    fn malformed_json_is_rejected() {
        let mut runner = Runner::spawn(&[]);
        let payload = b"{not-json";
        runner.write_bytes(&(payload.len() as u32).to_be_bytes());
        runner.write_bytes(payload);

        let response = read_json_frame(&mut runner.control);
        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "malformed_json");
    }

    #[test]
    fn truncated_length_and_payload_are_rejected() {
        let mut truncated_length = Runner::spawn(&[]);
        truncated_length.write_bytes(&[0, 0]);
        truncated_length
            .control
            .shutdown(std::net::Shutdown::Write)
            .expect("finish truncated length");
        let response = read_json_frame(&mut truncated_length.control);
        assert_eq!(response["type"], "error");
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["error"]["code"], "malformed_frame");

        let mut truncated_payload = Runner::spawn(&[]);
        truncated_payload.write_bytes(&8_u32.to_be_bytes());
        truncated_payload.write_bytes(b"{}");
        truncated_payload
            .control
            .shutdown(std::net::Shutdown::Write)
            .expect("finish truncated payload");
        let response = read_json_frame(&mut truncated_payload.control);
        assert_eq!(response["type"], "error");
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["error"]["code"], "malformed_frame");
    }

    #[test]
    fn oversized_frame_is_rejected_before_payload_read() {
        let mut runner = Runner::spawn(&[]);
        runner.write_bytes(&(MAX_FRAME_SIZE + 1).to_be_bytes());

        let response = read_json_frame(&mut runner.control);
        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "malformed_frame");
        assert!(
            response["error"]["message"]
                .as_str()
                .expect("error message")
                .contains("exceeds")
        );
    }

    #[test]
    fn launch_is_invalid_without_a_native_target() {
        let mut runner = Runner::spawn(&[]);
        let response = runner.request(minimal_launch_request(21));

        assert_eq!(response["type"], "error");
        assert_eq!(response["id"], 21);
        assert_eq!(response["error"]["code"], "invalid_request");
        assert_eq!(response["error"]["target_started"], false);
    }

    #[test]
    fn unsupported_platform_policy_is_classified_before_launch() {
        let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        let target = [fixture.to_str().expect("Unicode fixture path"), "exit", "0"];
        let mut runner = Runner::spawn(&target);
        let mut launch = minimal_launch_request(22);
        launch["launch"]["platform_extensions"] = if cfg!(target_os = "macos") {
            json!({ "linux": {} })
        } else {
            json!({ "macos": {} })
        };

        let response = runner.request(launch);

        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "unsupported_policy");
        assert_eq!(response["error"]["phase"], "validation");
        assert_eq!(response["error"]["target_started"], false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_rejects_graceful_termination_before_launch() {
        let fixture = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        let target = [fixture.to_str().expect("Unicode fixture path"), "exit", "0"];
        let mut runner = Runner::spawn(&target);
        let mut launch = minimal_launch_request(23);
        launch["launch"]["lifecycle"]["terminate_grace_ms"] = json!(10);

        let response = runner.request(launch);

        assert_eq!(response["type"], "error");
        assert_eq!(response["error"]["code"], "unsupported_policy");
        assert_eq!(response["error"]["phase"], "validation");
        assert_eq!(response["error"]["target_started"], false);
    }

    fn minimal_launch_request(id: u64) -> Value {
        launch_request(id, stream_spec_null(), json!({ "mode": "denied" }))
    }

    fn launch_request(id: u64, streams: Value, network: Value) -> Value {
        let cwd = std::env::current_dir().expect("current directory");
        json!({
            "type": "launch",
            "id": id,
            "protocol_version": PROTOCOL_VERSION,
            "launch": {
                "working_directory": cwd,
                "policy_base_directory": cwd,
                "filesystem": {
                    "base": "host_read_only",
                    "rules": [],
                },
                "network": network,
                "streams": streams,
                "terminal": "preserve",
                "lifecycle": {
                    "kind": "command",
                    "root_exit_grace_ms": 10,
                    "terminate_grace_ms": if cfg!(target_os = "linux") { 0 } else { 10 },
                    "force_timeout_ms": 1000,
                },
                "platform_extensions": {},
            },
        })
    }

    fn setup_request() -> Value {
        let cwd = std::env::current_dir().expect("current directory");
        json!({
            "working_directory": cwd,
            "policy_base_directory": cwd,
            "filesystem": {
                "base": "host_read_only",
                "rules": [],
            },
            "network": { "mode": "denied" },
            "platform_extensions": {},
        })
    }

    fn wait_request(id: u64) -> Value {
        json!({
            "type": "wait",
            "id": id,
            "protocol_version": PROTOCOL_VERSION,
            "retirement_timeout_ms": 5000,
        })
    }

    fn stream_spec_null() -> Value {
        json!({
            "stdin": { "mode": "null" },
            "stdout": { "mode": "null" },
            "stderr": { "mode": "null" },
        })
    }

    fn stream_spec_passed() -> Value {
        json!({
            "stdin": { "mode": "passed_handle", "handle": TARGET_STDIN_FD },
            "stdout": { "mode": "passed_handle", "handle": TARGET_STDOUT_FD },
            "stderr": { "mode": "passed_handle", "handle": TARGET_STDERR_FD },
        })
    }

    fn decode_values(mut bytes: &[u8]) -> Vec<Vec<u8>> {
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
}

#[cfg(unix)]
fn write_json_frame(stream: &mut impl Write, value: Value) {
    let payload = serde_json::to_vec(&value).expect("serialize request");
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .expect("write frame length");
    stream.write_all(&payload).expect("write frame payload");
}

#[cfg(unix)]
fn read_json_frame(stream: &mut impl Read) -> Value {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).expect("read frame length");
    let mut payload = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).expect("read frame payload");
    serde_json::from_slice(&payload).expect("parse response")
}
