use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub protocol_version: u32,
    pub maximum_frame_size: usize,
    pub runner_version: String,
    pub codex_source_revision: String,
    pub codex_release_tag: Option<String>,
    pub operating_system: String,
    pub architecture: String,
    pub backend: NativeBackend,
    pub filesystem: FileSystemCapabilities,
    pub network: NetworkCapabilities,
    pub streams: StreamCapabilities,
    pub terminal: TerminalCapabilities,
    pub lifecycle: LifecycleCapabilities,
    pub required_companions: Vec<CompanionResource>,
    pub setup: SetupStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeBackend {
    MacosSeatbelt,
    LinuxBubblewrap,
    WindowsRestrictedToken,
    WindowsElevated,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileSystemCapabilities {
    pub platform_minimal: bool,
    pub host_read_only: bool,
    pub read_rules: bool,
    pub write_rules: bool,
    pub deny_read_rules: bool,
    pub deny_write_rules: bool,
    pub missing_path_error: bool,
    pub missing_path_ignore: bool,
    pub precedence: String,
    pub state_directory_protected: bool,
    pub unicode_policy_paths_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NetworkCapabilities {
    pub denied: bool,
    pub unrestricted: bool,
    pub managed_proxy: bool,
    pub full_access: bool,
    pub limited_access: bool,
    pub http: bool,
    pub socks: bool,
    pub socks_udp: bool,
    pub upstream_proxy: bool,
    pub domain_allow_patterns: bool,
    pub domain_deny_patterns: bool,
    pub local_binding_policy: bool,
    pub loopback_policy: bool,
    pub unix_socket_policy: bool,
    pub unix_socket_allow_rules: bool,
    pub unix_socket_deny_rules: bool,
    pub explicit_local_ports: bool,
    pub managed_ca: bool,
    pub direct_egress_confinement: bool,
    pub non_loopback_listeners: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StreamCapabilities {
    pub inherited: bool,
    pub passed_handle: bool,
    pub null: bool,
    pub independent: bool,
    pub byte_transparent: bool,
    pub application_bytes_on_control_channel: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TerminalCapabilities {
    pub inherited_terminal: bool,
    pub caller_supplied_pty: bool,
    pub controlling_terminal_reopen: bool,
    pub pty_creation_inside_sandbox: bool,
    pub host_device_isolation: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LifecycleCapabilities {
    pub interrupt: bool,
    pub graceful_termination: bool,
    pub forced_termination: bool,
    pub root_exit_observation: bool,
    pub process_tree_supervision: bool,
    pub full_tree_retirement: bool,
    pub cleanup_after_root_exit: bool,
    pub control_loss_retires_target: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompanionResource {
    pub name: String,
    pub relative_path: String,
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SetupStatus {
    pub state: SetupState,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupState {
    NotRequired,
    Ready,
    RefreshRequired,
    Unavailable,
    Unsupported,
    AdministrativeActionRequired,
    Failed,
}
