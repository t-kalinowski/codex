#[cfg(all(feature = "full", target_os = "linux"))]
mod bwrap;
#[cfg(feature = "full")]
mod denial;
#[cfg(feature = "full")]
pub mod landlock;
#[cfg(feature = "full")]
mod manager;
#[cfg(feature = "full")]
pub mod policy_transforms;
#[cfg(all(feature = "full", target_os = "macos"))]
pub mod seatbelt;
#[cfg(all(feature = "seatbelt-profile", target_os = "macos"))]
pub mod seatbelt_profile;
#[cfg(feature = "full")]
mod spawn;
#[cfg(feature = "full")]
mod violation;
#[cfg(feature = "full")]
mod windows;

#[cfg(all(feature = "full", target_os = "linux"))]
pub use bwrap::find_system_bwrap_in_path;
#[cfg(all(feature = "full", target_os = "linux"))]
pub use bwrap::system_bwrap_warning;
#[cfg(feature = "full")]
pub use codex_windows_sandbox::WindowsSandboxProxySettingsMode;
#[cfg(feature = "full")]
pub use denial::is_likely_executor_managed_sandbox_denied;
#[cfg(feature = "full")]
pub use denial::is_likely_sandbox_denied;
#[cfg(feature = "full")]
pub use manager::SandboxCommand;
#[cfg(feature = "full")]
pub use manager::SandboxDirectSpawnTransformRequest;
#[cfg(feature = "full")]
pub use manager::SandboxExecRequest;
#[cfg(feature = "full")]
pub use manager::SandboxManager;
#[cfg(feature = "full")]
pub use manager::SandboxTransformError;
#[cfg(feature = "full")]
pub use manager::SandboxTransformRequest;
#[cfg(feature = "full")]
pub use manager::SandboxType;
#[cfg(feature = "full")]
pub use manager::SandboxablePreference;
#[cfg(feature = "full")]
pub use manager::compatibility_sandbox_policy_for_permission_profile;
#[cfg(feature = "full")]
pub use manager::get_platform_sandbox;
#[cfg(feature = "full")]
pub use manager::with_managed_mitm_ca_readable_root;
#[cfg(feature = "full")]
pub use spawn::SpawnRequest;
#[cfg(feature = "full")]
pub use spawn::WindowsSandboxSpawnRequest;
#[cfg(feature = "full")]
pub use spawn::spawn_process;
#[cfg(feature = "full")]
pub use violation::FileSystemSandboxViolation;
#[cfg(feature = "full")]
pub use violation::FileSystemSandboxViolationReason;
#[cfg(feature = "full")]
pub use violation::NetworkSandboxViolation;
#[cfg(feature = "full")]
pub use violation::SandboxViolationBackend;
#[cfg(feature = "full")]
pub use violation::SandboxViolationEvent;
#[cfg(feature = "full")]
pub use violation::record_filesystem_sandbox_violation;
#[cfg(feature = "full")]
pub use violation::record_network_sandbox_violation;
#[cfg(feature = "full")]
pub use violation::record_sandbox_violation;
#[cfg(feature = "full")]
pub use windows::WindowsSandboxFilesystemOverrides;
#[cfg(feature = "full")]
pub use windows::permission_profile_supports_windows_restricted_token_sandbox;
#[cfg(feature = "full")]
pub use windows::resolve_windows_elevated_filesystem_overrides;
#[cfg(feature = "full")]
pub use windows::resolve_windows_restricted_token_filesystem_overrides;
#[cfg(feature = "full")]
pub use windows::unsupported_windows_restricted_token_sandbox_reason;
#[cfg(feature = "full")]
pub use windows::windows_sandbox_uses_elevated_backend;

#[cfg(feature = "full")]
use codex_protocol::error::CodexErr;

#[cfg(all(feature = "full", not(target_os = "linux")))]
pub fn system_bwrap_warning(
    _permission_profile: &codex_protocol::models::PermissionProfile,
) -> Option<String> {
    None
}

#[cfg(feature = "full")]
impl From<SandboxTransformError> for CodexErr {
    fn from(err: SandboxTransformError) -> Self {
        match err {
            error @ SandboxTransformError::InvalidCommandCwd { .. }
            | error @ SandboxTransformError::InvalidSandboxPolicyCwd { .. } => {
                CodexErr::InvalidRequest(error.to_string())
            }
            SandboxTransformError::MissingLinuxSandboxExecutable => {
                CodexErr::LandlockSandboxExecutableNotProvided
            }
            SandboxTransformError::EnvironmentNetworkProxy(message) => {
                CodexErr::UnsupportedOperation(message)
            }
            #[cfg(target_os = "linux")]
            SandboxTransformError::Wsl1UnsupportedForBubblewrap => {
                CodexErr::UnsupportedOperation(crate::bwrap::WSL1_BWRAP_WARNING.to_string())
            }
            #[cfg(not(target_os = "macos"))]
            SandboxTransformError::SeatbeltUnavailable => CodexErr::UnsupportedOperation(
                "seatbelt sandbox is only available on macOS".to_string(),
            ),
            #[cfg(target_os = "windows")]
            SandboxTransformError::WindowsSandboxPreparation(message) => {
                CodexErr::UnsupportedOperation(message)
            }
        }
    }
}
