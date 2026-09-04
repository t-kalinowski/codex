#![cfg(unix)]

#[path = "../src/codex.rs"]
mod codex;

use codex::cargo_bin;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::io::Write;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;

fn request(command: &[&str]) -> Value {
    json!({
        "version": 1,
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

fn frame(request: &Value, input: &[u8]) -> Vec<u8> {
    let payload = serde_json::to_vec(request).unwrap();
    let mut bytes = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    bytes.extend(payload);
    bytes.extend(input);
    bytes
}

fn run(bytes: Vec<u8>) -> Output {
    let mut child = Command::new(cargo_bin("mcp-console-sandbox").unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&bytes));
    let output = child.wait_with_output().unwrap();
    // Rejected frames may close the pipe before the complete input is written.
    if let Err(error) = writer.join().unwrap() {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    output
}

#[test]
fn valid_bootstrap_launches_one_command() {
    let output = run(frame(&request(&["/bin/echo", "launched"]), &[]));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"launched\n");
    assert_eq!(output.stderr, b"");
}

#[test]
fn stdin_starts_immediately_after_bootstrap() {
    for size in [0, 1, 255, 4095, 4096, 8191, 8192, 65537, 1048576] {
        let sentinel: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let output = run(frame(&request(&["/bin/cat"]), &sentinel));
        assert!(output.status.success(), "size={size}: {output:?}");
        assert_eq!(output.stdout, sentinel, "size={size}");
        assert_eq!(output.stderr, b"");
    }
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
    ] {
        let output = run(bytes);
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"");
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn invalid_requests_fail_on_stderr_only() {
    let mut unknown = request(&["/bin/true"]);
    unknown["version"] = json!(2);
    for request in [unknown, request(&[])] {
        let output = run(frame(&request, &[]));
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"");
        assert!(!output.stderr.is_empty());
    }
}
