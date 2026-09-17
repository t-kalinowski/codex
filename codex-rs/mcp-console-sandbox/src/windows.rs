//! Standalone Windows commands; security and provisioning stay in the shared backend.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_protocol::models::PermissionProfile;
use codex_windows_sandbox::ResolvedWindowsSandboxPermissions;
use codex_windows_sandbox::SandboxSetupRequest;
use codex_windows_sandbox::WindowsSandboxProduct;
use std::collections::HashMap;
use std::path::PathBuf;

pub(crate) fn run() -> Result<i32> {
    WindowsSandboxProduct::Console.initialize()?;
    let mut args = std::env::args().skip(1);
    let action = args.next().unwrap_or_else(|| "help".to_owned());
    if matches!(action.as_str(), "help" | "--help" | "-h") {
        println!(
            "MCP Console sandbox\n\n  setup [--state-dir PATH]  Provision Windows accounts and network rules (UAC)\n  status [--state-dir PATH] Report setup records, accounts, and packaged helpers\n  run [--state-dir PATH] <native Windows options> -- <command> [args...]\n\nState defaults to %LOCALAPPDATA%\\mcp-console. Run defaults to the elevated backend.\nSetup requires adjacent mcp-console-sandbox-setup.exe and mcp-console-sandbox-runner.exe.\nSee WINDOWS.md for permission-profile and environment options."
        );
        return Ok(0);
    }
    if !matches!(
        action.as_str(),
        "setup" | "status" | "run" | "--run-as-windows-sandbox"
    ) {
        bail!(
            "Windows requires --run-as-windows-sandbox with native Windows arguments; --config-env and --bootstrap-fd are not supported (see WINDOWS.md)"
        );
    }
    let mut state_dir = None;
    let mut native_args = Vec::new();
    let mut explicit_level = false;
    while let Some(arg) = args.next() {
        if arg == "--" {
            native_args.push(arg);
            native_args.extend(args);
            break;
        }
        if arg == "--state-dir" || arg == "--codex-home" {
            if state_dir.is_some() {
                bail!("state directory specified more than once");
            }
            state_dir = Some(PathBuf::from(
                args.next().context("missing state directory")?,
            ));
        } else if matches!(action.as_str(), "setup" | "status") {
            bail!("unexpected {action} argument: {arg}");
        } else {
            explicit_level |= arg == "--windows-sandbox-level";
            let takes_value = !matches!(
                arg.as_str(),
                "--windows-sandbox-private-desktop"
                    | "--preserve-proxy-settings"
                    | "--proxy-enforced"
                    | "--read-roots-include-platform-defaults"
            );
            native_args.push(arg);
            if takes_value {
                native_args.push(args.next().context("missing native option value")?);
            }
        }
    }
    let state_dir = match state_dir {
        Some(path) => path,
        None => PathBuf::from(
            std::env::var_os("LOCALAPPDATA")
                .context("LOCALAPPDATA is unavailable; pass --state-dir")?,
        )
        .join("mcp-console"),
    };
    if !state_dir.is_absolute() {
        bail!("--state-dir must be absolute: {}", state_dir.display());
    }
    if matches!(action.as_str(), "setup" | "status") {
        if !native_args.is_empty() {
            bail!("{action} does not accept a target command");
        }
        let configured = codex_windows_sandbox::check_sandbox_setup(&state_dir)?;
        let executable = std::env::current_exe()?;
        let directory = executable
            .parent()
            .context("executable has no parent directory")?;
        let helpers_available = [
            "mcp-console-sandbox-setup.exe",
            "mcp-console-sandbox-runner.exe",
        ]
        .iter()
        .all(|name| directory.join(name).is_file());
        if action == "status" {
            println!(
                "{}",
                serde_json::json!({
                    "state_dir": state_dir,
                    "configured": configured,
                    "helpers_available": helpers_available,
                    "offline_account": "ConsoleSandboxOff",
                    "online_account": "ConsoleSandboxOn",
                })
            );
            return Ok(if configured && helpers_available {
                0
            } else {
                1
            });
        }
        if !helpers_available {
            bail!(
                "setup requires adjacent mcp-console-sandbox-setup.exe and mcp-console-sandbox-runner.exe"
            );
        }
        if !configured {
            let permissions = ResolvedWindowsSandboxPermissions::try_from_permission_profile(
                &PermissionProfile::read_only(),
            )?;
            codex_windows_sandbox::run_elevated_setup(SandboxSetupRequest {
                permissions: &permissions,
                command_cwd: &state_dir,
                env_map: &HashMap::new(),
                codex_home: &state_dir,
                proxy_enforced: false,
            })?;
        }
        if !codex_windows_sandbox::check_sandbox_setup(&state_dir)? {
            bail!("sandbox setup did not produce current records and enabled accounts");
        }
        println!("Console sandbox is configured at {}", state_dir.display());
        return Ok(0);
    }
    // Refuse to reuse another product's provisioned accounts even if its marker is missing.
    codex_windows_sandbox::check_sandbox_setup(&state_dir)?;
    let mut forwarded = vec![
        "--codex-home".to_owned(),
        state_dir
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("state directory is not valid Unicode"))?,
    ];
    if !explicit_level {
        forwarded.extend(["--windows-sandbox-level".to_owned(), "elevated".to_owned()]);
    }
    forwarded.extend(native_args);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(codex_windows_sandbox::run_windows_sandbox_wrapper_args(
            forwarded,
        ))
}
