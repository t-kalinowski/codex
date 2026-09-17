//! Public Windows CLI. The shared sandbox crate owns launch and policy enforcement.

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::builder::PathBufValueParser;
use clap::builder::PossibleValuesParser;
use clap::builder::TypedValueParser;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Parser)]
#[command(
    name = "mcp-console-sandbox",
    version,
    about = "Run commands in the MCP Console Windows sandbox",
    after_help = "State defaults to %LOCALAPPDATA%\\mcp-console.\nSetup requires adjacent mcp-console-sandbox-setup.exe and mcp-console-sandbox-runner.exe.",
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    /// Persistent sandbox state directory (absolute path).
    #[arg(long, alias = "codex-home", global = true, value_name = "PATH", value_parser = absolute_path_parser())]
    pub state_dir: Option<AbsolutePathBuf>,

    #[command(subcommand)]
    pub action: Action,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Provision Windows accounts and network rules (requests UAC approval).
    Setup,
    /// Report setup records, accounts, and packaged helpers as JSON.
    Status,
    /// Run a command with the supplied permissions and environment.
    #[command(long_flag = "run-as-windows-sandbox")]
    Run(Box<RunArgs>),
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Absolute working directory for the target command.
    #[arg(long, value_name = "PATH", value_parser = absolute_path_parser())]
    pub command_cwd: AbsolutePathBuf,
    /// Workspace root; repeat for multiple roots (defaults to command-cwd).
    #[arg(long = "workspace-root", value_name = "PATH", value_parser = absolute_path_parser())]
    pub workspace_roots: Vec<AbsolutePathBuf>,
    /// Serialized PermissionProfile.
    #[arg(long, value_name = "JSON")]
    pub permission_profile: Json<PermissionProfile>,
    /// Target environment as a JSON object of string values.
    #[arg(long, value_name = "JSON")]
    pub env_json: Json<HashMap<String, String>>,
    /// Windows sandbox backend.
    #[arg(long, default_value = "elevated", value_parser = PossibleValuesParser::new(["disabled", "restricted-token", "elevated"]).try_map(|value| serde_json::from_value::<WindowsSandboxLevel>(value.into())))]
    pub windows_sandbox_level: WindowsSandboxLevel,
    /// Run on a private Windows desktop.
    #[arg(long)]
    pub windows_sandbox_private_desktop: bool,
    /// Keep existing proxy settings instead of reconciling them.
    #[arg(long)]
    pub preserve_proxy_settings: bool,
    /// Require managed proxy enforcement (elevated backend only).
    #[arg(long)]
    pub proxy_enforced: bool,
    /// Restricting SID for managed proxy access.
    #[arg(long, value_name = "SID")]
    pub network_proxy_restricting_sid: Option<String>,
    /// Override readable roots with a JSON array of paths.
    #[arg(long, value_name = "JSON")]
    pub read_roots_json: Option<Json<Vec<PathBuf>>>,
    /// Include platform defaults alongside the supplied readable roots.
    #[arg(long)]
    pub read_roots_include_platform_defaults: bool,
    /// Override writable roots with a JSON array of paths.
    #[arg(long, value_name = "JSON")]
    pub write_roots_json: Option<Json<Vec<PathBuf>>>,
    /// Deny reads beneath the absolute paths in this JSON array.
    #[arg(long, value_name = "JSON", default_value = "[]")]
    pub deny_read_paths_json: Json<Vec<AbsolutePathBuf>>,
    /// Deny writes beneath the absolute paths in this JSON array.
    #[arg(long, value_name = "JSON", default_value = "[]")]
    pub deny_write_paths_json: Json<Vec<AbsolutePathBuf>>,
    /// Target executable and arguments, following --.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

fn absolute_path_parser() -> impl TypedValueParser<Value = AbsolutePathBuf> {
    PathBufValueParser::new().try_map(AbsolutePathBuf::from_absolute_path_checked)
}

/// A single JSON option value, including arrays that must not become repeated CLI values.
#[derive(Clone, Debug)]
pub(crate) struct Json<T>(pub T);

impl<T: DeserializeOwned> FromStr for Json<T> {
    type Err = serde_json::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value).map(Self)
    }
}
