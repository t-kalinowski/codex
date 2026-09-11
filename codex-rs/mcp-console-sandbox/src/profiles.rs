//! Built-in selection at the standalone runner boundary; no configuration discovery.

use crate::bootstrap::Bootstrap;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceOptions {
    pub exclude_tmpdir_env_var: bool,
    pub exclude_slash_tmp: bool,
}

pub fn resolve(request: &Bootstrap) -> Result<(FileSystemSandboxPolicy, NetworkSandboxPolicy)> {
    let explicit = request
        .filesystem
        .clone()
        .map(FileSystemSandboxPolicy::try_from)
        .transpose()
        .map_err(anyhow::Error::msg)
        .context("invalid filesystem policy")?;
    let Some(name) = request.extends.as_deref() else {
        ensure!(
            request.workspace.is_none() && request.workspace_options.is_none(),
            "workspace and workspace_options require extends"
        );
        return Ok((
            explicit.context("filesystem is required without extends")?,
            request
                .network
                .context("network is required without extends")?,
        ));
    };
    let workspace = request.workspace.as_ref().unwrap_or(&request.cwd);
    let baseline = match name {
        ":read-only" => {
            ensure!(
                request.workspace_options.is_none(),
                "workspace_options requires \":workspace\""
            );
            PermissionProfile::read_only()
        }
        ":workspace" => {
            let defaults = WorkspaceOptions::default();
            let options = request.workspace_options.as_ref().unwrap_or(&defaults);
            PermissionProfile::workspace_write_with(
                std::slice::from_ref(workspace),
                NetworkSandboxPolicy::Restricted,
                options.exclude_tmpdir_env_var,
                options.exclude_slash_tmp,
            )
        }
        _ => bail!(
            "unsupported built-in profile {name:?}; expected \":workspace\" or \":read-only\""
        ),
    };
    let network = request
        .network
        .unwrap_or_else(|| baseline.network_sandbox_policy());
    let mut filesystem = baseline.file_system_sandbox_policy();
    if let Some(explicit) = explicit {
        match explicit.kind {
            FileSystemSandboxKind::Restricted => {
                filesystem.entries.extend(explicit.entries);
                filesystem.glob_scan_max_depth = explicit.glob_scan_max_depth;
            }
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                filesystem = explicit;
            }
        }
    }
    let profile = PermissionProfile::from_runtime_permissions(&filesystem, network)
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(workspace));
    Ok((
        profile.file_system_sandbox_policy(),
        profile.network_sandbox_policy(),
    ))
}
