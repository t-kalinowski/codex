#![cfg(windows)]

use codex_protocol::models::PermissionProfile;
use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn runner() -> anyhow::Result<Command> {
    Ok(Command::new(cargo_bin("mcp-console-sandbox")?))
}

#[test]
fn invalid_paths_and_json_fail_before_creating_state() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = root.path().join("state");
    for (args, diagnostic) in [
        (
            vec!["status", "--state-dir", "relative"],
            "path is not absolute",
        ),
        (
            vec!["run", "--command-cwd", "relative"],
            "path is not absolute",
        ),
        (
            vec!["run", "--workspace-root", "relative"],
            "path is not absolute",
        ),
        (
            vec!["run", "--permission-profile", "not-json"],
            "invalid value",
        ),
        (vec!["run", "--env-json", "[]"], "invalid value"),
        (vec!["run", "--read-roots-json", "{}"], "invalid value"),
        (
            vec!["run", "--deny-read-paths-json", r#"["relative"]"#],
            "invalid value",
        ),
    ] {
        let output = runner()
            .expect("runner binary")
            .arg("--state-dir")
            .arg(&state)
            .args(&args)
            .output()
            .expect("invalid invocation");
        assert_eq!(
            (output.status.code(), output.stdout),
            (Some(2), vec![]),
            "{args:?}"
        );
        let stderr = String::from_utf8(output.stderr).expect("stderr");
        assert!(stderr.contains(diagnostic), "{args:?}: {stderr}");
        assert!(!state.exists());
    }
}

#[test]
fn run_preserves_target_flags_and_repeated_workspace_roots() {
    let root = tempfile::tempdir().expect("tempdir");
    let system_root = std::env::var("SystemRoot").expect("SystemRoot");
    let profile = serde_json::to_string(&PermissionProfile::read_only()).expect("profile");
    let output = runner()
        .expect("runner binary")
        .arg("run")
        .arg("--state-dir")
        .arg(root.path().join("state"))
        .arg("--command-cwd")
        .arg(root.path())
        .arg("--workspace-root")
        .arg(root.path())
        .arg("--workspace-root")
        .arg(&system_root)
        .arg(format!("--permission-profile={profile}"))
        .arg(format!(
            "--env-json={}",
            json!({ "SystemRoot": system_root })
        ))
        .args(["--windows-sandbox-level=restricted-token", "--"])
        .arg(Path::new(&system_root).join("System32").join("cmd.exe"))
        .args([
            "/d",
            "/c",
            "echo",
            "--state-dir",
            "--help",
            "--windows-sandbox-level",
        ])
        .output()
        .expect("sandbox run");
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8(output.stdout).expect("stdout"),
            output.stderr
        ),
        (
            Some(0),
            "--state-dir --help --windows-sandbox-level\r\n".to_owned(),
            vec![]
        )
    );
}

#[test]
fn typed_options_reach_shared_backend_validation() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = root.path().join("state");
    let paths = json!([root.path()]).to_string();
    let output = runner()
        .expect("runner binary")
        .arg("run")
        .arg("--state-dir")
        .arg(&state)
        .arg("--command-cwd")
        .arg(root.path())
        .arg("--permission-profile")
        .arg(serde_json::to_string(&PermissionProfile::read_only()).expect("profile"))
        .args([
            "--env-json",
            "{}",
            "--windows-sandbox-level",
            "restricted-token",
        ])
        .args([
            "--read-roots-json",
            &paths,
            "--write-roots-json",
            &paths,
            "--deny-read-paths-json",
            &paths,
            "--deny-write-paths-json",
            &paths,
        ])
        .args([
            "--windows-sandbox-private-desktop",
            "--preserve-proxy-settings",
            "--read-roots-include-platform-defaults",
            "--proxy-enforced",
            "--network-proxy-restricting-sid",
            "S-1-5-21-100-200-300-400",
            "--",
            "cmd.exe",
        ])
        .output()
        .expect("backend validation");
    assert_eq!((output.status.code(), output.stdout), (Some(1), vec![]));
    assert!(
        String::from_utf8(output.stderr)
            .expect("stderr")
            .contains("managed networking requires the elevated Windows sandbox backend")
    );
    assert!(!state.exists());
}
