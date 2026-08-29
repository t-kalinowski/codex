use crate::MAX_FRAME_SIZE;
use crate::PROTOCOL_VERSION;
use crate::RELEASE_TAG;
use crate::protocol::Capabilities;
use crate::protocol::CompanionResource;
use crate::protocol::FileSystemCapabilities;
use crate::protocol::LifecycleCapabilities;
use crate::protocol::NativeBackend;
use crate::protocol::NetworkCapabilities;
use crate::protocol::SetupState;
use crate::protocol::SetupStatus;
use crate::protocol::StreamCapabilities;
use crate::protocol::TerminalCapabilities;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendAvailabilityError {
    Companion(String),
    Backend(String),
}

impl std::fmt::Display for BackendAvailabilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Companion(message) | Self::Backend(message) => formatter.write_str(message),
        }
    }
}

pub fn native_backend() -> NativeBackend {
    if cfg!(target_os = "macos") {
        NativeBackend::MacosSeatbelt
    } else if cfg!(target_os = "linux") {
        NativeBackend::LinuxBubblewrap
    } else {
        NativeBackend::Unsupported
    }
}

pub fn capabilities(state_dir: &Path, source_revision: &str) -> Capabilities {
    let availability = backend_availability(state_dir);
    let supported = availability.is_ok() && cfg!(any(target_os = "macos", target_os = "linux"));
    Capabilities {
        protocol_version: PROTOCOL_VERSION,
        maximum_frame_size: MAX_FRAME_SIZE,
        runner_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_source_revision: source_revision.to_string(),
        codex_release_tag: Some(RELEASE_TAG.to_string()),
        operating_system: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        backend: native_backend(),
        filesystem: FileSystemCapabilities {
            platform_minimal: supported,
            host_read_only: supported,
            read_rules: supported,
            write_rules: supported,
            deny_read_rules: supported,
            deny_write_rules: supported,
            missing_path_error: supported,
            missing_path_ignore: supported,
            precedence: "more_specific_then_deny_then_write_then_read".to_string(),
            state_directory_protected: supported,
            unicode_policy_paths_only: true,
        },
        network: NetworkCapabilities {
            denied: supported,
            unrestricted: supported,
            managed_proxy: supported,
            full_access: supported,
            limited_access: supported,
            http: supported,
            socks: supported,
            socks_udp: false,
            upstream_proxy: supported,
            domain_allow_patterns: supported,
            domain_deny_patterns: supported,
            local_binding_policy: supported && cfg!(target_os = "macos"),
            loopback_policy: supported && cfg!(target_os = "macos"),
            unix_socket_policy: supported && cfg!(target_os = "macos"),
            unix_socket_allow_rules: supported && cfg!(target_os = "macos"),
            unix_socket_deny_rules: false,
            explicit_local_ports: false,
            managed_ca: false,
            direct_egress_confinement: supported,
            non_loopback_listeners: false,
        },
        streams: StreamCapabilities {
            inherited: supported,
            passed_handle: supported,
            null: supported,
            independent: supported,
            byte_transparent: supported,
            application_bytes_on_control_channel: false,
        },
        terminal: TerminalCapabilities {
            inherited_terminal: supported,
            caller_supplied_pty: supported,
            controlling_terminal_reopen: false,
            pty_creation_inside_sandbox: supported,
            host_device_isolation: false,
        },
        lifecycle: LifecycleCapabilities {
            interrupt: supported && cfg!(target_os = "macos"),
            graceful_termination: supported && cfg!(target_os = "macos"),
            forced_termination: supported,
            root_exit_observation: supported,
            process_tree_supervision: supported,
            full_tree_retirement: supported,
            cleanup_after_root_exit: supported,
            control_loss_retires_target: supported,
        },
        required_companions: companion_resources(),
        setup: setup_status_from_backend_availability(&availability),
    }
}

fn companion_resources() -> Vec<CompanionResource> {
    if cfg!(target_os = "linux") {
        vec![CompanionResource {
            name: "bubblewrap".to_string(),
            relative_path: "codex-resources/bwrap".to_string(),
            required: true,
        }]
    } else {
        Vec::new()
    }
}

pub fn setup_status(state_dir: &Path) -> SetupStatus {
    setup_status_from_backend_availability(&backend_availability(state_dir))
}

fn setup_status_from_backend_availability(
    availability: &Result<(), BackendAvailabilityError>,
) -> SetupStatus {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return SetupStatus {
            state: SetupState::Unsupported,
            detail: Some("no exported native sandbox backend exists for this platform".to_string()),
        };
    }
    match availability {
        Ok(()) => SetupStatus {
            state: SetupState::NotRequired,
            detail: None,
        },
        Err(error) => SetupStatus {
            state: SetupState::Unavailable,
            detail: Some(error.to_string()),
        },
    }
}

pub fn backend_availability(state_dir: &Path) -> Result<(), BackendAvailabilityError> {
    #[cfg(target_os = "macos")]
    {
        let _ = state_dir;
        let sandbox_exec = Path::new(codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        if !sandbox_exec.is_file() {
            return Err(BackendAvailabilityError::Backend(format!(
                "required Seatbelt executable is unavailable: {}",
                sandbox_exec.display()
            )));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let _ = state_dir;
        codex_linux_sandbox::verify_mcp_console_bundled_bwrap()
            .map_err(BackendAvailabilityError::Companion)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = state_dir;
        Err(BackendAvailabilityError::Backend(
            "no exported native sandbox backend exists for this platform".to_string(),
        ))
    }
}
