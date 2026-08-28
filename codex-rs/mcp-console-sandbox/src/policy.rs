use crate::protocol::FileSystemBase;
use crate::protocol::FileSystemPolicy;
use crate::protocol::LaunchRequest;
use crate::protocol::MissingPathBehavior;
use crate::protocol::PathAccess;
use crate::protocol::PlatformExtensions;
use crate::protocol::SetupRequest;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use crate::protocol::TerminalPolicy;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::path::PathBuf;

const MAX_LIFECYCLE_DEADLINE_MS: u64 = 5 * 60 * 1000;

pub struct ValidatedPolicy {
    pub working_directory: AbsolutePathBuf,
    pub policy_base_directory: AbsolutePathBuf,
    pub filesystem: FileSystemSandboxPolicy,
}

pub fn validate_launch_support(launch: &LaunchRequest) -> Result<()> {
    validate_filesystem_support(&launch.filesystem)?;
    validate_platform_extensions(&launch.platform_extensions)?;
    crate::network::validate_network_policy(&launch.network)?;
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if launch.terminal == TerminalPolicy::IsolateHostDevices {
        bail!("host terminal-device isolation is unsupported by this backend")
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if launch.lifecycle.terminate_grace_ms != 0 {
        #[cfg(target_os = "linux")]
        bail!(
            "graceful termination cannot cross this release's Linux bubblewrap session boundary; lifecycle.terminate_grace_ms must be zero"
        );
        #[cfg(windows)]
        bail!(
            "graceful termination is unsupported by the Windows elevated backend; lifecycle.terminate_grace_ms must be zero"
        );
    }
    Ok(())
}

pub fn validate_setup_support(setup: &SetupRequest) -> Result<()> {
    validate_filesystem_support(&setup.filesystem)?;
    validate_platform_extensions(&setup.platform_extensions)?;
    crate::network::validate_network_policy(&setup.network)
}

fn validate_filesystem_support(filesystem: &FileSystemPolicy) -> Result<()> {
    if cfg!(target_os = "windows") && filesystem.base == FileSystemBase::PlatformMinimal {
        bail!(
            "filesystem.base=platform_minimal is unsupported by this release's Windows elevated backend"
        );
    }
    Ok(())
}

pub fn validate_launch(
    launch: &LaunchRequest,
    state_directory: &Path,
    target_executable: &Path,
) -> Result<ValidatedPolicy> {
    validate_launch_support(launch)?;
    validate_deadline(
        launch.lifecycle.root_exit_grace_ms,
        "lifecycle.root_exit_grace_ms",
    )?;
    validate_deadline(
        launch.lifecycle.terminate_grace_ms,
        "lifecycle.terminate_grace_ms",
    )?;
    validate_deadline(
        launch.lifecycle.force_timeout_ms,
        "lifecycle.force_timeout_ms",
    )?;
    validate_policy(
        &launch.working_directory,
        &launch.policy_base_directory,
        &launch.filesystem,
        &launch.platform_extensions,
        state_directory,
        Some(target_executable),
    )
}

pub fn validate_setup(setup: &SetupRequest, state_directory: &Path) -> Result<ValidatedPolicy> {
    validate_setup_support(setup)?;
    validate_policy(
        &setup.working_directory,
        &setup.policy_base_directory,
        &setup.filesystem,
        &setup.platform_extensions,
        state_directory,
        /*target_executable*/ None,
    )
}

fn validate_policy(
    working_directory: &str,
    policy_base_directory: &str,
    filesystem: &FileSystemPolicy,
    platform_extensions: &PlatformExtensions,
    state_directory: &Path,
    target_executable: Option<&Path>,
) -> Result<ValidatedPolicy> {
    let working_directory = absolute_existing_directory(working_directory, "working_directory")?;
    let policy_base_directory =
        absolute_existing_directory(policy_base_directory, "policy_base_directory")?;
    validate_platform_extensions(platform_extensions)?;
    let filesystem = compile_filesystem_policy(filesystem, state_directory, target_executable)?;
    Ok(ValidatedPolicy {
        working_directory,
        policy_base_directory,
        filesystem,
    })
}

fn compile_filesystem_policy(
    policy: &FileSystemPolicy,
    state_directory: &Path,
    target_executable: Option<&Path>,
) -> Result<FileSystemSandboxPolicy> {
    let state_directory = AbsolutePathBuf::from_absolute_path(state_directory)
        .context("application state directory must be absolute")?;
    let base = match policy.base {
        FileSystemBase::PlatformMinimal => FileSystemSpecialPath::Minimal,
        FileSystemBase::HostReadOnly => FileSystemSpecialPath::Root,
    };
    let mut entries = vec![FileSystemSandboxEntry::new(
        FileSystemPath::Special { value: base },
        FileSystemAccessMode::Read,
    )];
    let target_paths = target_executable
        .into_iter()
        .flat_map(|target| std::iter::once(target.to_path_buf()).chain(target.canonicalize().ok()))
        .collect::<Vec<_>>();
    let infrastructure_paths = infrastructure_paths()?;
    for rule in &policy.rules {
        let path = absolute_path(&rule.path, "filesystem rule path")?;
        let canonical_path = path.as_path().canonicalize().ok();
        if rule.access == PathAccess::Deny
            && target_paths.iter().any(|target| {
                target.starts_with(path.as_path())
                    || canonical_path
                        .as_deref()
                        .is_some_and(|path| target.starts_with(path))
            })
        {
            bail!("filesystem deny rule cannot cover the target executable")
        }
        if path.as_path().starts_with(state_directory.as_path())
            || canonical_path
                .as_deref()
                .is_some_and(|path| path.starts_with(state_directory.as_path()))
        {
            bail!("filesystem rules cannot target the runner-owned state directory")
        }
        if rule.access != PathAccess::Read
            && infrastructure_paths.iter().any(|infrastructure| {
                (if infrastructure.as_path().is_dir() {
                    path.as_path().starts_with(infrastructure.as_path())
                        || canonical_path
                            .as_deref()
                            .is_some_and(|path| path.starts_with(infrastructure.as_path()))
                } else {
                    path == *infrastructure
                        || canonical_path.as_deref() == Some(infrastructure.as_path())
                }) || rule.access == PathAccess::Deny
                    && (infrastructure.as_path().starts_with(path.as_path())
                        || canonical_path
                            .as_deref()
                            .is_some_and(|path| infrastructure.as_path().starts_with(path)))
            })
        {
            bail!("filesystem rules cannot override runner or companion resources")
        }
        let exists = path.as_path().try_exists().with_context(|| {
            format!(
                "could not inspect filesystem rule path `{}`",
                path.as_path().display()
            )
        })?;
        if !exists {
            match rule.missing {
                MissingPathBehavior::Error => {
                    bail!(
                        "filesystem rule path does not exist: `{}`",
                        path.as_path().display()
                    )
                }
                MissingPathBehavior::Ignore => continue,
            }
        }
        let access = match rule.access {
            PathAccess::Read => FileSystemAccessMode::Read,
            PathAccess::Write => FileSystemAccessMode::Write,
            PathAccess::Deny => FileSystemAccessMode::Deny,
        };
        entries.push(FileSystemSandboxEntry::new(path.into(), access));
    }

    if let Some(target_executable) = target_executable {
        let target_executable = AbsolutePathBuf::from_absolute_path(target_executable)
            .context("target executable must be absolute")?;
        entries.push(FileSystemSandboxEntry::new(
            target_executable.into(),
            FileSystemAccessMode::Read,
        ));
    }

    entries.push(FileSystemSandboxEntry::new(
        state_directory.clone().into(),
        FileSystemAccessMode::Deny,
    ));
    entries.extend(
        infrastructure_paths
            .into_iter()
            .map(|path| FileSystemSandboxEntry::new(path.into(), FileSystemAccessMode::Read)),
    );
    if let Ok(canonical_state_directory) = state_directory.as_path().canonicalize()
        && canonical_state_directory != state_directory.as_path()
    {
        let canonical_state_directory =
            AbsolutePathBuf::from_absolute_path(canonical_state_directory)
                .context("canonical application state directory must be absolute")?;
        entries.push(FileSystemSandboxEntry::new(
            canonical_state_directory.into(),
            FileSystemAccessMode::Deny,
        ));
    }
    Ok(FileSystemSandboxPolicy::restricted(entries))
}

fn infrastructure_paths() -> Result<Vec<AbsolutePathBuf>> {
    let executable = std::env::current_exe().context("resolve runner executable")?;
    let executable = AbsolutePathBuf::from_absolute_path(executable)
        .context("runner executable must be absolute")?;
    let mut paths = vec![executable.clone()];
    if let Some(parent) = executable.as_path().parent() {
        let resources = parent.join("codex-resources");
        if resources
            .try_exists()
            .context("inspect runner companion directory")?
        {
            paths.push(
                AbsolutePathBuf::from_absolute_path(resources)
                    .context("runner companion directory must be absolute")?,
            );
        }
    }
    let canonical = paths
        .iter()
        .filter_map(|path| path.as_path().canonicalize().ok())
        .filter_map(|path| AbsolutePathBuf::from_absolute_path(path).ok())
        .collect::<Vec<_>>();
    paths.extend(canonical);
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub fn absolute_target(
    target: &[std::ffi::OsString],
) -> Result<(&std::ffi::OsStr, &[std::ffi::OsString])> {
    let Some((program, arguments)) = target.split_first() else {
        bail!("launch requires an absolute native target executable argument")
    };
    let path = Path::new(program);
    if !path.is_absolute() {
        bail!("target executable must be absolute")
    }
    if path.to_str().is_none() {
        bail!("target executable path must be valid Unicode in protocol version 1")
    }
    let metadata = path
        .metadata()
        .with_context(|| format!("target executable is unavailable: `{}`", path.display()))?;
    if !metadata.is_file() {
        bail!("target executable is not a file: `{}`", path.display())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("target executable is not executable: `{}`", path.display())
        }
    }
    Ok((program.as_os_str(), arguments))
}

fn absolute_existing_directory(value: &str, field: &str) -> Result<AbsolutePathBuf> {
    let path = absolute_path(value, field)?;
    let metadata = path
        .as_path()
        .metadata()
        .with_context(|| format!("{field} is unavailable: `{}`", path.as_path().display()))?;
    if !metadata.is_dir() {
        bail!("{field} is not a directory: `{}`", path.as_path().display())
    }
    Ok(path)
}

fn absolute_path(value: &str, field: &str) -> Result<AbsolutePathBuf> {
    let path = PathBuf::from(value);
    AbsolutePathBuf::from_absolute_path(path).with_context(|| format!("{field} must be absolute"))
}

fn validate_deadline(value: u64, field: &str) -> Result<()> {
    if value > MAX_LIFECYCLE_DEADLINE_MS {
        bail!("{field} exceeds the {MAX_LIFECYCLE_DEADLINE_MS} ms limit")
    }
    Ok(())
}

fn validate_platform_extensions(extensions: &PlatformExtensions) -> Result<()> {
    if extensions.macos.is_some() && !cfg!(target_os = "macos") {
        bail!("macOS platform extensions are unsupported on this host")
    }
    if extensions.linux.is_some() && !cfg!(target_os = "linux") {
        bail!("Linux platform extensions are unsupported on this host")
    }
    if extensions.windows.is_some() && !cfg!(target_os = "windows") {
        bail!("Windows platform extensions are unsupported on this host")
    }
    Ok(())
}
