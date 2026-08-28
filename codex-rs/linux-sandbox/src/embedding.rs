use crate::bundled_bwrap;
use crate::bundled_bwrap::BundledBwrapLauncher;
use clap::Args;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

const PACKAGED_BWRAP_DIRECTORY: &str = "codex-resources";
const PACKAGED_BWRAP_NAME: &str = "bwrap";
const SYNTHETIC_MOUNT_REGISTRY_DIRECTORY: &str = "bwrap-synthetic-mount-registry";

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
    let runner_executable = runner_executable.canonicalize().map_err(|err| {
        format!(
            "failed to resolve embedding executable {}: {err}",
            runner_executable.display()
        )
    })?;
    let runner_dir = runner_executable.parent().ok_or_else(|| {
        format!(
            "embedding executable has no parent directory: {}",
            runner_executable.display()
        )
    })?;
    let bwrap = runner_dir
        .join(PACKAGED_BWRAP_DIRECTORY)
        .join(PACKAGED_BWRAP_NAME);
    embedding_config_for_paths(&bwrap, application_state_dir)
}

fn embedding_config_for_paths(
    bwrap: &Path,
    application_state_dir: &Path,
) -> Result<EmbeddingConfig, String> {
    let bwrap = bwrap.canonicalize().map_err(|err| {
        format!(
            "required packaged companion {} is unavailable: {err}",
            bwrap.display()
        )
    })?;
    if bwrap.to_str().is_none() {
        return Err("packaged bubblewrap path must be valid UTF-8 for embedding".to_string());
    }
    let has_packaged_layout = bwrap
        .file_name()
        .is_some_and(|name| name == PACKAGED_BWRAP_NAME)
        && bwrap
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == PACKAGED_BWRAP_DIRECTORY);
    if !has_packaged_layout {
        return Err(format!(
            "embedding bubblewrap must use the {PACKAGED_BWRAP_DIRECTORY}/{PACKAGED_BWRAP_NAME} layout: {}",
            bwrap.display()
        ));
    }
    let launcher = bundled_bwrap::launcher_for_program(&bwrap).ok_or_else(|| {
        format!(
            "required packaged companion is not executable: {}",
            bwrap.display()
        )
    })?;
    launcher.verify()?;

    let application_state_dir = application_state_dir.canonicalize().map_err(|err| {
        format!(
            "failed to resolve application state directory {}: {err}",
            application_state_dir.display()
        )
    })?;
    if !application_state_dir.is_dir() {
        return Err(format!(
            "application state path is not a directory: {}",
            application_state_dir.display()
        ));
    }
    if application_state_dir.to_str().is_none() {
        return Err("application state directory must be valid UTF-8".to_string());
    }
    let application_state_dir = AbsolutePathBuf::from_absolute_path(application_state_dir)
        .map_err(|err| format!("application state directory must be absolute: {err}"))?;
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
