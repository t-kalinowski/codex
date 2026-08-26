use crate::BackendPreference;
use crate::CommandSpec;
use crate::SandboxBackend;
use crate::SandboxCapabilities;
use crate::SandboxError;
use crate::SandboxFeature;
use crate::SandboxLifetime;
use crate::SandboxRequest;
use crate::SandboxRuntimeConfig;
use crate::SandboxStdio;
use crate::TerminalPolicy;
use crate::policy::prepare_policy;
use crate::policy::validate_absolute_path;
use crate::policy::validate_existing_path_encoding;
use crate::process::SandboxedChild;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(target_os = "linux")]
#[path = "runtime/linux.rs"]
mod linux;
#[cfg(target_os = "macos")]
#[path = "runtime/macos.rs"]
mod macos;
#[cfg(target_os = "windows")]
#[path = "runtime/windows.rs"]
mod windows;

#[cfg(target_os = "linux")]
static EMBEDDED_HELPER_DISPATCH_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Dispatch the embedded Linux helper when this executable was invoked through its reserved alias.
/// Call this before creating threads or a Tokio runtime.
pub fn dispatch_embedded_helper() {
    #[cfg(target_os = "linux")]
    {
        let executable_name = std::env::args_os().next().and_then(|arg0| {
            std::path::PathBuf::from(arg0)
                .file_name()
                .map(ToOwned::to_owned)
        });
        if executable_name.as_deref() == Some(std::ffi::OsStr::new("codex-linux-sandbox")) {
            codex_linux_sandbox::run_main();
        }
        EMBEDDED_HELPER_DISPATCH_REGISTERED.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Opaque owner of one selected native sandbox backend.
pub struct SandboxRuntime {
    inner: Arc<RuntimeInner>,
}

impl SandboxRuntime {
    pub fn new(config: SandboxRuntimeConfig) -> Result<Self, SandboxError> {
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = config;
            return Err(SandboxError::UnsupportedPlatform {
                platform: std::env::consts::OS.to_string(),
            });
        }

        let SandboxRuntimeConfig {
            state_dir,
            backend,
            #[cfg(target_os = "linux")]
            linux,
            #[cfg(target_os = "windows")]
                windows: _,
        } = config;
        match backend {
            BackendPreference::PlatformDefault => {}
        }
        validate_absolute_path(&state_dir, "the sandbox state directory")?;
        std::fs::create_dir_all(&state_dir).map_err(|source| SandboxError::Preparation {
            backend: host_backend(),
            message: format!(
                "failed to create application-owned state directory {}",
                state_dir.display()
            ),
            source: Some(Box::new(source)),
        })?;
        let state_dir = state_dir
            .canonicalize()
            .map_err(|source| SandboxError::Preparation {
                backend: host_backend(),
                message: "failed to resolve the application-owned state directory".to_string(),
                source: Some(Box::new(source)),
            })?;
        if !state_dir.is_dir() {
            return Err(SandboxError::InvalidPath {
                path: state_dir,
                message: "the sandbox state path must be a directory".to_string(),
            });
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        validate_existing_path_encoding(&state_dir, "the sandbox state directory")?;

        #[cfg(target_os = "macos")]
        let inner = RuntimeInner::new_macos(state_dir)?;
        #[cfg(target_os = "linux")]
        let inner = RuntimeInner::new_linux(state_dir, linux)?;
        #[cfg(target_os = "windows")]
        let inner = RuntimeInner::new_windows(state_dir)?;
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        let inner: RuntimeInner = unreachable!();
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn capabilities(&self) -> SandboxCapabilities {
        self.inner.capabilities
    }

    pub async fn spawn(&self, request: SandboxRequest) -> Result<SandboxedChild, SandboxError> {
        validate_command_cwd(&request.command)?;
        self.inner.validate_command(&request.command)?;
        if request.lifetime == SandboxLifetime::SupervisedProcessTree
            && !self.inner.capabilities.process_tree_termination
        {
            return Err(SandboxError::UnsupportedPolicy {
                backend: self.inner.backend,
                feature: SandboxFeature::ProcessTreeTermination,
                message: "durable supervised process-tree ownership is unavailable".to_string(),
            });
        }
        if request.policy.terminal == TerminalPolicy::InheritedAndCreatedOnly
            && !self.inner.capabilities.terminal_isolation
        {
            return Err(SandboxError::UnsupportedPolicy {
                backend: self.inner.backend,
                feature: SandboxFeature::TerminalIsolation,
                message: "isolating host terminal-device paths is unavailable".to_string(),
            });
        }
        self.inner
            .validate_backend(&request.command, request.policy.network)?;
        let internal_read_roots = self.inner.internal_read_roots();
        let prepared = prepare_policy(
            &request.policy,
            &request.command.cwd,
            self.inner.backend,
            &internal_read_roots,
        )?;
        self.inner
            .spawn(
                request.command,
                prepared,
                request.stdio,
                request.lifetime,
                request.policy.terminal,
            )
            .await
    }
}

pub(crate) struct RuntimeInner {
    pub(crate) backend: SandboxBackend,
    capabilities: SandboxCapabilities,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    state_dir: PathBuf,
    #[cfg(target_os = "linux")]
    linux_helper: linux::LinuxRuntimeHelper,
    #[cfg(target_os = "linux")]
    linux_bwrap: codex_linux_sandbox::EmbeddingBwrapLauncher,
}

impl RuntimeInner {
    fn internal_read_roots(&self) -> Vec<PathBuf> {
        #[cfg(target_os = "linux")]
        return self.linux_internal_read_roots();
        #[cfg(target_os = "windows")]
        return vec![self.state_dir.clone()];
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        Vec::new()
    }

    fn validate_command(&self, command: &CommandSpec) -> Result<(), SandboxError> {
        if command.program.is_empty() {
            return Err(SandboxError::InvalidCommand {
                message: "program must not be empty".to_string(),
            });
        }
        if !Path::new(&command.program).is_absolute() {
            return Err(SandboxError::InvalidCommand {
                message: "program must be an absolute path".to_string(),
            });
        }
        require_no_nul(&command.program, "program")?;
        for argument in &command.args {
            require_no_nul(argument, "argument")?;
        }
        for (key, value) in &command.env {
            require_valid_environment_key(key)?;
            require_no_nul(value, "environment value")?;
            #[cfg(target_os = "linux")]
            if key.as_encoded_bytes().starts_with(b"LD_") {
                return Err(SandboxError::InvalidCommand {
                    message: "Linux child environment keys beginning with LD_ are unsupported because they would affect the pre-sandbox helper"
                        .to_string(),
                });
            }
            #[cfg(target_os = "macos")]
            if key.as_encoded_bytes().starts_with(b"DYLD_") {
                return Err(SandboxError::InvalidCommand {
                    message: "macOS child environment keys beginning with DYLD_ are unsupported because sandbox-exec removes them before target launch"
                        .to_string(),
                });
            }
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            require_utf8_os(&command.program, "program")?;
            for argument in &command.args {
                require_utf8_os(argument, "argument")?;
            }
        }
        #[cfg(target_os = "windows")]
        {
            let mut folded_keys = std::collections::BTreeSet::new();
            for (key, value) in &command.env {
                let key = require_utf8_os(key, "environment key")?;
                require_utf8_os(value, "environment value")?;
                if !folded_keys.insert(key.to_uppercase()) {
                    return Err(SandboxError::InvalidCommand {
                        message: "environment keys must be unique ignoring case on Windows"
                            .to_string(),
                    });
                }
            }
        }
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        require_utf8_path(&command.cwd, "command working directory")?;
        Ok(())
    }

    fn validate_backend(
        &self,
        command: &CommandSpec,
        network: crate::NetworkPolicy,
    ) -> Result<(), SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let _ = command;
            self.validate_linux_backend(network)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, network);
            Ok(())
        }
    }

    async fn spawn(
        self: &Arc<Self>,
        command: CommandSpec,
        prepared: crate::policy::PreparedPolicy,
        stdio: SandboxStdio,
        lifetime: SandboxLifetime,
        terminal: TerminalPolicy,
    ) -> Result<SandboxedChild, SandboxError> {
        #[cfg(not(target_os = "macos"))]
        let _ = terminal;
        #[cfg(target_os = "macos")]
        return self
            .spawn_macos(command, prepared, stdio, lifetime, terminal)
            .await;
        #[cfg(target_os = "linux")]
        return self.spawn_linux(command, prepared, stdio, lifetime).await;
        #[cfg(target_os = "windows")]
        return self.spawn_windows(command, prepared, stdio, lifetime).await;
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        Err(SandboxError::UnsupportedPlatform {
            platform: std::env::consts::OS.to_string(),
        })
    }
}

/// Terminates every other live member of the caller's isolated sandbox process group.
pub fn terminate_current_process_group_members() -> Result<(), SandboxError> {
    #[cfg(target_os = "macos")]
    {
        let process_id = std::process::id();
        let process_id_native =
            libc::pid_t::try_from(process_id).map_err(|_| SandboxError::InvalidOperation {
                message: "the current process ID cannot identify a native process group"
                    .to_string(),
            })?;
        let process_group_id = unsafe { libc::getpgrp() };
        let session_id = unsafe {
            libc::getsid(/*pid*/ 0)
        };
        if process_group_id != process_id_native || session_id != process_id_native {
            return Err(SandboxError::InvalidOperation {
                message:
                    "the caller is not the leader of its own isolated process group and session"
                        .to_string(),
            });
        }
        codex_utils_pty::process_group::kill_process_group_members_except(process_id, process_id)
            .map_err(|source| {
                SandboxError::io("terminating current sandbox process-group members", source)
            })
    }

    #[cfg(not(target_os = "macos"))]
    Err(SandboxError::UnsupportedPolicy {
        backend: host_backend(),
        feature: SandboxFeature::CurrentProcessGroupTermination,
        message: "terminating the current process group while preserving its leader is unavailable on this backend"
            .to_string(),
    })
}

fn validate_command_cwd(command: &CommandSpec) -> Result<(), SandboxError> {
    validate_absolute_path(&command.cwd, "the command working directory")?;
    let metadata = command
        .cwd
        .metadata()
        .map_err(|source| SandboxError::InvalidPath {
            path: command.cwd.clone(),
            message: format!("failed to inspect the command working directory: {source}"),
        })?;
    validate_existing_path_encoding(&command.cwd, "the command working directory")?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(SandboxError::InvalidPath {
            path: command.cwd.clone(),
            message: "the command working directory is not a directory".to_string(),
        })
    }
}

#[cfg(unix)]
fn configure_command(
    process: &mut tokio::process::Command,
    cwd: &Path,
    env: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) {
    process.current_dir(cwd);
    process.env_clear();
    process.envs(env);
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn require_utf8_os<'a>(
    value: &'a std::ffi::OsStr,
    description: &'static str,
) -> Result<&'a str, SandboxError> {
    value.to_str().ok_or_else(|| SandboxError::InvalidCommand {
        message: format!("{description} must be valid UTF-8 for the selected backend"),
    })
}

fn require_no_nul(value: &std::ffi::OsStr, description: &'static str) -> Result<(), SandboxError> {
    if value.as_encoded_bytes().contains(&0) {
        return Err(SandboxError::InvalidCommand {
            message: format!("{description} must not contain an interior NUL"),
        });
    }
    Ok(())
}

fn require_valid_environment_key(key: &std::ffi::OsStr) -> Result<(), SandboxError> {
    require_no_nul(key, "environment key")?;
    let bytes = key.as_encoded_bytes();
    if bytes.is_empty() {
        return Err(SandboxError::InvalidCommand {
            message: "environment keys must not be empty".to_string(),
        });
    }
    #[cfg(windows)]
    let contains_invalid_equals = match bytes.strip_prefix(b"=") {
        Some(remainder) => remainder.is_empty() || remainder.contains(&b'='),
        None => bytes.contains(&b'='),
    };
    #[cfg(not(windows))]
    let contains_invalid_equals = bytes.contains(&b'=');
    if contains_invalid_equals {
        return Err(SandboxError::InvalidCommand {
            message: "environment keys must not contain `=` except for a Windows drive-current-directory prefix"
                .to_string(),
        });
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn require_utf8_path(path: &Path, description: &'static str) -> Result<(), SandboxError> {
    path.to_str()
        .map(|_| ())
        .ok_or_else(|| SandboxError::InvalidPath {
            path: path.to_path_buf(),
            message: format!("{description} must be valid UTF-8 for the selected backend"),
        })
}

fn host_backend() -> SandboxBackend {
    #[cfg(target_os = "macos")]
    return SandboxBackend::MacosSeatbelt;
    #[cfg(target_os = "linux")]
    return SandboxBackend::LinuxBubblewrap;
    #[cfg(target_os = "windows")]
    return SandboxBackend::WindowsRestrictedToken;
    #[allow(unreachable_code)]
    SandboxBackend::LinuxBubblewrap
}
