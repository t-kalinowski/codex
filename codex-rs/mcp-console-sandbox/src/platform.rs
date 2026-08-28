use crate::environment::NativeEnvironment;
use crate::network::PreparedNetwork;
use crate::policy::ValidatedPolicy;
use crate::protocol::LaunchRequest;
use crate::stdio::PassedStreamEndpoints;
use anyhow::Result;
use std::ffi::OsString;
use std::path::Path;
#[cfg(unix)]
use tokio::process::Command;

#[cfg(unix)]
pub struct PreparedCommand {
    pub command: Command,
    pub launch_gate: crate::launch_bridge::TargetStartGate,
    pub launch_gate_reader: std::os::fd::OwnedFd,
    pub launch_status: crate::launch_bridge::LaunchStatus,
    pub launch_status_writer: std::os::fd::OwnedFd,
    pub launch_commit_status: crate::launch_bridge::LaunchCommitStatus,
    pub launch_commit_status_writer: std::os::fd::OwnedFd,
}

#[cfg(unix)]
pub fn prepare_command(
    target: &[OsString],
    launch: &LaunchRequest,
    policy: &ValidatedPolicy,
    network: &PreparedNetwork,
    environment: &NativeEnvironment,
    state_directory: &Path,
    passed_stream_endpoints: &PassedStreamEndpoints,
) -> Result<PreparedCommand> {
    unix::prepare_command(
        target,
        launch,
        policy,
        network,
        environment,
        state_directory,
        passed_stream_endpoints,
    )
}

#[cfg(windows)]
pub fn prepare_windows_setup_request(
    policy: &ValidatedPolicy,
    network: &PreparedNetwork,
    environment: &NativeEnvironment,
    state_directory: &Path,
) -> Result<codex_windows_sandbox::WindowsSandboxStandaloneSetupRequest> {
    windows::prepare_setup_request(policy, network, environment, state_directory)
}

#[cfg(windows)]
pub fn spawn_windows_target(
    target: &[OsString],
    launch: &LaunchRequest,
    setup: codex_windows_sandbox::WindowsSandboxStandaloneSetupRequest,
    network: &PreparedNetwork,
    environment: &NativeEnvironment,
    passed_stream_endpoints: &PassedStreamEndpoints,
) -> Result<codex_windows_sandbox::WindowsSandboxStandaloneProcess> {
    windows::spawn_target(
        target,
        launch,
        setup,
        network,
        environment,
        passed_stream_endpoints,
    )
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::protocol::StandardStreams;
    use crate::protocol::StreamSpec;
    use crate::protocol::TerminalPolicy;
    use anyhow::Context;
    use codex_protocol::models::PermissionProfile;
    use codex_sandboxing::SandboxType;
    use codex_sandboxing::resolve_windows_elevated_filesystem_overrides;
    use codex_windows_sandbox::WindowsSandboxStandaloneCommand;
    use codex_windows_sandbox::WindowsSandboxStandaloneFilesystemOverrides;
    use codex_windows_sandbox::WindowsSandboxStandaloneLaunchRequest;
    use codex_windows_sandbox::WindowsSandboxStandaloneNetworkIdentity;
    use codex_windows_sandbox::WindowsSandboxStandaloneNetworkSetup;
    use codex_windows_sandbox::WindowsSandboxStandalonePolicyRequest;
    use codex_windows_sandbox::WindowsSandboxStandaloneResources;
    use codex_windows_sandbox::WindowsSandboxStandaloneStdio;
    use codex_windows_sandbox::WindowsSandboxStandaloneStream;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    pub(super) fn prepare_setup_request(
        policy: &ValidatedPolicy,
        network: &PreparedNetwork,
        environment: &NativeEnvironment,
        state_directory: &Path,
    ) -> Result<codex_windows_sandbox::WindowsSandboxStandaloneSetupRequest> {
        let permission_profile =
            PermissionProfile::from_runtime_permissions(&policy.filesystem, network.sandbox_policy);
        let overrides = resolve_windows_elevated_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &policy.policy_base_directory,
            /*use_windows_elevated_backend*/ true,
        )
        .map_err(anyhow::Error::msg)
        .context("translate normalized filesystem policy for Windows")?
        .map_or_else(
            WindowsSandboxStandaloneFilesystemOverrides::default,
            |overrides| WindowsSandboxStandaloneFilesystemOverrides {
                read_roots: overrides.read_roots_override,
                read_roots_include_platform_defaults: overrides
                    .read_roots_include_platform_defaults,
                write_roots: overrides.write_roots_override,
                additional_deny_read_paths: overrides
                    .additional_deny_read_paths
                    .into_iter()
                    .map(codex_utils_absolute_path::AbsolutePathBuf::into_path_buf)
                    .collect(),
                additional_deny_write_paths: overrides
                    .additional_deny_write_paths
                    .into_iter()
                    .map(codex_utils_absolute_path::AbsolutePathBuf::into_path_buf)
                    .collect(),
            },
        );
        let resources = windows_resources()?;
        let environment = policy_environment(environment)?;
        codex_windows_sandbox::windows_sandbox_standalone_setup_request_from_permission_profile(
            WindowsSandboxStandalonePolicyRequest {
                permission_profile: &permission_profile,
                workspace_roots: &[],
                command_cwd: policy.working_directory.as_path(),
                environment: &environment,
                state_dir: state_directory.to_path_buf(),
                resources,
                filesystem_overrides: overrides,
                network: network_setup(network)?,
            },
        )
    }

    pub(super) fn spawn_target(
        target: &[OsString],
        launch: &LaunchRequest,
        setup: codex_windows_sandbox::WindowsSandboxStandaloneSetupRequest,
        network: &PreparedNetwork,
        environment: &NativeEnvironment,
        passed_stream_endpoints: &PassedStreamEndpoints,
    ) -> Result<codex_windows_sandbox::WindowsSandboxStandaloneProcess> {
        if launch.terminal == TerminalPolicy::IsolateHostDevices {
            anyhow::bail!("Windows host terminal-device isolation is unsupported in this release")
        }
        let (program, args) = target
            .split_first()
            .context("launch requires a native target executable")?;
        let passed_handles = passed_stream_endpoints.duplicate_for_launch(&launch.streams)?;
        let streams = streams(&launch.streams, &passed_handles)?;
        let restricting_sid = if network.enforce_managed_network {
            Some(
                network
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.network_proxy_restricting_sid(None))
                    .context("managed Windows proxy route has no restricting SID")?,
            )
        } else {
            None
        };
        codex_windows_sandbox::spawn_windows_sandbox_standalone(
            WindowsSandboxStandaloneLaunchRequest {
                setup,
                command: WindowsSandboxStandaloneCommand {
                    program: PathBuf::from(program),
                    args: args.to_vec(),
                    environment: environment
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                    cwd: PathBuf::from(&launch.working_directory),
                },
                stdio: streams,
                network_proxy_restricting_sid: restricting_sid,
                use_private_desktop: launch
                    .platform_extensions
                    .windows
                    .as_ref()
                    .is_some_and(|extensions| extensions.private_desktop),
                descendant_grace: Duration::from_millis(launch.lifecycle.root_exit_grace_ms),
                force_stop_timeout: Duration::from_millis(launch.lifecycle.force_timeout_ms),
            },
        )
    }

    fn windows_resources() -> Result<WindowsSandboxStandaloneResources> {
        let runner = std::env::current_exe().context("resolve runner executable")?;
        let resources = runner
            .parent()
            .context("runner executable has no parent directory")?
            .join("codex-resources");
        Ok(WindowsSandboxStandaloneResources {
            setup_executable: resources.join("codex-windows-sandbox-setup.exe"),
            command_runner_executable: resources.join("codex-command-runner.exe"),
        })
    }

    fn network_setup(network: &PreparedNetwork) -> Result<WindowsSandboxStandaloneNetworkSetup> {
        if network.enforce_managed_network {
            let context = network
                .sandbox_context
                .as_ref()
                .context("managed network sandbox context is missing")?;
            return Ok(WindowsSandboxStandaloneNetworkSetup {
                identity: WindowsSandboxStandaloneNetworkIdentity::Offline,
                proxy_ports: context.loopback_ports.clone(),
                allow_local_binding: context.allow_local_binding,
            });
        }
        Ok(WindowsSandboxStandaloneNetworkSetup {
            identity: if network.sandbox_policy.is_enabled() {
                WindowsSandboxStandaloneNetworkIdentity::Online
            } else {
                WindowsSandboxStandaloneNetworkIdentity::Offline
            },
            proxy_ports: Vec::new(),
            allow_local_binding: false,
        })
    }

    fn policy_environment(environment: &NativeEnvironment) -> Result<HashMap<String, String>> {
        let mut policy_environment = HashMap::new();
        for (key, value) in environment {
            let key = key.to_str().context(
                "target environment name is not valid Unicode; protocol version 1 rejects it",
            )?;
            let Some(value) = value.to_str() else {
                if key.eq_ignore_ascii_case("TEMP") || key.eq_ignore_ascii_case("TMP") {
                    anyhow::bail!(
                        "target {key} value is not valid Unicode and cannot be used for Windows filesystem policy translation"
                    )
                }
                // The complete native environment remains on the launch request. This
                // projection is used only by Codex's root-resolution code, which can
                // consume valid-Unicode values and consults TEMP/TMP for authority.
                continue;
            };
            policy_environment.insert(key.to_string(), value.to_string());
        }
        Ok(policy_environment)
    }

    fn streams<'a>(
        streams: &StandardStreams,
        passed_handles: &'a crate::stdio::LaunchStreamEndpoints,
    ) -> Result<WindowsSandboxStandaloneStdio<'a>> {
        Ok(WindowsSandboxStandaloneStdio {
            stdin: stream(streams.stdin, passed_handles)?,
            stdout: stream(streams.stdout, passed_handles)?,
            stderr: stream(streams.stderr, passed_handles)?,
        })
    }

    fn stream<'a>(
        stream: StreamSpec,
        passed_handles: &'a crate::stdio::LaunchStreamEndpoints,
    ) -> Result<WindowsSandboxStandaloneStream<'a>> {
        match stream {
            StreamSpec::Inherited => Ok(WindowsSandboxStandaloneStream::Inherited),
            StreamSpec::Null => Ok(WindowsSandboxStandaloneStream::Null),
            StreamSpec::PassedHandle { handle } => passed_handles
                .handle(handle)
                .map(WindowsSandboxStandaloneStream::Passed)
                .ok_or_else(|| anyhow::anyhow!("passed target stream handle was not retained")),
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::protocol::StandardStreams;
    use crate::protocol::StreamSpec;
    use crate::protocol::TerminalPolicy;
    use anyhow::Context;
    use std::process::Stdio;

    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_command(
        target: &[OsString],
        launch: &LaunchRequest,
        policy: &ValidatedPolicy,
        network: &PreparedNetwork,
        environment: &NativeEnvironment,
        state_directory: &Path,
        passed_stream_endpoints: &PassedStreamEndpoints,
    ) -> Result<PreparedCommand> {
        let mut stream_endpoints = passed_stream_endpoints.duplicate_for_launch(&launch.streams)?;
        let runner_executable =
            std::env::current_exe().context("resolve runner executable for sandbox policy")?;
        #[cfg(target_os = "linux")]
        let bridge_mode = crate::launch_bridge::LaunchBridgeMode::NamespacePid1;
        #[cfg(not(target_os = "linux"))]
        let bridge_mode = crate::launch_bridge::LaunchBridgeMode::Direct;
        let bridge = crate::launch_bridge::prepare_target(&runner_executable, target, bridge_mode)?;
        let (program, arguments) = bridge
            .command
            .split_first()
            .context("launch bridge command is empty")?;
        let mut filesystem = policy.filesystem.clone();
        let runner_executable =
            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(runner_executable)
                .context("runner executable must be absolute")?;
        filesystem
            .entries
            .push(codex_protocol::permissions::FileSystemSandboxEntry::new(
                runner_executable.into(),
                codex_protocol::permissions::FileSystemAccessMode::Read,
            ));
        let mut command = native_command(
            program,
            arguments,
            launch,
            policy,
            &filesystem,
            network,
            state_directory,
        )?;
        command
            .current_dir(policy.working_directory.as_path())
            .env_clear()
            .envs(environment);
        configure_streams(&mut command, &launch.streams, &mut stream_endpoints)?;
        #[cfg(target_os = "linux")]
        let parent_pid = std::process::id() as i32;
        let terminal = launch.terminal;
        unsafe {
            command.pre_exec(move || {
                match terminal {
                    TerminalPolicy::Preserve => {
                        codex_utils_pty::process_group::set_process_group()?
                    }
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
            launch_commit_status: bridge.commit_status,
            launch_commit_status_writer: bridge.commit_status_writer,
        })
    }

    #[cfg(target_os = "macos")]
    fn native_command(
        program: &std::ffi::OsStr,
        arguments: &[OsString],
        launch: &LaunchRequest,
        policy: &ValidatedPolicy,
        filesystem: &codex_protocol::permissions::FileSystemSandboxPolicy,
        network: &PreparedNetwork,
        _state_directory: &Path,
    ) -> Result<Command> {
        use codex_sandboxing::seatbelt::CreateSeatbeltCommandArgsParams;
        use codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
        use codex_sandboxing::seatbelt::MacosTerminalPolicy;
        use codex_sandboxing::seatbelt::create_seatbelt_command_args_with_terminal_policy;

        let terminal_policy = match launch.terminal {
            TerminalPolicy::Preserve => MacosTerminalPolicy::BackendDefault,
            TerminalPolicy::IsolateHostDevices => MacosTerminalPolicy::DenyPreexistingReopen,
        };
        let seatbelt_arguments = create_seatbelt_command_args_with_terminal_policy(
            CreateSeatbeltCommandArgsParams {
                command: Vec::new(),
                file_system_sandbox_policy: filesystem,
                network_sandbox_policy: network.sandbox_policy,
                sandbox_policy_cwd: policy.policy_base_directory.as_path(),
                enforce_managed_network: network.enforce_managed_network,
                managed_network: network.sandbox_context.as_ref(),
                environment_id: None,
                network: network.proxy.as_ref(),
                extra_allow_unix_sockets: &[],
            },
            terminal_policy,
        )
        .map_err(anyhow::Error::msg)
        .context("Seatbelt policy preparation failed")?;
        let mut command = Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        command
            .args(seatbelt_arguments)
            .arg(program)
            .args(arguments);
        Ok(command)
    }

    #[cfg(target_os = "linux")]
    fn native_command(
        program: &std::ffi::OsStr,
        arguments: &[OsString],
        launch: &LaunchRequest,
        policy: &ValidatedPolicy,
        filesystem: &codex_protocol::permissions::FileSystemSandboxPolicy,
        network: &PreparedNetwork,
        state_directory: &Path,
    ) -> Result<Command> {
        use codex_protocol::models::PermissionProfile;
        use codex_sandboxing::landlock::CODEX_LINUX_SANDBOX_ARG0;
        use codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile_native;
        use std::os::unix::process::CommandExt;

        if launch.terminal == TerminalPolicy::IsolateHostDevices {
            anyhow::bail!("Linux host terminal-device isolation is unsupported in this release")
        }
        let runner = std::env::current_exe().context("resolve Linux sandbox runner executable")?;
        let embedding = codex_linux_sandbox::prepare_packaged_bwrap(&runner, state_directory)
            .map_err(anyhow::Error::msg)
            .context("prepare packaged Linux sandbox companion")?;
        let permission_profile =
            PermissionProfile::from_runtime_permissions(filesystem, network.sandbox_policy);
        let target = std::iter::once(program.to_os_string())
            .chain(arguments.iter().cloned())
            .collect();
        let mut helper_arguments = embedding.helper_args();
        helper_arguments.extend(
            create_linux_sandbox_command_args_for_permission_profile_native(
                target,
                policy.working_directory.as_path(),
                &permission_profile,
                policy.policy_base_directory.as_path(),
                /*use_legacy_landlock*/ false,
                network.enforce_managed_network,
            ),
        );
        let mut command = Command::new(runner);
        command.args(helper_arguments);
        command.as_std_mut().arg0(CODEX_LINUX_SANDBOX_ARG0);
        Ok(command)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn native_command(
        _program: &std::ffi::OsStr,
        _arguments: &[OsString],
        _launch: &LaunchRequest,
        _policy: &ValidatedPolicy,
        _filesystem: &codex_protocol::permissions::FileSystemSandboxPolicy,
        _network: &PreparedNetwork,
        _state_directory: &Path,
    ) -> Result<Command> {
        anyhow::bail!("this operating system has no native sandbox backend")
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
}
