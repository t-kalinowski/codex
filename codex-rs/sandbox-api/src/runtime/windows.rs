use super::*;

impl RuntimeInner {
    pub(super) fn new_windows(state_dir: PathBuf) -> Result<Self, SandboxError> {
        let backend = SandboxBackend::WindowsRestrictedToken;
        Ok(Self {
            backend,
            capabilities: SandboxCapabilities {
                backend,
                minimal_read_policy: false,
                denied_read_paths: false,
                denied_write_paths: true,
                network_denial: false,
                network_unrestricted: true,
                interrupt: false,
                process_tree_termination: true,
            },
            state_dir,
        })
    }

    pub(super) async fn spawn_windows(
        self: &Arc<Self>,
        command: CommandSpec,
        prepared: crate::policy::PreparedPolicy,
        stdin_mode: ChildStdinMode,
    ) -> Result<SandboxedChild, SandboxError> {
        use codex_protocol::models::PermissionProfile;
        use codex_utils_absolute_path::AbsolutePathBuf;
        use codex_windows_sandbox::WindowsSandboxEmbeddingRequest;
        use std::collections::HashMap;

        let cwd = AbsolutePathBuf::from_absolute_path(&command.cwd).map_err(|source| {
            SandboxError::InvalidPath {
                path: command.cwd.clone(),
                message: source.to_string(),
            }
        })?;
        let state_dir = AbsolutePathBuf::from_absolute_path(&self.state_dir).map_err(|source| {
            SandboxError::InvalidPath {
                path: self.state_dir.clone(),
                message: source.to_string(),
            }
        })?;
        let mut target = Vec::with_capacity(command.args.len() + 1);
        target.push(require_utf8_os(&command.program, "program")?.to_string());
        for argument in &command.args {
            target.push(require_utf8_os(argument, "argument")?.to_string());
        }
        let env_map = command
            .env
            .into_iter()
            .map(|(key, value)| {
                Ok((
                    require_utf8_os(&key, "environment key")?.to_string(),
                    require_utf8_os(&value, "environment value")?.to_string(),
                ))
            })
            .collect::<Result<HashMap<_, _>, SandboxError>>()?;
        let deny_write_paths = prepared
            .file_system
            .get_writable_roots_with_cwd(&cwd)
            .into_iter()
            .flat_map(|root| root.read_only_subpaths)
            .collect::<Vec<_>>();
        let permission_profile =
            PermissionProfile::from_runtime_permissions(&prepared.file_system, prepared.network);
        let spawned = codex_windows_sandbox::spawn_windows_sandbox_session_for_embedding(
            WindowsSandboxEmbeddingRequest {
                permission_profile: &permission_profile,
                state_dir: &state_dir,
                command: target,
                cwd: &cwd,
                env_map,
                additional_deny_write_paths: &deny_write_paths,
                stdin_open: matches!(stdin_mode, ChildStdinMode::Open),
            },
        )
        .await
        .map_err(|source| SandboxError::Spawn {
            backend: self.backend,
            message: "the restricted-token backend rejected the process".to_string(),
            source: Some(source.into()),
        })?;
        Ok(SandboxedChild::from_windows(
            spawned,
            self.backend,
            Arc::clone(self),
            stdin_mode,
        ))
    }
}
