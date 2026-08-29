use crate::environment::TargetEnvironment;
use crate::network::PreparedNetwork;
use crate::policy::ValidatedPolicy;
use crate::protocol::LaunchRequest;
use crate::protocol::NetworkPolicy;
use crate::protocol::StandardStreams;
use crate::protocol::StreamSpec;
use crate::protocol::TerminalPolicy;
use crate::stdio::PassedStreamEndpoints;
use anyhow::Context;
use anyhow::Result;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxType;
use codex_utils_path_uri::PathUri;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

pub(crate) struct PreparedCommand {
    pub(crate) command: Command,
    pub(crate) launch_gate: crate::launch_bridge::TargetStartGate,
    pub(crate) launch_gate_reader: std::os::fd::OwnedFd,
    pub(crate) launch_status: crate::launch_bridge::LaunchStatus,
    pub(crate) launch_status_writer: std::os::fd::OwnedFd,
    pub(crate) sandbox_canary: crate::launch_bridge::SandboxCanary,
    #[cfg(target_os = "macos")]
    pub(crate) _target_arguments: std::fs::File,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_command(
    target: &[OsString],
    launch: &LaunchRequest,
    policy: &ValidatedPolicy,
    network: &PreparedNetwork,
    environment: &TargetEnvironment,
    state_directory: &Path,
    passed_stream_endpoints: &PassedStreamEndpoints,
) -> Result<PreparedCommand> {
    let mut stream_endpoints = passed_stream_endpoints.duplicate_for_launch(&launch.streams)?;
    let runner = std::env::current_exe().context("resolve runner executable")?;
    let bridge = crate::launch_bridge::prepare_target(
        &runner,
        target,
        state_directory,
        !matches!(&launch.network, NetworkPolicy::Unrestricted),
    )?;
    let (program, arguments) = bridge
        .command
        .split_first()
        .context("launch bridge command is empty")?;
    let program = program
        .to_str()
        .context("runner executable must be valid Unicode")?;
    let arguments = arguments
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(str::to_owned)
                .context("target arguments must be valid Unicode in protocol version 1")
        })
        .collect::<Result<Vec<_>>>()?;
    let permissions =
        PermissionProfile::from_runtime_permissions(&policy.filesystem, network.sandbox_policy);
    let cwd = PathUri::from_abs_path(&policy.working_directory);
    let sandbox_policy_cwd = PathUri::from_abs_path(&policy.policy_base_directory);
    let transformed = SandboxManager::for_mcp_console()
        .transform(SandboxTransformRequest {
            command: SandboxCommand {
                program: program.into(),
                args: arguments,
                cwd,
                env: environment.clone(),
                managed_network: network.sandbox_context.clone(),
                additional_permissions: None,
            },
            permissions: &permissions,
            sandbox: native_sandbox(),
            enforce_managed_network: network.enforce_managed_network,
            environment_id: None,
            network: network.proxy.as_ref(),
            sandbox_policy_cwd: &sandbox_policy_cwd,
            codex_linux_sandbox_exe: Some(&runner),
            use_legacy_landlock: false,
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
            windows_sandbox_private_desktop: false,
        })
        .map_err(anyhow::Error::new)
        .context("translate normalized policy with the native sandbox backend")?;
    let (program, arguments) = transformed
        .command
        .split_first()
        .context("native sandbox transformation returned an empty command")?;
    let arguments = arguments.to_vec();
    #[cfg(target_os = "linux")]
    let arguments = {
        let mut arguments = arguments;
        let registry = state_directory.join("bwrap-synthetic-mount-registry");
        let registry = registry
            .to_str()
            .context("application state directory must be valid Unicode")?;
        arguments.splice(
            0..0,
            [
                "--mcp-console-bundled-bwrap".to_string(),
                "--mcp-console-state-root".to_string(),
                registry.to_string(),
            ],
        );
        arguments
    };
    #[cfg(not(target_os = "linux"))]
    let _ = state_directory;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(policy.working_directory.as_path())
        .env_clear()
        .envs(environment);
    if let Some(arg0) = transformed.arg0 {
        command.as_std_mut().arg0(arg0);
    }
    configure_streams(&mut command, &launch.streams, &mut stream_endpoints)?;
    #[cfg(target_os = "linux")]
    let parent_pid = std::process::id() as i32;
    let terminal = launch.terminal;
    unsafe {
        command.pre_exec(move || {
            match terminal {
                TerminalPolicy::Preserve => codex_utils_pty::process_group::set_process_group()?,
                TerminalPolicy::IsolateHostDevices => {
                    codex_utils_pty::process_group::detach_from_tty()?
                }
            }
            #[cfg(target_os = "linux")]
            codex_utils_pty::process_group::set_parent_death_signal(parent_pid)?;
            Ok(())
        });
    }
    Ok(PreparedCommand {
        command,
        launch_gate: bridge.gate,
        launch_gate_reader: bridge.gate_reader,
        launch_status: bridge.status,
        launch_status_writer: bridge.writer,
        sandbox_canary: bridge.canary,
        #[cfg(target_os = "macos")]
        _target_arguments: bridge.target_arguments,
    })
}

fn native_sandbox() -> SandboxType {
    #[cfg(target_os = "macos")]
    {
        SandboxType::MacosSeatbelt
    }
    #[cfg(target_os = "linux")]
    {
        SandboxType::LinuxSeccomp
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        unreachable!("platform preparation is only compiled for supported Unix backends")
    }
}

fn configure_streams(
    command: &mut Command,
    streams: &StandardStreams,
    passed_stream_endpoints: &mut crate::stdio::LaunchStreamEndpoints,
) -> Result<()> {
    command
        .stdin(stream_stdio(streams.stdin, passed_stream_endpoints)?)
        .stdout(stream_stdio(streams.stdout, passed_stream_endpoints)?)
        .stderr(stream_stdio(streams.stderr, passed_stream_endpoints)?);
    Ok(())
}

fn stream_stdio(
    stream: StreamSpec,
    passed_stream_endpoints: &mut crate::stdio::LaunchStreamEndpoints,
) -> Result<Stdio> {
    match stream {
        StreamSpec::Inherited => Ok(Stdio::inherit()),
        StreamSpec::Null => Ok(Stdio::null()),
        StreamSpec::PassedHandle { handle } => {
            let descriptor = passed_stream_endpoints
                .take_fd(handle)
                .context("passed target stream descriptor was not retained")?;
            Ok(Stdio::from(descriptor))
        }
    }
}
