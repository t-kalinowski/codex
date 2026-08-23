use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// Selects which native sandbox implementation the runtime should use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BackendPreference {
    PlatformDefault,
}

/// Native sandbox implementation selected for a runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SandboxBackend {
    MacosSeatbelt,
    LinuxBubblewrap,
    LinuxLandlock,
    WindowsRestrictedToken,
    WindowsElevated,
}

/// Features enforced by the selected native backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SandboxCapabilities {
    pub backend: SandboxBackend,
    pub minimal_read_policy: bool,
    pub denied_read_paths: bool,
    pub denied_write_paths: bool,
    pub network_denial: bool,
    pub network_unrestricted: bool,
    pub interrupt: bool,
    /// Whether termination remains authoritative for descendants after the root exits.
    pub process_tree_termination: bool,
}

/// Linux helper executable used to enter the Codex Linux sandbox.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LinuxHelper {
    /// Use a separately installed or vendored helper executable.
    External(PathBuf),
    /// Re-execute the embedding application through a private helper alias.
    CurrentExecutable,
}

/// Linux-specific runtime choices.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxOptions {
    pub helper: LinuxHelper,
}

#[cfg(target_os = "linux")]
impl Default for LinuxOptions {
    fn default() -> Self {
        Self {
            helper: LinuxHelper::CurrentExecutable,
        }
    }
}

/// Windows-specific runtime choices.
#[cfg(target_os = "windows")]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct WindowsOptions {}

/// Runtime configuration owned entirely by the embedding application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRuntimeConfig {
    /// Application-owned persistent or runtime state directory.
    ///
    /// Keep it outside paths writable by sandboxed children.
    pub state_dir: PathBuf,
    /// Select the native platform sandbox and fail if it is unavailable.
    pub backend: BackendPreference,
    #[cfg(target_os = "linux")]
    pub linux: LinuxOptions,
    #[cfg(target_os = "windows")]
    pub windows: WindowsOptions,
}

impl SandboxRuntimeConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            backend: BackendPreference::PlatformDefault,
            #[cfg(target_os = "linux")]
            linux: LinuxOptions::default(),
            #[cfg(target_os = "windows")]
            windows: WindowsOptions::default(),
        }
    }
}

/// Complete process launch description supplied by the embedding application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    /// Absolute executable path. The facade does not search `PATH`.
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// Complete environment for the child; no parent or Codex values are merged.
    pub env: BTreeMap<OsString, OsString>,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<OsString>,
        cwd: impl Into<PathBuf>,
        env: BTreeMap<OsString, OsString>,
    ) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
}

/// Filesystem access baseline applied before explicit path rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FileSystemBase {
    /// Allow only roots required to launch ordinary programs, plus explicit rules.
    PlatformMinimal,
    /// Allow host filesystem reads while limiting writes and applying deny rules.
    HostReadOnly,
}

/// Access granted at a path and its descendants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAccess {
    Read,
    Write,
    Deny,
}

/// Treatment of a rule whose path is absent during sandbox preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingPathBehavior {
    Error,
    Ignore,
}

/// One absolute filesystem policy rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRule {
    pub path: PathBuf,
    pub access: PathAccess,
    pub missing: MissingPathBehavior,
}

impl PathRule {
    pub fn new(path: impl Into<PathBuf>, access: PathAccess) -> Self {
        Self {
            path: path.into(),
            access,
            missing: MissingPathBehavior::Error,
        }
    }

    pub fn ignore_if_missing(mut self) -> Self {
        self.missing = MissingPathBehavior::Ignore;
        self
    }
}

/// Filesystem policy independent of any Codex configuration type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSystemPolicy {
    pub base: FileSystemBase,
    pub rules: Vec<PathRule>,
}

/// Direct network access policy for the target process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NetworkPolicy {
    Denied,
    Unrestricted,
}

/// Filesystem and network policy for one child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxPolicy {
    pub filesystem: FileSystemPolicy,
    pub network: NetworkPolicy,
}

impl SandboxPolicy {
    pub fn platform_minimal() -> Self {
        Self {
            filesystem: FileSystemPolicy {
                base: FileSystemBase::PlatformMinimal,
                rules: Vec::new(),
            },
            network: NetworkPolicy::Denied,
        }
    }

    pub fn host_read_only() -> Self {
        Self {
            filesystem: FileSystemPolicy {
                base: FileSystemBase::HostReadOnly,
                rules: Vec::new(),
            },
            network: NetworkPolicy::Denied,
        }
    }

    pub fn rule(mut self, rule: PathRule) -> Self {
        self.filesystem.rules.push(rule);
        self
    }

    pub fn read_only(self, path: impl Into<PathBuf>) -> Self {
        self.rule(PathRule::new(path, PathAccess::Read))
    }

    pub fn read_write(self, path: impl Into<PathBuf>) -> Self {
        self.rule(PathRule::new(path, PathAccess::Write))
    }

    pub fn deny(self, path: impl Into<PathBuf>) -> Self {
        self.rule(PathRule::new(path, PathAccess::Deny))
    }

    pub fn network_denied(mut self) -> Self {
        self.network = NetworkPolicy::Denied;
        self
    }

    pub fn network_unrestricted(mut self) -> Self {
        self.network = NetworkPolicy::Unrestricted;
        self
    }
}

/// One sandboxed process launch request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRequest {
    pub command: CommandSpec,
    pub policy: SandboxPolicy,
    /// Whether the child starts with writable stdin.
    pub stdin_open: bool,
}

impl SandboxRequest {
    pub fn new(command: CommandSpec, policy: SandboxPolicy) -> Self {
        Self {
            command,
            policy,
            stdin_open: false,
        }
    }

    pub fn stdin_open(mut self) -> Self {
        self.stdin_open = true;
        self
    }

    pub fn stdin_closed(mut self) -> Self {
        self.stdin_open = false;
        self
    }
}
