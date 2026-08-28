use crate::bundled_bwrap;
use crate::bundled_bwrap::BundledBwrapLauncher;
use crate::proxy_lifecycle::set_parent_death_signal;
use clap::Args;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_sandboxing::landlock::CODEX_LINUX_SANDBOX_ARG0;
use codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile_native;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStderr;
use std::process::Command;
use std::process::Stdio;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const PACKAGED_BWRAP_DIRECTORY: &str = "codex-resources";
const PACKAGED_BWRAP_NAME: &str = "bwrap";
const SYNTHETIC_MOUNT_REGISTRY_DIRECTORY: &str = "bwrap-synthetic-mount-registry";
const RUNTIME_PREFLIGHT_ARG: &str = "--codex-mcp-console-sandbox-linux-runtime-preflight-v1";
const RUNTIME_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RUNTIME_PREFLIGHT_STDERR_BYTES: usize = 4096;
const MAX_RUNTIME_PREFLIGHT_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;

/// Classifies whether Linux availability failed at the companion or native backend boundary.
#[derive(Debug, Eq, PartialEq)]
pub enum PackagedBwrapRuntimeError {
    Companion(String),
    Backend(String),
}

impl std::fmt::Display for PackagedBwrapRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Companion(message) | Self::Backend(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PackagedBwrapRuntimeError {}

#[derive(Debug)]
enum EmbeddingPreparationError {
    Companion(String),
    Infrastructure(String),
}

impl EmbeddingPreparationError {
    fn into_message(self) -> String {
        match self {
            Self::Companion(message) | Self::Infrastructure(message) => message,
        }
    }

    fn into_runtime_error(self) -> PackagedBwrapRuntimeError {
        match self {
            Self::Companion(message) => PackagedBwrapRuntimeError::Companion(message),
            Self::Infrastructure(message) => PackagedBwrapRuntimeError::Backend(message),
        }
    }
}

/// Exact packaged bubblewrap and application-owned state selected by an embedding runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingConfig {
    launcher: BundledBwrapLauncher,
    application_state_dir: AbsolutePathBuf,
    synthetic_mount_registry_root: AbsolutePathBuf,
}

impl EmbeddingConfig {
    /// Returns the canonical packaged bubblewrap executable path.
    pub fn program(&self) -> &Path {
        self.launcher.program()
    }

    /// Returns the canonical application-owned state directory.
    pub fn application_state_dir(&self) -> &Path {
        self.application_state_dir.as_path()
    }

    /// Returns the hidden helper arguments that pin bubblewrap and sandbox bookkeeping.
    pub fn helper_args(&self) -> Vec<OsString> {
        vec![
            OsString::from("--embedding-bwrap"),
            self.program().as_os_str().to_owned(),
            OsString::from("--embedding-state-dir"),
            self.application_state_dir.as_os_str().to_owned(),
            OsString::from("--embedding-command-as-pid-1"),
        ]
    }

    /// Returns helper arguments that request bubblewrap's private process-information report.
    pub fn helper_args_with_info_fd(&self, info_fd: RawFd) -> Vec<OsString> {
        assert!(info_fd >= 0, "bubblewrap information descriptor is invalid");
        let mut arguments = self.helper_args();
        arguments.extend([
            OsString::from("--embedding-bwrap-info-fd"),
            OsString::from(info_fd.to_string()),
        ]);
        arguments
    }
}

/// Resolves only `<runner-dir>/codex-resources/bwrap` and binds its bookkeeping to caller state.
pub fn prepare_packaged_bwrap(
    runner_executable: &Path,
    application_state_dir: &Path,
) -> Result<EmbeddingConfig, String> {
    prepare_packaged_bwrap_classified(runner_executable, application_state_dir)
        .map(|(_, embedding)| embedding)
        .map_err(EmbeddingPreparationError::into_message)
}

fn prepare_packaged_bwrap_classified(
    runner_executable: &Path,
    application_state_dir: &Path,
) -> Result<(PathBuf, EmbeddingConfig), EmbeddingPreparationError> {
    let runner_executable = runner_executable.canonicalize().map_err(|err| {
        EmbeddingPreparationError::Infrastructure(format!(
            "failed to resolve embedding executable {}: {err}",
            runner_executable.display()
        ))
    })?;
    let runner_dir = runner_executable.parent().ok_or_else(|| {
        EmbeddingPreparationError::Infrastructure(format!(
            "embedding executable has no parent directory: {}",
            runner_executable.display()
        ))
    })?;
    let bwrap = runner_dir
        .join(PACKAGED_BWRAP_DIRECTORY)
        .join(PACKAGED_BWRAP_NAME);
    let embedding = embedding_config_for_paths_classified(&bwrap, application_state_dir)?;
    Ok((runner_executable, embedding))
}

/// Executes the exact packaged helper pipeline with its required native namespaces.
pub fn verify_packaged_bwrap_runtime(
    runner_executable: &Path,
    application_state_dir: &Path,
) -> Result<(), PackagedBwrapRuntimeError> {
    let (runner_executable, embedding) =
        prepare_packaged_bwrap_classified(runner_executable, application_state_dir)
            .map_err(EmbeddingPreparationError::into_runtime_error)?;
    let runner_read_path =
        AbsolutePathBuf::from_absolute_path(&runner_executable).map_err(|err| {
            PackagedBwrapRuntimeError::Backend(format!(
                "embedding executable must be absolute: {err}"
            ))
        })?;
    let filesystem = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry::new(
            FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            },
            FileSystemAccessMode::Read,
        ),
        FileSystemSandboxEntry::new(runner_read_path.into(), FileSystemAccessMode::Read),
    ]);
    let permission_profile =
        PermissionProfile::from_runtime_permissions(&filesystem, NetworkSandboxPolicy::Restricted);
    let target = vec![
        runner_executable.as_os_str().to_owned(),
        OsString::from(RUNTIME_PREFLIGHT_ARG),
    ];
    let mut helper_arguments = embedding.helper_args();
    helper_arguments.extend(
        create_linux_sandbox_command_args_for_permission_profile_native(
            target,
            Path::new("/"),
            &permission_profile,
            Path::new("/"),
            /*use_legacy_landlock*/ false,
            /*allow_network_for_proxy*/ false,
        ),
    );

    let mut command = Command::new(&runner_executable);
    command
        .args(helper_arguments)
        .arg0(CODEX_LINUX_SANDBOX_ARG0)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let parent_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || set_parent_death_signal(parent_pid));
    }
    let mut child = command.spawn().map_err(|err| {
        PackagedBwrapRuntimeError::Backend(format!(
            "failed to start packaged Linux namespace preflight: {err}"
        ))
    })?;
    let Some(mut stderr) = child.stderr.take() else {
        terminate_runtime_preflight(&mut child);
        return Err(PackagedBwrapRuntimeError::Backend(
            "packaged Linux namespace preflight stderr was unavailable".to_string(),
        ));
    };
    if let Err(err) = set_runtime_preflight_stderr_nonblocking(&stderr) {
        terminate_runtime_preflight(&mut child);
        return Err(PackagedBwrapRuntimeError::Backend(format!(
            "failed to configure packaged Linux namespace preflight diagnostics: {err}"
        )));
    }
    let mut captured_stderr = RuntimePreflightStderr::default();
    let deadline = Instant::now() + RUNTIME_PREFLIGHT_TIMEOUT;
    loop {
        if let Err(err) = captured_stderr.read_available(&mut stderr) {
            terminate_runtime_preflight(&mut child);
            return Err(PackagedBwrapRuntimeError::Backend(format!(
                "failed to read packaged Linux namespace preflight diagnostics: {err}"
            )));
        }
        match runtime_preflight_has_exited(&child) {
            Ok(true) => {
                kill_runtime_preflight_group(&child);
                let status = child.wait().map_err(|err| {
                    PackagedBwrapRuntimeError::Backend(format!(
                        "failed to reap packaged Linux namespace preflight: {err}"
                    ))
                })?;
                captured_stderr.read_available(&mut stderr).map_err(|err| {
                    PackagedBwrapRuntimeError::Backend(format!(
                        "failed to read packaged Linux namespace preflight diagnostics: {err}"
                    ))
                })?;
                if status.success() {
                    return Ok(());
                }
                return Err(PackagedBwrapRuntimeError::Backend(
                    captured_stderr.format_error(&format!(
                        "packaged Linux namespace preflight failed with {status}"
                    )),
                ));
            }
            Ok(false) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(false) => {
                terminate_runtime_preflight(&mut child);
                captured_stderr.read_available(&mut stderr).map_err(|err| {
                    PackagedBwrapRuntimeError::Backend(format!(
                        "failed to read packaged Linux namespace preflight diagnostics: {err}"
                    ))
                })?;
                return Err(PackagedBwrapRuntimeError::Backend(
                    captured_stderr.format_error("packaged Linux namespace preflight timed out"),
                ));
            }
            Err(err) => {
                terminate_runtime_preflight(&mut child);
                return Err(PackagedBwrapRuntimeError::Backend(format!(
                    "failed to observe packaged Linux namespace preflight: {err}"
                )));
            }
        }
    }
}

/// Returns true only for the exact no-op target used by the runtime preflight.
pub fn dispatch_packaged_bwrap_runtime_preflight() -> bool {
    let mut arguments = std::env::args_os();
    let _ = arguments.next();
    arguments.next().as_deref() == Some(std::ffi::OsStr::new(RUNTIME_PREFLIGHT_ARG))
        && arguments.next().is_none()
}

fn runtime_preflight_has_exited(child: &Child) -> std::io::Result<bool> {
    let mut process_info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            process_info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let process_info = unsafe { process_info.assume_init() };
    Ok(unsafe { process_info.si_pid() } != 0)
}

fn kill_runtime_preflight_group(child: &Child) {
    if let Ok(process_group) = i32::try_from(child.id()) {
        // SAFETY: the unreaped preflight leader still reserves this process-group ID.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
}

fn terminate_runtime_preflight(child: &mut Child) {
    kill_runtime_preflight_group(child);
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Default)]
struct RuntimePreflightStderr {
    bytes: Vec<u8>,
    truncated: bool,
}

impl RuntimePreflightStderr {
    fn read_available(&mut self, stderr: &mut ChildStderr) -> std::io::Result<()> {
        let mut drained = 0;
        let mut buffer = [0_u8; 1024];
        while drained < MAX_RUNTIME_PREFLIGHT_DRAIN_BYTES_PER_POLL {
            match stderr.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => {
                    drained += read;
                    let available =
                        MAX_RUNTIME_PREFLIGHT_STDERR_BYTES.saturating_sub(self.bytes.len());
                    self.bytes.extend_from_slice(&buffer[..read.min(available)]);
                    self.truncated |= read > available;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        self.truncated = true;
        Ok(())
    }

    fn format_error(&self, summary: &str) -> String {
        if self.bytes.is_empty() {
            return summary.to_string();
        }
        let diagnostic = String::from_utf8_lossy(&self.bytes);
        let diagnostic = diagnostic.trim();
        if self.truncated {
            format!("{summary}: {diagnostic} [diagnostic truncated]")
        } else {
            format!("{summary}: {diagnostic}")
        }
    }
}

fn set_runtime_preflight_stderr_nonblocking(stderr: &ChildStderr) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn embedding_config_for_paths(
    bwrap: &Path,
    application_state_dir: &Path,
) -> Result<EmbeddingConfig, String> {
    embedding_config_for_paths_classified(bwrap, application_state_dir)
        .map_err(EmbeddingPreparationError::into_message)
}

fn embedding_config_for_paths_classified(
    bwrap: &Path,
    application_state_dir: &Path,
) -> Result<EmbeddingConfig, EmbeddingPreparationError> {
    let bwrap = bwrap.canonicalize().map_err(|err| {
        EmbeddingPreparationError::Companion(format!(
            "required packaged companion {} is unavailable: {err}",
            bwrap.display()
        ))
    })?;
    if bwrap.to_str().is_none() {
        return Err(EmbeddingPreparationError::Companion(
            "packaged bubblewrap path must be valid UTF-8 for embedding".to_string(),
        ));
    }
    let has_packaged_layout = bwrap
        .file_name()
        .is_some_and(|name| name == PACKAGED_BWRAP_NAME)
        && bwrap
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == PACKAGED_BWRAP_DIRECTORY);
    if !has_packaged_layout {
        return Err(EmbeddingPreparationError::Companion(format!(
            "embedding bubblewrap must use the {PACKAGED_BWRAP_DIRECTORY}/{PACKAGED_BWRAP_NAME} layout: {}",
            bwrap.display()
        )));
    }
    let launcher = bundled_bwrap::launcher_for_program(&bwrap).ok_or_else(|| {
        EmbeddingPreparationError::Companion(format!(
            "required packaged companion is not executable: {}",
            bwrap.display()
        ))
    })?;
    launcher
        .verify()
        .map_err(EmbeddingPreparationError::Companion)?;

    let application_state_dir = application_state_dir.canonicalize().map_err(|err| {
        EmbeddingPreparationError::Infrastructure(format!(
            "failed to resolve application state directory {}: {err}",
            application_state_dir.display()
        ))
    })?;
    if !application_state_dir.is_dir() {
        return Err(EmbeddingPreparationError::Infrastructure(format!(
            "application state path is not a directory: {}",
            application_state_dir.display()
        )));
    }
    if application_state_dir.to_str().is_none() {
        return Err(EmbeddingPreparationError::Infrastructure(
            "application state directory must be valid UTF-8".to_string(),
        ));
    }
    let application_state_dir = AbsolutePathBuf::from_absolute_path(application_state_dir)
        .map_err(|err| {
            EmbeddingPreparationError::Infrastructure(format!(
                "application state directory must be absolute: {err}"
            ))
        })?;
    let synthetic_mount_registry_root =
        application_state_dir.join(SYNTHETIC_MOUNT_REGISTRY_DIRECTORY);

    Ok(EmbeddingConfig {
        launcher,
        application_state_dir,
        synthetic_mount_registry_root,
    })
}

#[derive(Debug, Args)]
pub(crate) struct EmbeddingOptions {
    /// Exact packaged bubblewrap chosen by the embedding application.
    #[arg(long = "embedding-bwrap", hide = true, requires = "state_dir")]
    pub(crate) bwrap: Option<PathBuf>,

    /// Application-owned state directory used for sandbox bookkeeping.
    #[arg(long = "embedding-state-dir", hide = true, requires = "bwrap")]
    pub(crate) state_dir: Option<PathBuf>,

    /// Run the sandbox command as PID 1 so it owns namespace descendant reaping.
    #[arg(
        long = "embedding-command-as-pid-1",
        hide = true,
        default_value_t = false
    )]
    pub(crate) command_as_pid_1: bool,

    /// Inheritable descriptor for bubblewrap's host-visible child process report.
    #[arg(long = "embedding-bwrap-info-fd", hide = true, requires = "bwrap")]
    pub(crate) bwrap_info_fd: Option<RawFd>,
}

impl EmbeddingOptions {
    pub(crate) fn activate(self) {
        if self.bwrap_info_fd.is_some() && !self.command_as_pid_1 {
            panic!("bubblewrap process information requires the command-as-PID-1 embedding mode")
        }
        if let Some(info_fd) = self.bwrap_info_fd {
            let flags = unsafe { libc::fcntl(info_fd, libc::F_GETFD) };
            if flags == -1
                || unsafe { libc::fcntl(info_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1
            {
                panic!(
                    "invalid bubblewrap process-information descriptor {info_fd}: {}",
                    std::io::Error::last_os_error()
                )
            }
        }
        let config = match (self.bwrap, self.state_dir) {
            (Some(bwrap), Some(state_dir)) => {
                let config = embedding_config_for_paths(&bwrap, &state_dir)
                    .unwrap_or_else(|err| panic!("invalid Linux sandbox embedding input: {err}"));
                Some(config)
            }
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                panic!("embedding bubblewrap and state directory must be supplied together")
            }
        };

        if let Some(config) = config {
            std::fs::create_dir_all(config.synthetic_mount_registry_root.as_path()).unwrap_or_else(
                |err| {
                    panic!(
                        "failed to create embedding synthetic mount registry {}: {err}",
                        config.synthetic_mount_registry_root.as_path().display()
                    )
                },
            );
            EMBEDDING_CONFIG.set(config).unwrap_or_else(|_| {
                panic!("Linux sandbox embedding was configured more than once")
            });
        }
        COMMAND_AS_PID_1
            .set(self.command_as_pid_1)
            .unwrap_or_else(|_| panic!("Linux sandbox embedding was configured more than once"));
        BWRAP_INFO_FD
            .set(self.bwrap_info_fd)
            .unwrap_or_else(|_| panic!("Linux sandbox embedding was configured more than once"));
    }
}

static EMBEDDING_CONFIG: OnceLock<EmbeddingConfig> = OnceLock::new();
static COMMAND_AS_PID_1: OnceLock<bool> = OnceLock::new();
static BWRAP_INFO_FD: OnceLock<Option<RawFd>> = OnceLock::new();

pub(crate) fn selected_bwrap_launcher() -> Option<BundledBwrapLauncher> {
    EMBEDDING_CONFIG.get().map(|config| config.launcher.clone())
}

pub(crate) fn synthetic_mount_registry_root() -> Option<&'static Path> {
    EMBEDDING_CONFIG
        .get()
        .map(|config| config.synthetic_mount_registry_root.as_path())
}

pub(crate) fn command_as_pid_1() -> bool {
    COMMAND_AS_PID_1.get().copied().unwrap_or(false)
}

pub(crate) fn bwrap_info_fd() -> Option<RawFd> {
    BWRAP_INFO_FD.get().copied().flatten()
}
