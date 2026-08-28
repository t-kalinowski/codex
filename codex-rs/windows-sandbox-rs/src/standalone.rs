//! Standalone Windows sandbox boundary for private embedding applications.
//!
//! This module intentionally exposes Windows-native values and handles. It
//! never discovers helpers through `PATH`, never initializes Codex
//! configuration, and never starts a target unless the explicitly supplied
//! setup state is current.

mod client;
mod compatibility;
mod helper;
mod setup;
mod wire;

pub use client::WindowsSandboxStandaloneProcess;
pub use client::spawn_windows_sandbox_standalone;
#[doc(hidden)]
pub use compatibility::WindowsSandboxStandaloneHelperKind;
pub use compatibility::verify_windows_sandbox_standalone_resources;
#[doc(hidden)]
pub use compatibility::windows_sandbox_standalone_helper_compatibility_response;
pub use helper::run_windows_sandbox_standalone_helper;
#[doc(hidden)]
pub use setup::WINDOWS_SANDBOX_STANDALONE_VERIFY_NETWORK_SWITCH;
pub use setup::WindowsSandboxStandaloneFilesystemOverrides;
pub use setup::WindowsSandboxStandaloneNetworkIdentity;
pub use setup::WindowsSandboxStandaloneNetworkSetup;
pub use setup::WindowsSandboxStandalonePolicyRequest;
pub use setup::WindowsSandboxStandaloneResources;
pub use setup::WindowsSandboxStandaloneSetupOperation;
pub use setup::WindowsSandboxStandaloneSetupRequest;
pub use setup::WindowsSandboxStandaloneSetupState;
pub use setup::is_windows_sandbox_standalone_setup_only_environment_variable;
pub use setup::prepare_windows_sandbox_standalone;
pub use setup::refresh_windows_sandbox_standalone;
pub use setup::windows_sandbox_standalone_setup_request_from_permission_profile;
pub use setup::windows_sandbox_standalone_setup_status;
pub use setup::windows_sandbox_standalone_verified_setup_status;

use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::BorrowedHandle;
use std::path::PathBuf;
use std::time::Duration;

const STANDALONE_HELPER_SWITCH: &str = "--standalone-sandbox-runner";

#[doc(hidden)]
pub fn is_windows_sandbox_standalone_helper_invocation(arguments: &[OsString]) -> bool {
    arguments.len() == 3
        && arguments[0] == STANDALONE_HELPER_SWITCH
        && arguments[1]
            .to_str()
            .is_some_and(|argument| argument.starts_with("--pipe-in=") && argument.len() > 10)
        && arguments[2]
            .to_str()
            .is_some_and(|argument| argument.starts_with("--pipe-out=") && argument.len() > 11)
}

/// Native command and complete target environment for a standalone launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsSandboxStandaloneCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

impl WindowsSandboxStandaloneCommand {
    pub fn validate(&self) -> Result<()> {
        if !setup::is_absolute_local_disk_path(&self.program) {
            anyhow::bail!("target program must be an absolute local-disk Windows path");
        }
        if !self.program.is_file() {
            anyhow::bail!("target program is missing: {}", self.program.display());
        }
        if !setup::is_absolute_local_disk_path(&self.cwd) || !self.cwd.is_dir() {
            anyhow::bail!("target working directory must be an existing absolute local directory");
        }
        validate_native_os_str(self.program.as_os_str(), "target program")?;
        validate_native_os_str(self.cwd.as_os_str(), "target working directory")?;
        for argument in &self.args {
            validate_native_os_str(argument, "target argument")?;
        }
        let mut environment_names = BTreeSet::new();
        for (key, value) in &self.environment {
            validate_native_os_str(key, "environment name")?;
            validate_native_os_str(value, "environment value")?;
            let key = key.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "environment name is not valid Unicode; protocol version 1 rejects it"
                )
            })?;
            let units = key.encode_utf16().collect::<Vec<_>>();
            if units.is_empty() || units.iter().skip(1).any(|unit| *unit == b'=' as u16) {
                anyhow::bail!("invalid Windows environment variable name");
            }
            if !environment_names.insert(key.to_uppercase()) {
                anyhow::bail!("duplicate Windows environment variable name");
            }
        }
        Ok(())
    }
}

pub(super) fn validate_native_os_str(value: &OsStr, label: &str) -> Result<()> {
    if value.encode_wide().any(|unit| unit == 0) {
        anyhow::bail!("{label} contains an embedded NUL");
    }
    Ok(())
}

/// Direct standard-stream endpoint for a standalone launch.
pub enum WindowsSandboxStandaloneStream<'a> {
    /// Duplicate the runner's corresponding standard handle into the helper.
    Inherited,
    /// Duplicate a caller-owned handle into the helper. The caller retains its
    /// original handle and owns its lifetime.
    Passed(BorrowedHandle<'a>),
    /// Connect the target stream directly to the native null device.
    Null,
}

/// Independently selected direct endpoints for target stdin, stdout, and stderr.
pub struct WindowsSandboxStandaloneStdio<'a> {
    pub stdin: WindowsSandboxStandaloneStream<'a>,
    pub stdout: WindowsSandboxStandaloneStream<'a>,
    pub stderr: WindowsSandboxStandaloneStream<'a>,
}

/// Complete elevated-helper launch request.
pub struct WindowsSandboxStandaloneLaunchRequest<'a> {
    pub setup: WindowsSandboxStandaloneSetupRequest,
    pub command: WindowsSandboxStandaloneCommand,
    pub stdio: WindowsSandboxStandaloneStdio<'a>,
    /// Optional restricting SID owned by Codex's managed network proxy.
    pub network_proxy_restricting_sid: Option<String>,
    pub use_private_desktop: bool,
    /// Time descendants may retire naturally after the root exits.
    pub descendant_grace: Duration,
    /// Time allowed for the job to become empty after forced termination.
    pub force_stop_timeout: Duration,
}

/// Root-process result, observed independently from descendant retirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsSandboxStandaloneRootOutcome {
    Exited { code: u32 },
    Unknown { error: String },
}

/// Full non-breakaway Job Object retirement result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsSandboxStandaloneRetirementOutcome {
    pub complete: bool,
    pub forced: bool,
    pub error: Option<String>,
}

/// Final standalone launch result. Infrastructure failure after launch is kept
/// separate from the target and retirement results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsSandboxStandaloneOutcome {
    pub target: WindowsSandboxStandaloneRootOutcome,
    pub retirement: WindowsSandboxStandaloneRetirementOutcome,
    pub infrastructure_error: Option<String>,
}
