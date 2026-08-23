use super::*;

impl RuntimeInner {
    pub(super) fn new_linux(
        state_dir: PathBuf,
        options: crate::LinuxOptions,
    ) -> Result<Self, SandboxError> {
        let linux_helper = LinuxRuntimeHelper::new(&state_dir, options.helper)?;
        let backend = SandboxBackend::LinuxBubblewrap;
        let application_cwd =
            std::env::current_dir().map_err(|source| SandboxError::Preparation {
                backend,
                message: "failed to resolve the embedding application's working directory"
                    .to_string(),
                source: Some(Box::new(source)),
            })?;
        let search_path = std::env::var_os("PATH");
        let linux_bwrap = codex_linux_sandbox::prepare_embedding_bwrap(
            search_path.as_deref(),
            &application_cwd,
            linux_helper.executable(),
        )
        .ok_or_else(|| SandboxError::BackendUnavailable {
            backend: Some(backend),
            message:
                "no packaged or system bubblewrap executable can create the required user namespace"
                    .to_string(),
        })?;
        validate_existing_path_encoding(linux_bwrap.program(), "the pinned bubblewrap executable")?;
        let network_denial = cfg!(any(target_arch = "x86_64", target_arch = "aarch64"))
            && linux_bwrap.network_namespace_available();
        Ok(Self {
            backend,
            capabilities: SandboxCapabilities {
                backend,
                minimal_read_policy: true,
                denied_read_paths: true,
                denied_write_paths: true,
                network_denial,
                network_unrestricted: true,
                interrupt: true,
                process_tree_termination: false,
            },
            state_dir,
            linux_helper,
            linux_bwrap,
        })
    }

    pub(super) fn linux_internal_read_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.linux_helper.read_roots();
        roots.push(self.linux_bwrap.program().to_path_buf());
        roots.sort();
        roots.dedup();
        roots
    }

    pub(super) fn validate_linux_backend(
        &self,
        network: crate::NetworkPolicy,
    ) -> Result<(), SandboxError> {
        validate_executable(&self.linux_helper.executable, self.backend)?;
        if !self.linux_bwrap.is_available() {
            return Err(SandboxError::BackendUnavailable {
                backend: Some(self.backend),
                message: "the pinned bubblewrap executable is unavailable or cannot create a user namespace"
                    .to_string(),
            });
        }
        if matches!(network, crate::NetworkPolicy::Denied)
            && (!self.capabilities.network_denial
                || !self.linux_bwrap.network_namespace_available())
        {
            return Err(SandboxError::UnsupportedPolicy {
                backend: self.backend,
                feature: crate::SandboxFeature::NetworkDenial,
                message:
                    "the pinned bubblewrap executable cannot create the required network namespace"
                        .to_string(),
            });
        }
        Ok(())
    }

    pub(super) async fn spawn_linux(
        self: &Arc<Self>,
        command: CommandSpec,
        prepared: crate::policy::PreparedPolicy,
        stdin_mode: ChildStdinMode,
    ) -> Result<SandboxedChild, SandboxError> {
        use codex_protocol::models::PermissionProfile;
        use codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile;

        if !self.capabilities.network_denial
            && prepared.network == codex_protocol::permissions::NetworkSandboxPolicy::Restricted
        {
            return Err(SandboxError::UnsupportedPolicy {
                backend: self.backend,
                feature: crate::SandboxFeature::NetworkDenial,
                message: "seccomp network denial is unavailable on this architecture".to_string(),
            });
        }
        let mut target = Vec::with_capacity(command.args.len() + 1);
        target.push(require_utf8_os(&command.program, "program")?.to_string());
        for argument in &command.args {
            target.push(require_utf8_os(argument, "argument")?.to_string());
        }
        let profile =
            PermissionProfile::from_runtime_permissions(&prepared.file_system, prepared.network);
        let helper_args = create_linux_sandbox_command_args_for_permission_profile(
            target,
            &command.cwd,
            &profile,
            &command.cwd,
            /*use_legacy_landlock*/ false,
            /*allow_network_for_proxy*/ false,
        );
        let mut process = tokio::process::Command::new(self.linux_helper.executable());
        process.arg("--embedding");
        process.arg("--embedding-bwrap");
        process.arg(self.linux_bwrap.program());
        process.arg("--embedding-bwrap-kind");
        process.arg(self.linux_bwrap.kind().as_str());
        process.arg("--embedding-registry-root");
        process.arg(self.linux_helper.registry_root());
        process.args(helper_args);
        configure_command(&mut process, &command.cwd, command.env);
        crate::process::spawn_unix(process, self.backend, Arc::clone(self), stdin_mode).await
    }
}

pub(super) struct LinuxRuntimeHelper {
    executable: PathBuf,
    current_executable: Option<PathBuf>,
    registry_root: PathBuf,
    _directory: tempfile::TempDir,
}

impl LinuxRuntimeHelper {
    fn new(state_dir: &Path, helper: crate::LinuxHelper) -> Result<Self, SandboxError> {
        let (mut executable, current_executable) = match helper {
            crate::LinuxHelper::External(executable) => {
                validate_executable(&executable, SandboxBackend::LinuxBubblewrap)?;
                (executable, None)
            }
            crate::LinuxHelper::CurrentExecutable => {
                if !EMBEDDED_HELPER_DISPATCH_REGISTERED.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(SandboxError::BackendUnavailable {
                        backend: Some(SandboxBackend::LinuxBubblewrap),
                        message: "LinuxHelper::CurrentExecutable requires dispatch_embedded_helper() at process startup"
                            .to_string(),
                    });
                }
                let current_executable =
                    std::env::current_exe().map_err(|source| SandboxError::Preparation {
                        backend: SandboxBackend::LinuxBubblewrap,
                        message: "failed to locate the embedding executable".to_string(),
                        source: Some(Box::new(source)),
                    })?;
                validate_executable(&current_executable, SandboxBackend::LinuxBubblewrap)?;
                (PathBuf::new(), Some(current_executable))
            }
        };
        let directory = tempfile::Builder::new()
            .prefix(".sandbox-helper-")
            .tempdir_in(state_dir)
            .map_err(|source| SandboxError::Preparation {
                backend: SandboxBackend::LinuxBubblewrap,
                message: "failed to create the private Linux helper directory".to_string(),
                source: Some(Box::new(source)),
            })?;
        let registry_root = directory.path().join("synthetic-mount-registry");
        std::fs::create_dir(&registry_root).map_err(|source| SandboxError::Preparation {
            backend: SandboxBackend::LinuxBubblewrap,
            message: "failed to create the private Linux helper registry".to_string(),
            source: Some(Box::new(source)),
        })?;
        if let Some(current_executable) = &current_executable {
            executable = directory
                .path()
                .join(codex_sandboxing::landlock::CODEX_LINUX_SANDBOX_ARG0);
            std::os::unix::fs::symlink(current_executable, &executable).map_err(|source| {
                SandboxError::Preparation {
                    backend: SandboxBackend::LinuxBubblewrap,
                    message: "failed to create the private Linux helper alias".to_string(),
                    source: Some(Box::new(source)),
                }
            })?;
        }
        Ok(Self {
            executable,
            current_executable,
            registry_root,
            _directory: directory,
        })
    }

    pub(super) fn executable(&self) -> &Path {
        &self.executable
    }

    fn registry_root(&self) -> &Path {
        &self.registry_root
    }

    fn read_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![
            self._directory.path().to_path_buf(),
            self.executable.clone(),
        ];
        if let Ok(target) = self.executable.canonicalize()
            && target != self.executable
        {
            roots.push(target);
        }
        roots.extend(self.current_executable.iter().cloned());
        roots.sort();
        roots.dedup();
        roots
    }
}

fn validate_executable(path: &Path, backend: SandboxBackend) -> Result<(), SandboxError> {
    use std::os::unix::fs::PermissionsExt;

    validate_absolute_path(path, "the Linux sandbox helper")?;
    require_utf8_path(path, "Linux sandbox helper path")?;
    let metadata = path
        .metadata()
        .map_err(|source| SandboxError::BackendUnavailable {
            backend: Some(backend),
            message: format!("failed to inspect Linux sandbox helper: {source}"),
        })?;
    validate_existing_path_encoding(path, "the Linux sandbox helper")?;
    if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
        Ok(())
    } else {
        Err(SandboxError::BackendUnavailable {
            backend: Some(backend),
            message: "the Linux sandbox helper is not an executable file".to_string(),
        })
    }
}
