use super::*;

impl RuntimeInner {
    pub(super) fn new_macos(state_dir: PathBuf) -> Result<Self, SandboxError> {
        let executable = Path::new(codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        if !executable.is_file() {
            return Err(SandboxError::BackendUnavailable {
                backend: Some(SandboxBackend::MacosSeatbelt),
                message: "the Seatbelt launcher is not installed".to_string(),
            });
        }
        let backend = SandboxBackend::MacosSeatbelt;
        Ok(Self {
            backend,
            capabilities: SandboxCapabilities {
                backend,
                minimal_read_policy: true,
                denied_read_paths: true,
                denied_write_paths: true,
                network_denial: true,
                network_unrestricted: true,
                interrupt: true,
                process_tree_termination: false,
            },
            state_dir,
        })
    }

    pub(super) async fn spawn_macos(
        self: &Arc<Self>,
        command: CommandSpec,
        prepared: crate::policy::PreparedPolicy,
        stdin_mode: ChildStdinMode,
    ) -> Result<SandboxedChild, SandboxError> {
        use codex_sandboxing::seatbelt::CreateSeatbeltCommandArgsParams;
        use codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
        use codex_sandboxing::seatbelt::create_seatbelt_command_args;

        let seatbelt_args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
            command: Vec::new(),
            file_system_sandbox_policy: &prepared.file_system,
            network_sandbox_policy: prepared.network,
            sandbox_policy_cwd: &command.cwd,
            enforce_managed_network: false,
            managed_network: None,
            environment_id: None,
            network: None,
            extra_allow_unix_sockets: &[],
        })
        .map_err(|message| SandboxError::Preparation {
            backend: self.backend,
            message,
            source: None,
        })?;
        let mut process = tokio::process::Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        process.args(seatbelt_args);
        process.arg(command.program);
        process.args(command.args);
        configure_command(&mut process, &command.cwd, command.env);
        crate::process::spawn_unix(process, self.backend, Arc::clone(self), stdin_mode).await
    }
}
