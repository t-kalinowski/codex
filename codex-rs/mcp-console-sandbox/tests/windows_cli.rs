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
fn help_describes_commands_and_run_options() {
    let mut help = String::new();
    for args in [
        vec!["--help"],
        vec!["setup", "--help"],
        vec!["status", "--help"],
        vec!["run", "--help"],
    ] {
        let output = runner()
            .expect("runner binary")
            .args(args)
            .env_remove("LOCALAPPDATA")
            .output()
            .expect("help");
        assert_eq!((output.status.code(), output.stderr), (Some(0), vec![]));
        help.push_str(&String::from_utf8(output.stdout).expect("help text"));
        help.push('\n');
    }
    insta::assert_snapshot!(help, @r"
Run commands in the MCP Console Windows sandbox

Usage: mcp-console-sandbox.exe [OPTIONS] <COMMAND>

Commands:
  setup                          Provision Windows accounts and network rules (requests UAC approval)
  status                         Report setup records, accounts, and packaged helpers as JSON
  run, --run-as-windows-sandbox  Run a command with the supplied permissions and environment
  help                           Print this message or the help of the given subcommand(s)

Options:
      --state-dir <PATH>  Persistent sandbox state directory (absolute path)
  -h, --help              Print help
  -V, --version           Print version

State defaults to %LOCALAPPDATA%\mcp-console.
Setup requires adjacent mcp-console-sandbox-setup.exe and mcp-console-sandbox-runner.exe.

Provision Windows accounts and network rules (requests UAC approval)

Usage: mcp-console-sandbox.exe setup [OPTIONS]

Options:
      --state-dir <PATH>  Persistent sandbox state directory (absolute path)
  -h, --help              Print help

Report setup records, accounts, and packaged helpers as JSON

Usage: mcp-console-sandbox.exe status [OPTIONS]

Options:
      --state-dir <PATH>  Persistent sandbox state directory (absolute path)
  -h, --help              Print help

Run a command with the supplied permissions and environment

Usage: mcp-console-sandbox.exe {run|--run-as-windows-sandbox} [OPTIONS] --command-cwd <PATH> --permission-profile <JSON> --env-json <JSON> -- <COMMAND>...

Arguments:
  <COMMAND>...  Target executable and arguments, following --

Options:
      --command-cwd <PATH>
          Absolute working directory for the target command
      --state-dir <PATH>
          Persistent sandbox state directory (absolute path)
      --workspace-root <PATH>
          Workspace root; repeat for multiple roots (defaults to command-cwd)
      --permission-profile <JSON>
          Serialized PermissionProfile
      --env-json <JSON>
          Target environment as a JSON object of string values
      --windows-sandbox-level <WINDOWS_SANDBOX_LEVEL>
          Windows sandbox backend [default: elevated] [possible values: disabled, restricted-token, elevated]
      --windows-sandbox-private-desktop
          Run on a private Windows desktop
      --preserve-proxy-settings
          Keep existing proxy settings instead of reconciling them
      --proxy-enforced
          Require managed proxy enforcement (elevated backend only)
      --network-proxy-restricting-sid <SID>
          Restricting SID for managed proxy access
      --read-roots-json <JSON>
          Override readable roots with a JSON array of paths
      --read-roots-include-platform-defaults
          Include platform defaults alongside the supplied readable roots
      --write-roots-json <JSON>
          Override writable roots with a JSON array of paths
      --deny-read-paths-json <JSON>
          Deny reads beneath the absolute paths in this JSON array [default: []]
      --deny-write-paths-json <JSON>
          Deny writes beneath the absolute paths in this JSON array [default: []]
  -h, --help
          Print help
");
}

#[test]
fn invalid_options_fail_before_creating_state() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = root.path().join("state");
    let state_text = state.to_str().expect("state path");
    let cwd = root.path().to_str().expect("working directory");
    for (args, diagnostic) in [
        (vec!["status", "--unknown"], "unexpected argument"),
        (
            vec![
                "status",
                "--state-dir",
                state_text,
                "--codex-home",
                state_text,
            ],
            "cannot be used multiple times",
        ),
        (
            vec!["status", "--state-dir", "relative"],
            "path is not absolute",
        ),
        (vec!["setup", "--", "cmd.exe"], "unexpected argument"),
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
        (
            vec!["run", "--windows-sandbox-level", "unknown"],
            "invalid value",
        ),
        (vec!["run"], "required arguments"),
        (
            vec![
                "run",
                "--command-cwd",
                cwd,
                "--permission-profile",
                r#"{"type":"external","network":"enabled"}"#,
                "--env-json",
                "{}",
                "cmd.exe",
            ],
            "unexpected argument",
        ),
        (
            vec![
                "run",
                "--command-cwd",
                cwd,
                "--permission-profile",
                r#"{"type":"external","network":"enabled"}"#,
                "--env-json",
                "{}",
                "--",
            ],
            "required arguments",
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
fn run_aliases_preserve_target_flags_and_repeated_workspace_roots() {
    let root = tempfile::tempdir().expect("tempdir");
    let system_root = std::env::var("SystemRoot").expect("SystemRoot");
    let profile = serde_json::to_string(&PermissionProfile::read_only()).expect("profile");
    for (action, state_flag) in [
        ("run", "--state-dir"),
        ("--run-as-windows-sandbox", "--codex-home"),
    ] {
        let output = runner()
            .expect("runner binary")
            .arg(action)
            .arg(state_flag)
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
