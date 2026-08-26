//! Platform-agnostic embedding facade for Codex sandbox implementations.

mod error;
mod policy;
mod process;
mod runtime;
mod types;

pub use error::SandboxError;
pub use error::SandboxFeature;
pub use process::SandboxExitStatus;
pub use process::SandboxedChild;
pub use process::SandboxedOutput;
pub use process::SandboxedProcess;
pub use process::SandboxedStdin;
pub use runtime::SandboxRuntime;
pub use runtime::dispatch_embedded_helper;
pub use runtime::terminate_current_process_group_members;
pub use types::BackendPreference;
pub use types::CommandSpec;
pub use types::FileSystemBase;
pub use types::FileSystemPolicy;
#[cfg(target_os = "linux")]
pub use types::LinuxHelper;
#[cfg(target_os = "linux")]
pub use types::LinuxOptions;
pub use types::MissingPathBehavior;
pub use types::NetworkPolicy;
pub use types::PathAccess;
pub use types::PathRule;
pub use types::SandboxBackend;
pub use types::SandboxCapabilities;
pub use types::SandboxLifetime;
pub use types::SandboxPolicy;
pub use types::SandboxRequest;
pub use types::SandboxRuntimeConfig;
pub use types::SandboxStdio;
pub use types::SandboxStdioMode;
pub use types::TerminalPolicy;
#[cfg(target_os = "windows")]
pub use types::WindowsOptions;

/// Version of the public embedding contract exported by this crate.
pub const SANDBOX_API_VERSION: u32 = 2;
