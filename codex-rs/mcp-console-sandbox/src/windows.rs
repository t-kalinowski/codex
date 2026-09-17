//! Standalone Windows commands; security and provisioning stay in the shared backend.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use codex_protocol::config_types::WindowsSandboxProxySettingsMode;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_windows_sandbox::ResolvedWindowsSandboxPermissions;
use codex_windows_sandbox::SandboxSetupRequest;
use codex_windows_sandbox::WindowsSandboxProduct;
use codex_windows_sandbox::WindowsSandboxSessionRequest;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::windows_cli::Action;
use crate::windows_cli::Cli;

pub(crate) fn run() -> Result<i32> {
    let cli = Cli::parse();
    WindowsSandboxProduct::Console.initialize()?;
    let state_dir = match cli.state_dir {
        Some(path) => path,
        None => AbsolutePathBuf::from_absolute_path_checked(
            PathBuf::from(
                std::env::var_os("LOCALAPPDATA")
                    .context("LOCALAPPDATA is unavailable; pass --state-dir")?,
            )
            .join("mcp-console"),
        )?,
    };
    if matches!(cli.action, Action::Setup | Action::Status) {
        let configured = codex_windows_sandbox::check_sandbox_setup(state_dir.as_path())?;
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
        if matches!(cli.action, Action::Status) {
            println!(
                "{}",
                serde_json::json!({
                    "state_dir": state_dir,
                    "configured": configured,
                    "helpers_available": helpers_available,
                    "offline_account": "McpConsoleSandboxOff",
                    "online_account": "McpConsoleSandboxOn",
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
                command_cwd: state_dir.as_path(),
                env_map: &HashMap::new(),
                codex_home: state_dir.as_path(),
                proxy_enforced: false,
            })?;
        }
        if !codex_windows_sandbox::check_sandbox_setup(state_dir.as_path())? {
            bail!("sandbox setup did not produce current records and enabled accounts");
        }
        println!("Console sandbox is configured at {}", state_dir.display());
        return Ok(0);
    }
    // Refuse to reuse another product's provisioned accounts even if its marker is missing.
    codex_windows_sandbox::check_sandbox_setup(state_dir.as_path())?;
    let Action::Run(mut args) = cli.action else {
        unreachable!("setup and status returned above");
    };
    if args.workspace_roots.is_empty() {
        args.workspace_roots.push(args.command_cwd.clone());
    }
    let proxy_settings_mode = if args.preserve_proxy_settings {
        WindowsSandboxProxySettingsMode::Preserve
    } else {
        WindowsSandboxProxySettingsMode::Reconcile
    };
    // The shared session API needs Tokio to coordinate process exit, Ctrl+C,
    // stdin EOF, and output-drain timeouts.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let session = codex_windows_sandbox::spawn_windows_sandbox_session_for_level(
                WindowsSandboxSessionRequest {
                    permission_profile: &args.permission_profile.0,
                    workspace_roots: &args.workspace_roots,
                    codex_home: state_dir.as_path(),
                    command: args.command,
                    cwd: args.command_cwd.as_path(),
                    env_map: args.env_json.0,
                    windows_sandbox_level: args.windows_sandbox_level,
                    proxy_enforced: args.proxy_enforced,
                    network_proxy_restricting_sid: args.network_proxy_restricting_sid,
                    proxy_settings_mode,
                    timeout_ms: None,
                    read_roots_override: args
                        .read_roots_json
                        .as_ref()
                        .map(|paths| paths.0.as_slice()),
                    read_roots_include_platform_defaults: args.read_roots_include_platform_defaults,
                    write_roots_override: args
                        .write_roots_json
                        .as_ref()
                        .map(|paths| paths.0.as_slice()),
                    deny_read_paths_override: &args.deny_read_paths_json.0,
                    deny_write_paths_override: &args.deny_write_paths_json.0,
                    tty: false,
                    stdin_open: true,
                    use_private_desktop: args.windows_sandbox_private_desktop,
                },
            )
            .await?;
            Ok(codex_windows_sandbox::forward_sandbox_session_stdio(session).await)
        })
}
