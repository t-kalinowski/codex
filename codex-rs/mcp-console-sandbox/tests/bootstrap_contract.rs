#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used)]

#[path = "lifecycle/cancellation_tests.rs"]
mod cancellation;
#[path = "../src/codex.rs"]
mod codex;
#[path = "lifecycle/config_tests.rs"]
mod configuration;
#[path = "lifecycle/lifecycle_tests.rs"]
mod lifecycle;
#[path = "lifecycle/ownership_tests.rs"]
mod ownership;
#[cfg(target_os = "linux")]
#[path = "lifecycle/prerequisite_tests.rs"]
mod prerequisite;
#[path = "lifecycle/retirement_tests.rs"]
mod retirement;
#[path = "lifecycle/security_tests.rs"]
mod security;
#[path = "lifecycle/startup_tests.rs"]
mod startup;
#[path = "lifecycle/terminal_tests.rs"]
mod terminal;
#[path = "transport/transport_tests.rs"]
mod transport;

use crate::codex::cargo_bin;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;

fn request(command: &[&str]) -> Value {
    json!({
        "version": 2,
        "command": command,
        "cwd": std::env::current_dir().unwrap(),
        "environment": {},
        "filesystem": {"kind": "restricted", "entries": [
            {"path": {"type": "special", "value": {"kind": "root"}}, "access": "read"}
        ]},
        "network": "restricted",
        "proxy": null
    })
}

fn frame(request: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec(request).unwrap();
    let mut bytes = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    bytes.extend(payload);
    bytes
}

#[cfg(target_os = "linux")]
fn copy_executable(from: &Path, to: &Path) {
    // A concurrent fork can retain a writable copy descriptor until exec, even
    // with CLOEXEC, and make another launch fail with ETXTBSY. Keep those
    // descriptors in a separate copy process and wait for it to close them.
    let output = Command::new("cp")
        .arg("--")
        .arg(from)
        .arg(to)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn runner(directory: &Path) -> Command {
    let executable = cargo_bin("mcp-console-sandbox").unwrap();
    #[cfg(target_os = "linux")]
    let executable = {
        // Exercise the ordinary sibling helper layout under Cargo and Bazel.
        let staged = directory.join("runner");
        copy_executable(&executable, &staged);
        copy_executable(&cargo_bin("bwrap").unwrap(), &directory.join("bwrap"));
        staged
    };
    #[cfg(not(target_os = "linux"))]
    let _ = directory;
    let mut command = Command::new(executable);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

fn run(bytes: Vec<u8>, input: &[u8]) -> Output {
    let directory = tempfile::tempdir().unwrap();
    run_command(runner(directory.path()), bytes, input)
}

fn spawn(command: &mut Command) -> (std::process::Child, std::io::PipeWriter) {
    let (bootstrap, writer) = std::io::pipe().unwrap();
    let child = spawn_with_bootstrap(command, &bootstrap);
    drop(bootstrap);
    (child, writer)
}

fn spawn_with_bootstrap(command: &mut Command, bootstrap: &impl AsRawFd) -> std::process::Child {
    let raw = bootstrap.as_raw_fd();
    command.args(["--bootstrap-fd", &raw.to_string()]);
    // SAFETY: only the child changes its copy's flags, using async-signal-safe
    // fcntl. The writer and other Rust descriptors remain close-on-exec.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(raw, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().unwrap()
}

fn run_command(mut command: Command, bytes: Vec<u8>, input: &[u8]) -> Output {
    let (mut child, mut bootstrap) = spawn(command.stdin(Stdio::piped()));
    let mut stdin = child.stdin.take().unwrap();
    std::thread::scope(|scope| {
        let input_writer = scope.spawn(move || stdin.write_all(input));
        let bootstrap_writer = scope.spawn(move || bootstrap.write_all(&bytes));
        let output = child.wait_with_output().unwrap();
        // Rejected requests may close either pipe before its writer finishes.
        for writer in [input_writer, bootstrap_writer] {
            if let Err(error) = writer.join().unwrap() {
                assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            }
        }
        output
    })
}

#[test]
fn valid_bootstrap_launches_one_command() {
    let output = run(frame(&request(&["/bin/echo", "launched"])), &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"launched\n");
    assert_eq!(output.stderr, b"");
}

#[test]
fn binary_stdin_is_independent_of_bootstrap() {
    for size in [0, 1, 255, 4095, 4096, 8191, 8192, 65537, 1048576] {
        let sentinel: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let output = run(frame(&fixture("stdout", &[])), &sentinel);
        assert!(output.status.success(), "size={size}: {output:?}");
        assert_eq!(output.stdout, sentinel, "size={size}");
        assert_eq!(output.stderr, b"");
    }
}

#[test]
fn concurrent_launches_preserve_independent_input() {
    const LAUNCHERS: usize = 8;
    let start = std::sync::Barrier::new(LAUNCHERS);
    std::thread::scope(|scope| {
        for launcher in 0..LAUNCHERS {
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for sequence in 0..4 {
                    let input = vec![(launcher * 4 + sequence) as u8; 8193];
                    let tag = format!("{launcher}:{sequence}:");
                    let mut request = fixture("tagged-stdin", &[]);
                    request["environment"]["INPUT_TAG"] = json!(tag);
                    let output = run(frame(&request), &input);
                    assert!(output.status.success(), "{output:?}");
                    assert_eq!(
                        (output.stdout, output.stderr),
                        ([tag.as_bytes(), &input].concat(), vec![])
                    );
                }
            });
        }
    });
}

#[test]
fn invalid_frames_fail_on_stderr_only() {
    for bytes in [
        vec![],
        vec![0],
        vec![0, 0],
        vec![0, 0, 0],
        0u32.to_be_bytes().to_vec(),
        1048577u32.to_be_bytes().to_vec(),
        u32::MAX.to_be_bytes().to_vec(),
        vec![0, 0, 0, 2, b'{'],
        vec![0, 0, 0, 1, b'{'],
        vec![0, 0, 0, 1, 0xff],
        vec![0, 0, 0, 4, b'{', b'}', b'{', b'}'],
    ] {
        let output = run(bytes, &[]);
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"");
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn invalid_requests_fail_on_stderr_only() {
    let valid = request(&["/bin/echo", "must-not-launch"]);
    let mut requests = vec![request(&[]), request(&[""])];
    for (field, value) in [
        ("version", json!(1)),
        ("version", json!(3)),
        ("version", json!(-1)),
        ("command", json!("/bin/true")),
        ("cwd", json!("relative")),
        ("unexpected", json!(true)),
        ("filesystem", json!({"kind": "invalid"})),
        ("network", json!("invalid")),
        ("proxy", json!({"enabled": false})),
    ] {
        let mut request = valid.clone();
        request[field] = value;
        requests.push(request);
    }
    for request in requests {
        let output = run(frame(&request), &[]);
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"");
        assert!(!output.stderr.is_empty());
    }
}

fn fixture(operation: &str, args: &[&str]) -> Value {
    let executable = cargo_bin("mcp-console-sandbox-fixture").unwrap();
    let mut command = vec![executable.to_str().unwrap(), operation];
    command.extend_from_slice(args);
    request(&command)
}

#[test]
fn binary_stdout_and_stderr_are_inherited() {
    let bytes: Vec<u8> = (0..131073).map(|i| (i % 256) as u8).collect();
    for operation in ["stdout", "stderr"] {
        let output = run(frame(&fixture(operation, &[])), &bytes);
        assert!(output.status.success(), "{output:?}");
        let (actual, other) = if operation == "stdout" {
            (output.stdout, output.stderr)
        } else {
            (output.stderr, output.stdout)
        };
        assert_eq!(actual, bytes);
        assert_eq!(other, b"");
    }
}

#[test]
fn cwd_and_complete_environment_reach_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let mut request = fixture("context", &[]);
    request["cwd"] = json!(cwd);
    let value = "value with spaces and \u{00e9}";
    request["environment"] = json!({"ONLY_THIS": value});
    let staging = tempfile::tempdir().unwrap();
    let mut command = runner(staging.path());
    command.env("NOT_FOR_TARGET", "runner-only");
    let output = run_command(command, frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    // Some macOS toolchains initialize __CF_USER_TEXT_ENCODING before main.
    // Compare the complete native process environment, including that behavior.
    let mut native = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap());
    native
        .arg("context")
        .current_dir(&cwd)
        .env_clear()
        .env("ONLY_THIS", value);
    // Bubblewrap sets PWD to the command cwd as part of its native setup.
    #[cfg(target_os = "linux")]
    native.env("PWD", &cwd);
    let native = native.output().unwrap();
    assert!(native.status.success(), "{native:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::from_slice::<Value>(&native.stdout).unwrap()
    );
}

#[test]
fn filesystem_policy_denies_and_grants_writes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let file = root.join("created");
    let mut request = fixture("write", &[file.to_str().unwrap()]);
    let denied = run(frame(&request), &[]);
    assert!(!denied.status.success(), "{denied:?}");
    assert!(!file.exists());
    request["filesystem"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "path": {"type": "path", "path": root}, "access": "write"
        }));
    let allowed = run(frame(&request), &[]);
    assert!(allowed.status.success(), "{allowed:?}");
    assert_eq!(std::fs::read(file).unwrap(), b"created");
}

#[test]
fn exit_codes_and_native_signal_mapping_are_preserved() {
    for code in [0, 1, 42, 127, 255] {
        let output = run(frame(&fixture("exit", &[&code.to_string()])), &[]);
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
    }
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        let output = run(frame(&fixture("signal", &[&signal.to_string()])), &[]);
        assert_eq!(output.status.code(), Some(128 + signal), "{output:?}");
        assert_eq!(output.stdout, b"");
    }
}

#[test]
fn launch_failure_has_stderr_and_no_acknowledgment() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing-program");
    let output = run(frame(&request(&[missing.to_str().unwrap()])), &[]);
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert!(!output.stderr.is_empty());
}

fn proxy_config() -> Value {
    json!({
        "enabled": true,
        "enableSocks5": true,
        "enableSocks5Udp": false,
        "allowUpstreamProxy": false,
        "dangerouslyAllowAllUnixSockets": false,
        "mode": "full",
        "domains": {"127.0.0.1": "allow"},
        "unixSockets": null,
        "allowLocalBinding": true
    })
}

#[test]
fn no_launcher_or_proxy_descriptors_reach_the_target() {
    for proxy in [Value::Null, proxy_config()] {
        let inherited = tempfile::tempfile().unwrap();
        let raw = inherited.as_raw_fd();
        let staging = tempfile::tempdir().unwrap();
        let mut command = runner(staging.path());
        // SAFETY: the child hook only calls async-signal-safe fcntl. Duplicating
        // here avoids changing descriptor flags in the concurrent test process.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(raw, libc::F_DUPFD, 180) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut request = fixture("descriptors", &[]);
        request["proxy"] = proxy;
        let output = run_command(command, frame(&request), &[]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!([])
        );
    }
}

#[test]
fn network_permission_controls_direct_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut request = fixture("connect", &[&address]);
    let denied = run(frame(&request), &[]);
    assert!(!denied.status.success(), "{denied:?}");
    request["network"] = json!("enabled");
    let allowed = run(frame(&request), &[]);
    assert!(allowed.status.success(), "{allowed:?}");
}

#[test]
fn managed_proxy_applies_the_upstream_allowlist() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nallowed")
            .unwrap();
    });
    let mut request = fixture("proxy-get", &[&format!("http://{address}/")]);
    request["proxy"] = proxy_config();
    let output = run(frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.ends_with(b"allowed"), "{output:?}");
    server.join().unwrap();

    request["proxy"]["domains"] = json!({"127.0.0.1": "deny"});
    let denied = run(frame(&request), &[]);
    assert!(denied.status.success(), "{denied:?}");
    assert!(
        String::from_utf8_lossy(&denied.stdout).starts_with("HTTP/1.1 403"),
        "{denied:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn ordinary_seatbelt_profile_retains_native_sysctl_denials() {
    // The removed application profile allowed this query. The ordinary process
    // profile permits hw.ncpu but denies kern.boottime.
    for (name, allowed) in [("hw.ncpu", true), ("kern.boottime", false)] {
        let output = run(frame(&fixture("sysctl", &[name])), &[]);
        assert_eq!(output.status.success(), allowed, "{name}: {output:?}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_profile_extension_preserves_pty_workflows() {
    use std::ffi::CStr;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    let (mut master, mut slave) = (-1, -1);
    let mut name = [0; libc::PATH_MAX as usize];
    // SAFETY: openpty initializes the descriptors and pathname; null keeps
    // the default terminal settings.
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
    // SAFETY: the test owns both descriptors and openpty terminated the name.
    let (_master, _slave, name) = unsafe {
        (
            OwnedFd::from_raw_fd(master),
            OwnedFd::from_raw_fd(slave),
            CStr::from_ptr(name.as_ptr()).to_str().unwrap(),
        )
    };
    let extension = r#"
(deny file-read*
  (literal "/dev/tty")
  (regex #"^/dev/ttys[0-9]+$"))
(allow file-read* file-write* file-ioctl
  (require-all
    (regex #"^/dev/ttys[0-9]+$")
    (extension "com.apple.sandbox.pty")))
"#;
    for extension in [None, Some(Value::Null), Some(json!(extension))] {
        let mut request = fixture("pty", &[name]);
        let host_readable = !extension.as_ref().is_some_and(Value::is_string);
        if let Some(extension) = extension {
            request["macos_seatbelt_profile_extension"] = extension;
        }
        let output = run(frame(&request), &[]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stderr, b"");
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({"host_readable": host_readable, "input": "input\n", "output": "output"})
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_profile_extension_can_grant_native_permissions() {
    let mut request = fixture("sysctl", &["kern.boottime"]);
    request["macos_seatbelt_profile_extension"] =
        json!(r#"(allow sysctl-read (sysctl-name "kern.boottime"))"#);
    let output = run(frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

#[cfg(target_os = "macos")]
#[test]
fn malformed_macos_seatbelt_profile_extension_prevents_target_launch() {
    let mut request = request(&["/bin/echo", "launched"]);
    request["macos_seatbelt_profile_extension"] = json!("(invalid-sbpl-operation)");
    let output = run(frame(&request), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid-sbpl-operation"));
}

#[cfg(target_os = "linux")]
#[test]
fn macos_seatbelt_profile_extension_is_rejected_on_linux() {
    let mut request = request(&["/bin/echo", "launched"]);
    request["macos_seatbelt_profile_extension"] = json!("(deny network*)");
    let output = run(frame(&request), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("macos_seatbelt_profile_extension is supported only on macOS")
    );
}

#[test]
fn maximum_frame_and_prequeued_input_remain_separate() {
    let mut payload = serde_json::to_vec(&request(&["/bin/cat"])).unwrap();
    payload.resize(1048576, b' ');
    let mut bytes = 1048576u32.to_be_bytes().to_vec();
    bytes.extend(payload);
    let output = run(bytes, b"\0sentinel\xff");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"\0sentinel\xff");
    assert_eq!(output.stderr, b"");
}

#[test]
fn launch_does_not_wait_for_more_input_or_eof() {
    let staging = tempfile::tempdir().unwrap();
    let (mut child, mut bootstrap) = spawn(runner(staging.path()).stdin(Stdio::piped()));
    let stdin = child.stdin.take().unwrap();
    bootstrap
        .write_all(&frame(&request(&["/bin/echo", "started"])))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    // Neither channel needs EOF, and stdin has no data throughout setup.
    drop((stdin, bootstrap));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"started\n");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_prefers_a_suitable_host_bwrap() {
    let staging = tempfile::tempdir().unwrap();
    let command = runner(staging.path());
    let host = staging.path().join("host");
    std::fs::create_dir(&host).unwrap();
    copy_executable(
        &cargo_bin("mcp-console-sandbox-fixture").unwrap(),
        &host.join("bwrap"),
    );
    let mut request = request(&["/bin/echo", "target"]);
    request["environment"]["PATH"] = json!(host);
    let output = run_command(command, frame(&request), &[]);
    assert_eq!(output.status.code(), Some(97), "{output:?}");
    assert_eq!(output.stdout, b"");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_uses_the_ordinary_bundle_when_host_bwrap_is_missing_or_unsuitable() {
    for unsuitable in [false, true] {
        let staging = tempfile::tempdir().unwrap();
        let command = runner(staging.path());
        let host = staging.path().join("host");
        std::fs::create_dir(&host).unwrap();
        if unsuitable {
            copy_executable(
                &cargo_bin("mcp-console-sandbox-fixture").unwrap(),
                &host.join("bwrap"),
            );
        }
        let mut request = request(&["/bin/echo", "target"]);
        request["environment"] = json!({"PATH": host, "TEST_BWRAP_UNSUITABLE": "1"});
        let output = run_command(command, frame(&request), &[]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"target\n");
        assert_eq!(output.stderr, b"");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_host_bwrap_without_argv0_can_reexec_the_native_helper() {
    let staging = tempfile::tempdir().unwrap();
    let command = runner(staging.path());
    let host = staging.path().join("host");
    std::fs::create_dir(&host).unwrap();
    copy_executable(
        &cargo_bin("mcp-console-sandbox-fixture").unwrap(),
        &host.join("bwrap"),
    );
    let mut request = request(&["/bin/echo", "target"]);
    request["environment"] = json!({
        "PATH": host,
        "TEST_BWRAP_NO_ARGV0": "1",
        "TEST_BWRAP_EXECUTABLE": cargo_bin("bwrap").unwrap()
    });
    let output = run_command(command, frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"target\n");
    assert_eq!(output.stderr, b"");
}

#[test]
fn restricted_network_preserves_native_socket_denials_and_descriptor_io() {
    let fixture = cargo_bin("mcp-console-sandbox-fixture").unwrap();
    let mut request = request(&[fixture.to_str().unwrap(), "local-ipc"]);
    // Use the suitable host helper when present, just as an installed caller does.
    request["environment"] = json!({"PATH": std::env::var("PATH").unwrap()});
    let output = run(frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    #[cfg(target_os = "linux")]
    let denied = json!(libc::EPERM);
    #[cfg(target_os = "macos")]
    let denied = serde_json::Value::Null;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        json!({
            "operations": {
                "getsockname": denied,
                "getpeername": denied,
                "getsockopt": denied,
                "setsockopt": denied,
                "send": denied,
                "shutdown": denied,
            },
            "bytes": b"local IPC",
        }),
    );
    assert_eq!(output.stderr, b"");
}
