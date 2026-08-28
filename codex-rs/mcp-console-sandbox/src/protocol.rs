use serde::Deserialize;
use serde::Serialize;

mod capabilities;

pub use capabilities::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    Discover {
        id: u64,
        protocol_version: u32,
    },
    SetupStatus {
        id: u64,
        protocol_version: u32,
        setup: Box<SetupRequest>,
    },
    Setup {
        id: u64,
        protocol_version: u32,
        operation: SetupOperation,
        setup: Box<SetupRequest>,
    },
    Launch {
        id: u64,
        protocol_version: u32,
        launch: Box<LaunchRequest>,
    },
    Status {
        id: u64,
        protocol_version: u32,
    },
    Interrupt {
        id: u64,
        protocol_version: u32,
    },
    Terminate {
        id: u64,
        protocol_version: u32,
        deadlines: StopDeadlines,
    },
    Wait {
        id: u64,
        protocol_version: u32,
        retirement_timeout_ms: u64,
    },
}

impl ClientRequest {
    pub fn id(&self) -> u64 {
        match self {
            Self::Discover { id, .. }
            | Self::SetupStatus { id, .. }
            | Self::Setup { id, .. }
            | Self::Launch { id, .. }
            | Self::Status { id, .. }
            | Self::Interrupt { id, .. }
            | Self::Terminate { id, .. }
            | Self::Wait { id, .. } => *id,
        }
    }

    pub fn protocol_version(&self) -> u32 {
        match self {
            Self::Discover {
                protocol_version, ..
            }
            | Self::SetupStatus {
                protocol_version, ..
            }
            | Self::Setup {
                protocol_version, ..
            }
            | Self::Launch {
                protocol_version, ..
            }
            | Self::Status {
                protocol_version, ..
            }
            | Self::Interrupt {
                protocol_version, ..
            }
            | Self::Terminate {
                protocol_version, ..
            }
            | Self::Wait {
                protocol_version, ..
            } => *protocol_version,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SetupOperation {
    Prepare,
    Refresh,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SetupRequest {
    pub working_directory: String,
    pub policy_base_directory: String,
    pub filesystem: FileSystemPolicy,
    pub network: NetworkPolicy,
    #[serde(default)]
    pub platform_extensions: PlatformExtensions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub working_directory: String,
    pub policy_base_directory: String,
    pub filesystem: FileSystemPolicy,
    pub network: NetworkPolicy,
    pub streams: StandardStreams,
    pub terminal: TerminalPolicy,
    pub lifecycle: LifecyclePolicy,
    #[serde(default)]
    pub platform_extensions: PlatformExtensions,
}

impl From<&LaunchRequest> for SetupRequest {
    fn from(launch: &LaunchRequest) -> Self {
        Self {
            working_directory: launch.working_directory.clone(),
            policy_base_directory: launch.policy_base_directory.clone(),
            filesystem: launch.filesystem.clone(),
            network: launch.network.clone(),
            platform_extensions: launch.platform_extensions.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileSystemPolicy {
    pub base: FileSystemBase,
    #[serde(default)]
    pub rules: Vec<PathRule>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FileSystemBase {
    PlatformMinimal,
    HostReadOnly,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub path: String,
    pub access: PathAccess,
    pub missing: MissingPathBehavior,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PathAccess {
    Read,
    Write,
    Deny,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MissingPathBehavior {
    Error,
    Ignore,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkPolicy {
    Denied,
    Unrestricted,
    ManagedProxy {
        access: ManagedNetworkAccess,
        #[serde(default)]
        allowed_domains: Vec<String>,
        #[serde(default)]
        denied_domains: Vec<String>,
        socks: bool,
        socks_udp: bool,
        upstream_proxy: bool,
        local_binding: bool,
        loopback: LoopbackPolicy,
        #[serde(default)]
        local_ports: Vec<u16>,
        #[serde(default)]
        unix_sockets: Vec<UnixSocketRule>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedNetworkAccess {
    Full,
    Limited,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LoopbackPolicy {
    ProxyOnly,
    Allow,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnixSocketRule {
    pub path: String,
    pub access: UnixSocketAccess,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UnixSocketAccess {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StandardStreams {
    pub stdin: StreamSpec,
    pub stdout: StreamSpec,
    pub stderr: StreamSpec,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamSpec {
    Inherited,
    Null,
    PassedHandle { handle: u64 },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalPolicy {
    Preserve,
    IsolateHostDevices,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LifecyclePolicy {
    pub kind: LaunchKind,
    pub root_exit_grace_ms: u64,
    pub terminate_grace_ms: u64,
    pub force_timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchKind {
    Command,
    Service,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StopDeadlines {
    pub graceful_ms: u64,
    pub force_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlatformExtensions {
    pub macos: Option<MacosExtensions>,
    pub linux: Option<LinuxExtensions>,
    pub windows: Option<WindowsExtensions>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MacosExtensions {}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LinuxExtensions {}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WindowsExtensions {
    #[serde(default)]
    pub private_desktop: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerResponse {
    Capabilities {
        id: u64,
        capabilities: Capabilities,
    },
    SetupStatus {
        id: u64,
        setup: SetupStatus,
    },
    SetupCompleted {
        id: u64,
        operation: SetupCompletedOperation,
    },
    LaunchAccepted {
        id: u64,
        backend: NativeBackend,
        root_process_id: Option<u32>,
    },
    Status {
        id: u64,
        status: RunnerStatus,
    },
    Acknowledged {
        id: u64,
        operation: AcknowledgedOperation,
    },
    Final {
        id: u64,
        outcome: FinalOutcome,
    },
    Error {
        id: Option<u64>,
        error: ProtocolError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunnerStatus {
    pub phase: RunnerPhase,
    pub target: Option<TargetOutcome>,
    pub retirement: Option<RetirementOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPhase {
    Idle,
    Running,
    RootExited,
    Retiring,
    Retired,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcknowledgedOperation {
    Interrupt,
    Terminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupCompletedOperation {
    AlreadyReady,
    Prepared,
    Refreshed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FinalOutcome {
    pub target: Option<TargetOutcome>,
    pub retirement: RetirementOutcome,
    pub infrastructure: InfrastructureOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TargetOutcome {
    pub kind: TargetOutcomeKind,
    pub code: Option<i64>,
    pub signal: Option<i32>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetOutcomeKind {
    Exited,
    Signaled,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RetirementOutcome {
    pub complete: bool,
    pub forced: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InfrastructureOutcome {
    pub error: Option<String>,
    pub cleanup_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub phase: ErrorPhase,
    pub message: String,
    pub target_started: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MalformedFrame,
    MalformedJson,
    VersionMismatch,
    InvalidState,
    InvalidRequest,
    InvalidPath,
    UnsupportedPlatform,
    UnsupportedPolicy,
    BackendUnavailable,
    CompanionMissing,
    SetupFailed,
    LaunchFailed,
    ControlFailed,
    CleanupFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPhase {
    Protocol,
    Discovery,
    Setup,
    Validation,
    ProxyStartup,
    SandboxPreparation,
    Launch,
    Running,
    Retirement,
}
