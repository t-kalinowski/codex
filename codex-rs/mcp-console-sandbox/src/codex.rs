// All upstream imports live here; bootstrap types remain patch-local.
#[cfg(test)]
pub use codex_utils_cargo_bin::cargo_bin;

#[cfg(not(test))]
pub use upstream::*;

#[cfg(not(test))]
mod upstream {
    #[cfg(target_os = "linux")]
    pub use codex_linux_sandbox::run_main as linux_sandbox_main;
    pub use codex_network_proxy::NetworkProxy;
    pub use codex_network_proxy::NetworkProxyState;
    pub use codex_network_proxy::RemoteNetworkProxyConfig;
    pub use codex_network_proxy::RemoteNetworkProxyLaunchConfig;
    pub use codex_protocol::config_types::WindowsSandboxLevel;
    pub use codex_protocol::models::PermissionProfile;
    pub use codex_protocol::permissions::FileSystemSandboxPolicy;
    pub use codex_protocol::permissions::NetworkSandboxPolicy;
    pub use codex_protocol::permissions::RawFileSystemSandboxPolicy;
    pub use codex_sandboxing::SandboxCommand;
    pub use codex_sandboxing::SandboxManager;
    pub use codex_sandboxing::SandboxTransformRequest;
    pub use codex_sandboxing::SandboxType;
    pub use codex_sandboxing::SandboxablePreference;
    pub use codex_utils_absolute_path::AbsolutePathBuf;
    pub use codex_utils_path_uri::PathUri;
}
