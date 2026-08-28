use crate::identity::load_marker;
use crate::identity::load_prepared_sandbox_creds;
use crate::identity::prepared_sandbox_users_match_policy_namespace;
use crate::policy_namespace::WindowsSandboxPolicyNamespace;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::setup::ElevationPayload;
use crate::setup::SETUP_EXE_FILENAME;
use crate::setup::SETUP_VERSION;
use crate::setup::SandboxNetworkIdentity;
use crate::setup::SandboxSetupRequest;
use crate::setup::SetupMode;
use crate::setup::SetupRootOverrides;
use crate::setup::WINDOWS_PLATFORM_DEFAULT_READ_ROOTS;
use crate::setup::WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY;
use crate::setup::build_payload_deny_write_paths;
use crate::setup::build_payload_roots;
use crate::setup::is_elevated;
use crate::setup::run_setup_exe_at_with_policy_lease;
use crate::setup::run_setup_network_verification_exe_at_with_policy_lease;
use crate::setup::setup_refresh_deny_read_paths;
use anyhow::Context;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;

const COMMAND_RUNNER_FILENAME: &str = "codex-command-runner.exe";
const STANDALONE_POLICY_NAMESPACE: WindowsSandboxPolicyNamespace =
    WindowsSandboxPolicyNamespace::McpConsole;

#[doc(hidden)]
pub const WINDOWS_SANDBOX_STANDALONE_VERIFY_NETWORK_SWITCH: &str =
    "--standalone-verify-network-policy";

/// Exact helper executables staged by the embedding application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsSandboxStandaloneResources {
    pub setup_executable: PathBuf,
    pub command_runner_executable: PathBuf,
}

/// Windows identity selected by the normalized network policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsSandboxStandaloneNetworkIdentity {
    /// The WFP-confined account used for denied or managed-proxy networking.
    Offline,
    /// The ordinary sandbox account used for unrestricted networking.
    Online,
}

/// Network-related state installed for the selected sandbox identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsSandboxStandaloneNetworkSetup {
    pub identity: WindowsSandboxStandaloneNetworkIdentity,
    pub proxy_ports: Vec<u16>,
    pub allow_local_binding: bool,
}

/// Optional normalized root overrides applied by Codex's existing Windows
/// policy translation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowsSandboxStandaloneFilesystemOverrides {
    pub read_roots: Option<Vec<PathBuf>>,
    pub read_roots_include_platform_defaults: bool,
    pub write_roots: Option<Vec<PathBuf>>,
    pub additional_deny_read_paths: Vec<PathBuf>,
    pub additional_deny_write_paths: Vec<PathBuf>,
}

/// Inputs needed to translate one managed permission profile into standalone
/// Windows setup state.
pub struct WindowsSandboxStandalonePolicyRequest<'a> {
    pub permission_profile: &'a PermissionProfile,
    pub workspace_roots: &'a [AbsolutePathBuf],
    pub command_cwd: &'a Path,
    pub environment: &'a HashMap<String, String>,
    pub state_dir: PathBuf,
    pub resources: WindowsSandboxStandaloneResources,
    pub filesystem_overrides: WindowsSandboxStandaloneFilesystemOverrides,
    pub network: WindowsSandboxStandaloneNetworkSetup,
}

/// Explicit application-owned Windows setup and ACL request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsSandboxStandaloneSetupRequest {
    pub state_dir: PathBuf,
    pub resources: WindowsSandboxStandaloneResources,
    pub command_cwd: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub read_roots_include_platform_defaults: bool,
    pub write_roots: Vec<PathBuf>,
    pub deny_read_paths: Vec<PathBuf>,
    pub deny_write_paths: Vec<PathBuf>,
    pub network: WindowsSandboxStandaloneNetworkSetup,
}

/// Read-only setup inspection result. Inspection never launches a helper or
/// requests elevation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsSandboxStandaloneSetupState {
    Ready,
    AdministrativeActionRequired { reason: String },
    Unavailable { reason: String },
}

/// Successful explicit setup operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsSandboxStandaloneSetupOperation {
    AlreadyReady,
    Prepared,
    Refreshed,
}

/// Returns whether an environment name carries setup-only Windows proxy state.
///
/// Embedders must remove these values after constructing the standalone setup
/// request and before passing the complete native environment to the target.
pub fn is_windows_sandbox_standalone_setup_only_environment_variable(key: &OsStr) -> bool {
    key.to_str()
        .is_some_and(|key| key.eq_ignore_ascii_case(WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY))
}

/// Resolves a managed permission profile through the same Windows root and
/// deny translation used by existing Codex callers.
pub fn windows_sandbox_standalone_setup_request_from_permission_profile(
    request: WindowsSandboxStandalonePolicyRequest<'_>,
) -> Result<WindowsSandboxStandaloneSetupRequest> {
    let WindowsSandboxStandalonePolicyRequest {
        permission_profile,
        workspace_roots,
        command_cwd,
        environment,
        state_dir,
        resources,
        filesystem_overrides,
        network,
    } = request;
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )
        .context("resolve permission profile for standalone Windows sandbox")?;
    if !permissions.is_enforceable_by_windows_sandbox() {
        anyhow::bail!("permission profile cannot be enforced by the Windows sandbox");
    }
    let expected_identity = if permissions.should_apply_network_block() {
        WindowsSandboxStandaloneNetworkIdentity::Offline
    } else {
        WindowsSandboxStandaloneNetworkIdentity::Online
    };
    if network.identity != expected_identity {
        anyhow::bail!(
            "permission profile requires the {expected_identity:?} Windows sandbox identity"
        );
    }

    let WindowsSandboxStandaloneFilesystemOverrides {
        read_roots,
        read_roots_include_platform_defaults,
        write_roots,
        additional_deny_read_paths,
        additional_deny_write_paths,
    } = filesystem_overrides;
    let mut deny_read_paths =
        setup_refresh_deny_read_paths(permission_profile, workspace_roots, command_cwd)?;
    deny_read_paths.extend(additional_deny_read_paths);
    let overrides = SetupRootOverrides {
        read_roots,
        read_roots_include_platform_defaults,
        write_roots,
        deny_read_paths: Some(deny_read_paths.clone()),
        deny_write_paths: Some(additional_deny_write_paths),
    };
    let native_request = SandboxSetupRequest {
        permissions: &permissions,
        command_cwd,
        env_map: environment,
        codex_home: &state_dir,
        proxy_enforced: matches!(
            network.identity,
            WindowsSandboxStandaloneNetworkIdentity::Offline
        ),
    };
    let (read_roots, write_roots) = build_payload_roots(&native_request, &overrides);
    let deny_write_paths =
        build_payload_deny_write_paths(&native_request, overrides.deny_write_paths);

    Ok(WindowsSandboxStandaloneSetupRequest {
        state_dir,
        resources,
        command_cwd: command_cwd.to_path_buf(),
        read_roots,
        read_roots_include_platform_defaults: false,
        write_roots,
        deny_read_paths,
        deny_write_paths,
        network,
    })
}

pub(super) fn is_absolute_local_disk_path(path: &Path) -> bool {
    path.is_absolute()
        && matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                )
        )
}

fn validate_unicode_absolute_path(path: &Path, label: &str) -> Result<()> {
    if !is_absolute_local_disk_path(path) {
        anyhow::bail!("{label} must be an absolute local-disk Windows path");
    }
    if path.to_str().is_none() {
        anyhow::bail!("{label} is not valid Unicode; setup protocol version 1 rejects it");
    }
    Ok(())
}

pub(super) fn validate_resource_layout(
    resources: &WindowsSandboxStandaloneResources,
) -> Result<()> {
    for (path, filename, label) in [
        (
            resources.setup_executable.as_path(),
            SETUP_EXE_FILENAME,
            "setup executable",
        ),
        (
            resources.command_runner_executable.as_path(),
            COMMAND_RUNNER_FILENAME,
            "command runner executable",
        ),
    ] {
        validate_unicode_absolute_path(path, label)?;
        if path.file_name() != Some(OsStr::new(filename)) {
            anyhow::bail!("{label} must be named {filename}");
        }
        if !path.is_file() {
            anyhow::bail!("{label} is missing: {}", path.display());
        }
    }
    let setup_parent = resources
        .setup_executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("setup executable has no parent directory"))?;
    let runner_parent = resources
        .command_runner_executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("command runner executable has no parent directory"))?;
    if crate::path_normalization::canonicalize_path(setup_parent)
        != crate::path_normalization::canonicalize_path(runner_parent)
    {
        anyhow::bail!("standalone Windows helper executables must be staged as siblings");
    }
    Ok(())
}

pub(super) fn validate_setup_request(request: &WindowsSandboxStandaloneSetupRequest) -> Result<()> {
    super::compatibility::verify_windows_sandbox_standalone_resources(&request.resources)?;
    validate_unicode_absolute_path(&request.state_dir, "state directory")?;
    validate_unicode_absolute_path(&request.command_cwd, "command working directory")?;
    for (label, paths) in [
        ("read root", request.read_roots.as_slice()),
        ("write root", request.write_roots.as_slice()),
        ("deny-read path", request.deny_read_paths.as_slice()),
        ("deny-write path", request.deny_write_paths.as_slice()),
    ] {
        for path in paths {
            validate_unicode_absolute_path(path, label)?;
            if !path.exists() {
                anyhow::bail!("{label} does not exist: {}", path.display());
            }
        }
    }
    if matches!(
        request.network.identity,
        WindowsSandboxStandaloneNetworkIdentity::Online
    ) && (!request.network.proxy_ports.is_empty() || request.network.allow_local_binding)
    {
        anyhow::bail!(
            "proxy ports and local-binding exceptions require the offline sandbox identity"
        );
    }
    if request.network.proxy_ports.contains(&0) {
        anyhow::bail!("proxy port 0 is not a valid firewall exception");
    }
    Ok(())
}

pub(super) fn native_network_identity(
    identity: WindowsSandboxStandaloneNetworkIdentity,
) -> SandboxNetworkIdentity {
    match identity {
        WindowsSandboxStandaloneNetworkIdentity::Offline => SandboxNetworkIdentity::Offline,
        WindowsSandboxStandaloneNetworkIdentity::Online => SandboxNetworkIdentity::Online,
    }
}

/// Inspects helper resources and state without mutating state or requesting UAC.
pub fn windows_sandbox_standalone_setup_status(
    request: &WindowsSandboxStandaloneSetupRequest,
) -> WindowsSandboxStandaloneSetupState {
    if let Err(error) = validate_setup_request(request) {
        return WindowsSandboxStandaloneSetupState::Unavailable {
            reason: error.to_string(),
        };
    }
    let marker = match load_marker(&request.state_dir) {
        Ok(Some(marker)) if marker.version_matches() => marker,
        Ok(_) => {
            return WindowsSandboxStandaloneSetupState::AdministrativeActionRequired {
                reason: "sandbox setup marker is missing or incompatible".to_string(),
            };
        }
        Err(error) => {
            return WindowsSandboxStandaloneSetupState::Unavailable {
                reason: format!("failed to inspect sandbox setup marker: {error}"),
            };
        }
    };
    if !marker.identities_match(STANDALONE_POLICY_NAMESPACE) {
        return WindowsSandboxStandaloneSetupState::AdministrativeActionRequired {
            reason: "sandbox setup marker uses a different identity namespace".to_string(),
        };
    }
    match prepared_sandbox_users_match_policy_namespace(
        &request.state_dir,
        STANDALONE_POLICY_NAMESPACE,
    ) {
        Ok(true) => {}
        Ok(false) => {
            return WindowsSandboxStandaloneSetupState::AdministrativeActionRequired {
                reason: "sandbox credential identities use a different namespace".to_string(),
            };
        }
        Err(error) => {
            return WindowsSandboxStandaloneSetupState::Unavailable {
                reason: format!("failed to inspect sandbox credential identities: {error}"),
            };
        }
    }
    match load_prepared_sandbox_creds(
        native_network_identity(request.network.identity),
        &request.state_dir,
    ) {
        Ok(Some(credentials))
            if credentials.username
                == match request.network.identity {
                    WindowsSandboxStandaloneNetworkIdentity::Offline => {
                        STANDALONE_POLICY_NAMESPACE.offline_username()
                    }
                    WindowsSandboxStandaloneNetworkIdentity::Online => {
                        STANDALONE_POLICY_NAMESPACE.online_username()
                    }
                } => {}
        Ok(_) => {
            return WindowsSandboxStandaloneSetupState::AdministrativeActionRequired {
                reason: "sandbox identities are missing or incompatible".to_string(),
            };
        }
        Err(error) => {
            return WindowsSandboxStandaloneSetupState::Unavailable {
                reason: format!("failed to inspect sandbox identities: {error}"),
            };
        }
    }
    if matches!(
        request.network.identity,
        WindowsSandboxStandaloneNetworkIdentity::Offline
    ) && (marker.proxy_ports != request.network.proxy_ports
        || marker.allow_local_binding != request.network.allow_local_binding)
    {
        return WindowsSandboxStandaloneSetupState::AdministrativeActionRequired {
            reason: "offline firewall settings differ from the requested policy".to_string(),
        };
    }
    WindowsSandboxStandaloneSetupState::Ready
}

/// Inspects local setup state and the installed machine-global firewall rules
/// without mutating either or requesting elevation.
pub fn windows_sandbox_standalone_verified_setup_status(
    request: &WindowsSandboxStandaloneSetupRequest,
) -> WindowsSandboxStandaloneSetupState {
    let local = windows_sandbox_standalone_setup_status(request);
    if local != WindowsSandboxStandaloneSetupState::Ready {
        return local;
    }
    let policy_lease = match crate::policy_lease::acquire_mcp_console_sandbox_policy_lease() {
        Ok(policy_lease) => policy_lease,
        Err(error) => {
            return WindowsSandboxStandaloneSetupState::Unavailable {
                reason: format!("could not inspect the active Windows sandbox policy: {error:#}"),
            };
        }
    };
    match verify_windows_sandbox_standalone_network_with_policy_lease(request, &policy_lease) {
        Ok(()) => WindowsSandboxStandaloneSetupState::Ready,
        Err(error) => WindowsSandboxStandaloneSetupState::AdministrativeActionRequired {
            reason: format!("installed Windows sandbox firewall policy requires refresh: {error:#}"),
        },
    }
}

fn setup_payload(
    request: &WindowsSandboxStandaloneSetupRequest,
    refresh_only: bool,
) -> Result<ElevationPayload> {
    let mut read_roots = request.read_roots.clone();
    if request.read_roots_include_platform_defaults {
        read_roots.extend(
            WINDOWS_PLATFORM_DEFAULT_READ_ROOTS
                .iter()
                .map(PathBuf::from),
        );
    }
    read_roots.push(request.resources.command_runner_executable.clone());
    let mut deny_write_paths = request.deny_write_paths.clone();
    if !deny_write_paths.contains(&request.state_dir) {
        deny_write_paths.push(request.state_dir.clone());
    }
    for resource in [
        &request.resources.setup_executable,
        &request.resources.command_runner_executable,
    ] {
        if !deny_write_paths.contains(resource) {
            deny_write_paths.push(resource.clone());
        }
    }
    let mut username_len = 0;
    unsafe {
        GetUserNameW(std::ptr::null_mut(), &mut username_len);
    }
    if username_len == 0 {
        anyhow::bail!("GetUserNameW did not report the current account name length");
    }
    let mut username = vec![0u16; username_len as usize];
    if unsafe { GetUserNameW(username.as_mut_ptr(), &mut username_len) } == 0 {
        anyhow::bail!("GetUserNameW failed: {}", unsafe { GetLastError() });
    }
    username.truncate(username_len as usize);
    if username.last() == Some(&0) {
        username.pop();
    }
    let real_user =
        String::from_utf16(&username).context("current Windows account name is invalid UTF-16")?;

    Ok(ElevationPayload {
        version: SETUP_VERSION,
        offline_username: STANDALONE_POLICY_NAMESPACE.offline_username().to_string(),
        online_username: STANDALONE_POLICY_NAMESPACE.online_username().to_string(),
        policy_namespace: STANDALONE_POLICY_NAMESPACE,
        codex_home: request.state_dir.clone(),
        command_cwd: request.command_cwd.clone(),
        read_roots,
        write_roots: request.write_roots.clone(),
        deny_read_paths: request.deny_read_paths.clone(),
        deny_write_paths,
        proxy_ports: request.network.proxy_ports.clone(),
        allow_local_binding: request.network.allow_local_binding,
        otel: None,
        real_user,
        mode: SetupMode::Full,
        refresh_only,
    })
}

/// Explicitly prepares identities, ACL state, and firewall state. This is the
/// only standalone operation that may request UAC.
pub fn prepare_windows_sandbox_standalone(
    request: &WindowsSandboxStandaloneSetupRequest,
) -> Result<WindowsSandboxStandaloneSetupOperation> {
    validate_setup_request(request)?;
    std::fs::create_dir_all(&request.state_dir)
        .with_context(|| format!("create state directory {}", request.state_dir.display()))?;
    let policy_lease = crate::policy_lease::acquire_mcp_console_sandbox_policy_lease()
        .context("acquire Windows sandbox policy generation lease")?;
    match windows_sandbox_standalone_setup_status(request) {
        WindowsSandboxStandaloneSetupState::Ready => {
            if verify_windows_sandbox_standalone_network_with_policy_lease(request, &policy_lease)
                .is_ok()
            {
                return Ok(WindowsSandboxStandaloneSetupOperation::AlreadyReady);
            }
        }
        WindowsSandboxStandaloneSetupState::AdministrativeActionRequired { .. } => {}
        WindowsSandboxStandaloneSetupState::Unavailable { reason } => {
            anyhow::bail!("standalone Windows setup is unavailable: {reason}");
        }
    }
    run_setup_exe_at_with_policy_lease(
        &setup_payload(request, /*refresh_only*/ false)?,
        !is_elevated().context("inspect Windows elevation state")?,
        &request.state_dir,
        &request.resources.setup_executable,
        &policy_lease,
    )?;
    match windows_sandbox_standalone_setup_status(request) {
        WindowsSandboxStandaloneSetupState::Ready => {
            verify_windows_sandbox_standalone_network_with_policy_lease(request, &policy_lease)?;
            Ok(WindowsSandboxStandaloneSetupOperation::Prepared)
        }
        state => anyhow::bail!("standalone Windows setup remained unavailable: {state:?}"),
    }
}

/// Refreshes path ACLs using existing prepared identities. This operation never
/// requests UAC and fails when administrative preparation is required.
pub fn refresh_windows_sandbox_standalone(
    request: &WindowsSandboxStandaloneSetupRequest,
) -> Result<WindowsSandboxStandaloneSetupOperation> {
    validate_setup_request(request)?;
    match windows_sandbox_standalone_setup_status(request) {
        WindowsSandboxStandaloneSetupState::Ready => {}
        state => anyhow::bail!("standalone Windows setup is not ready for refresh: {state:?}"),
    }
    let policy_lease = crate::policy_lease::acquire_mcp_console_sandbox_policy_lease()
        .context("acquire Windows sandbox policy generation lease")?;
    verify_windows_sandbox_standalone_network_with_policy_lease(request, &policy_lease)
        .context("Windows sandbox firewall policy requires administrative preparation")?;
    run_setup_exe_at_with_policy_lease(
        &setup_payload(request, /*refresh_only*/ true)?,
        /*needs_elevation*/ false,
        &request.state_dir,
        &request.resources.setup_executable,
        &policy_lease,
    )?;
    Ok(WindowsSandboxStandaloneSetupOperation::Refreshed)
}

pub(super) fn refresh_windows_sandbox_standalone_with_policy_lease(
    request: &WindowsSandboxStandaloneSetupRequest,
    policy_lease: &crate::policy_lease::McpConsoleSandboxPolicyLease,
) -> Result<WindowsSandboxStandaloneSetupOperation> {
    validate_setup_request(request)?;
    match windows_sandbox_standalone_setup_status(request) {
        WindowsSandboxStandaloneSetupState::Ready => {}
        state => anyhow::bail!("standalone Windows setup is not ready for refresh: {state:?}"),
    }
    run_setup_exe_at_with_policy_lease(
        &setup_payload(request, /*refresh_only*/ true)?,
        /*needs_elevation*/ false,
        &request.state_dir,
        &request.resources.setup_executable,
        policy_lease,
    )?;
    Ok(WindowsSandboxStandaloneSetupOperation::Refreshed)
}

pub(super) fn verify_windows_sandbox_standalone_network_with_policy_lease(
    request: &WindowsSandboxStandaloneSetupRequest,
    policy_lease: &crate::policy_lease::McpConsoleSandboxPolicyLease,
) -> Result<()> {
    validate_setup_request(request)?;
    run_setup_network_verification_exe_at_with_policy_lease(
        &setup_payload(request, /*refresh_only*/ true)?,
        &request.resources.setup_executable,
        policy_lease,
    )
}
