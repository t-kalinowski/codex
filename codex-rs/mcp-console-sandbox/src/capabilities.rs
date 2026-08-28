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

pub fn native_backend() -> NativeBackend {
    if cfg!(target_os = "macos") {
        NativeBackend::MacosSeatbelt
    } else if cfg!(target_os = "linux") {
        NativeBackend::LinuxBubblewrap
    } else if cfg!(target_os = "windows") {
        NativeBackend::WindowsElevated
    } else {
        NativeBackend::Unsupported
    }
}

pub fn capabilities(state_dir: &Path, source_revision: &str) -> Capabilities {
    let backend_availability = backend_availability(state_dir);
    let setup = setup_status_from_backend_availability(&backend_availability);
    let supported_host = cfg!(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )) && backend_availability.is_ok();
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
            platform_minimal: supported_host && !cfg!(target_os = "windows"),
            host_read_only: supported_host,
            read_rules: supported_host,
            write_rules: supported_host,
            deny_read_rules: supported_host,
            deny_write_rules: supported_host,
            missing_path_error: supported_host,
            missing_path_ignore: supported_host,
            precedence: "more_specific_then_deny_then_write_then_read".to_string(),
            state_directory_protected: supported_host,
            unicode_policy_paths_only: true,
        },
        network: NetworkCapabilities {
            denied: supported_host,
            unrestricted: supported_host,
            managed_proxy: supported_host,
            full_access: supported_host,
            limited_access: supported_host,
            http: supported_host,
            socks: supported_host,
            socks_udp: false,
            upstream_proxy: supported_host,
            domain_allow_patterns: supported_host,
            domain_deny_patterns: supported_host,
            local_binding_policy: supported_host
                && cfg!(any(target_os = "macos", target_os = "windows")),
            loopback_policy: supported_host
                && cfg!(any(target_os = "macos", target_os = "windows")),
            unix_socket_policy: supported_host && cfg!(target_os = "macos"),
            unix_socket_allow_rules: supported_host && cfg!(target_os = "macos"),
            unix_socket_deny_rules: false,
            explicit_local_ports: false,
            managed_ca: false,
            direct_egress_confinement: supported_host,
            non_loopback_listeners: false,
        },
        streams: StreamCapabilities {
            inherited: supported_host,
            passed_handle: supported_host,
            null: supported_host,
            independent: supported_host,
            byte_transparent: supported_host,
            application_bytes_on_control_channel: false,
        },
        terminal: TerminalCapabilities {
            inherited_terminal: supported_host
                && cfg!(any(target_os = "macos", target_os = "linux")),
            caller_supplied_pty: supported_host
                && cfg!(any(target_os = "macos", target_os = "linux")),
            controlling_terminal_reopen: supported_host && cfg!(target_os = "macos"),
            pty_creation_inside_sandbox: supported_host && cfg!(target_os = "macos"),
            host_device_isolation: supported_host && cfg!(target_os = "macos"),
        },
        lifecycle: LifecycleCapabilities {
            interrupt: supported_host && cfg!(target_os = "macos"),
            graceful_termination: supported_host && cfg!(target_os = "macos"),
            forced_termination: supported_host,
            root_exit_observation: supported_host,
            process_tree_supervision: supported_host
                && cfg!(any(target_os = "linux", target_os = "windows")),
            full_tree_retirement: supported_host
                && cfg!(any(target_os = "linux", target_os = "windows")),
            cleanup_after_root_exit: supported_host,
            control_loss_retires_target: supported_host,
        },
        required_companions: companion_resources(),
        setup,
    }
}

fn companion_resources() -> Vec<CompanionResource> {
    if cfg!(target_os = "linux") {
        vec![CompanionResource {
            name: "bubblewrap".to_string(),
            relative_path: "codex-resources/bwrap".to_string(),
            required: true,
        }]
    } else if cfg!(target_os = "windows") {
        vec![
            CompanionResource {
                name: "windows sandbox setup".to_string(),
                relative_path: "codex-resources/codex-windows-sandbox-setup.exe".to_string(),
                required: true,
            },
            CompanionResource {
                name: "elevated command runner".to_string(),
                relative_path: "codex-resources/codex-command-runner.exe".to_string(),
                required: true,
            },
        ]
    } else {
        Vec::new()
    }
}

pub fn setup_status(state_dir: &Path) -> SetupStatus {
    let backend_availability = backend_availability(state_dir);
    setup_status_from_backend_availability(&backend_availability)
}

fn setup_status_from_backend_availability(
    backend_availability: &Result<(), String>,
) -> SetupStatus {
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        return SetupStatus {
            state: SetupState::Unsupported,
            detail: Some("no native Codex sandbox backend exists for this platform".to_string()),
        };
    }
    if let Err(detail) = backend_availability {
        return SetupStatus {
            state: SetupState::Unavailable,
            detail: Some(detail.clone()),
        };
    }
    SetupStatus {
        state: if cfg!(target_os = "windows") {
            SetupState::RefreshRequired
        } else {
            SetupState::NotRequired
        },
        detail: None,
    }
}

pub fn backend_availability(state_dir: &Path) -> Result<(), String> {
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = state_dir;
        return Err("no native Codex sandbox backend exists for this platform".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        let sandbox_exec = Path::new(codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        if !sandbox_exec.is_file() {
            return Err(format!(
                "required Seatbelt executable is unavailable: {}",
                sandbox_exec.display()
            ));
        }
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux_process::LinuxProcess::open(std::process::id())
            .map_err(|error| format!("Linux pidfd supervision is unavailable: {error}"))?;
        let runner = std::env::current_exe().map_err(|error| error.to_string())?;
        codex_linux_sandbox::verify_packaged_bwrap_runtime(&runner, state_dir)?;
    }
    #[cfg(target_os = "windows")]
    {
        use codex_windows_sandbox::WindowsSandboxStandaloneResources;

        let runner = std::env::current_exe().map_err(|error| error.to_string())?;
        let resource_dir = runner
            .parent()
            .ok_or_else(|| "runner executable has no parent directory".to_string())?
            .join("codex-resources");
        codex_windows_sandbox::verify_windows_sandbox_standalone_resources(
            &WindowsSandboxStandaloneResources {
                setup_executable: resource_dir.join("codex-windows-sandbox-setup.exe"),
                command_runner_executable: resource_dir.join("codex-command-runner.exe"),
            },
        )
        .map_err(|error| format!("{error:#}"))?;
    }
    let _ = state_dir;
    Ok(())
}
