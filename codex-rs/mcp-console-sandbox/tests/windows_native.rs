#![cfg(windows)]

use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::process::Command;

#[test]
fn status_uses_default_state_without_creating_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let output = Command::new(cargo_bin("mcp-console-sandbox").expect("runner binary"))
        .arg("status")
        .env("LOCALAPPDATA", root.path())
        .output()
        .expect("status");
    let status: Value = serde_json::from_slice(&output.stdout).expect("status json");
    assert_eq!(
        (
            output.status.code(),
            status["state_dir"].clone(),
            status["configured"].clone()
        ),
        (
            Some(1),
            json!(root.path().join("mcp-console")),
            json!(false)
        ),
    );
    assert!(!root.path().join("mcp-console").exists());
}

#[test]
fn console_refuses_other_products_state_before_setup_or_launch() {
    let root = tempfile::tempdir().expect("tempdir");
    let secrets = root.path().join(".sandbox-secrets");
    std::fs::create_dir(&secrets).expect("secrets");
    let contents = json!({
        "version": 5,
        "offline": { "username": "CodexSandboxOffline", "password": "unused" },
        "online": { "username": "CodexSandboxOnline", "password": "unused" },
    })
    .to_string();
    std::fs::write(secrets.join("sandbox_users.json"), &contents).expect("users");
    for action in ["setup", "status", "run"] {
        let mut command = Command::new(cargo_bin("mcp-console-sandbox").expect("runner binary"));
        command.args([action, "--state-dir"]).arg(root.path());
        if action == "run" {
            command.arg("--command-cwd").arg(root.path()).args([
                "--permission-profile",
                r#"{"type":"external","network":"enabled"}"#,
                "--env-json",
                "{}",
                "--",
                "cmd.exe",
            ]);
        }
        let output = command.output().expect("runner");
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8(output.stderr)
                .expect("stderr")
                .contains("different product")
        );
    }
    assert_eq!(
        std::fs::read_to_string(secrets.join("sandbox_users.json")).expect("users"),
        contents
    );
    assert!(!root.path().join(".sandbox").exists());
}
