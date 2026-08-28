use crate::environment::NativeEnvironment;
use crate::network::PreparedNetwork;
#[cfg(unix)]
use crate::policy::ValidatedPolicy;
use crate::protocol::LaunchRequest;
use crate::stdio::PassedStreamEndpoints;
use crate::supervisor::Supervisor;
use anyhow::Error;
use std::ffi::OsString;
#[cfg(unix)]
use std::path::Path;

pub struct StartedTarget {
    pub root_process_id: Option<u32>,
    pub supervisor: Supervisor,
}

pub enum StartError {
    Preparation(Error),
    Launch {
        source: Error,
        supervisor: Option<Supervisor>,
    },
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub async fn start(
    target: &[OsString],
    launch: &LaunchRequest,
    policy: &ValidatedPolicy,
    network: &mut PreparedNetwork,
    environment: &NativeEnvironment,
    state_directory: &Path,
    control_fd: i32,
    passed_stream_endpoints: &mut PassedStreamEndpoints,
) -> Result<StartedTarget, StartError> {
    use crate::platform::prepare_command;
    use crate::watchdog::ProcessGroupWatchdog;
    use std::os::fd::AsRawFd;

    let mut prepared = prepare_command(
        target,
        launch,
        policy,
        network,
        environment,
        state_directory,
        passed_stream_endpoints,
    )
    .map_err(StartError::Preparation)?;
    let mut child = prepared
        .command
        .spawn()
        .map_err(|source| StartError::Launch {
            source: source.into(),
            supervisor: None,
        })?;
    let process_id = child.id().ok_or_else(|| StartError::Launch {
        source: anyhow::anyhow!("launched sandbox process has no process identifier"),
        supervisor: None,
    })?;
    drop(prepared.launch_gate_reader);
    let launch_status_fd = prepared.launch_status_writer.as_raw_fd();
    let launch_status_flags = unsafe { libc::fcntl(launch_status_fd, libc::F_GETFD) };
    if launch_status_flags == -1
        || unsafe {
            libc::fcntl(
                launch_status_fd,
                libc::F_SETFD,
                launch_status_flags | libc::FD_CLOEXEC,
            )
        } == -1
    {
        let source = std::io::Error::last_os_error();
        drop(prepared.launch_gate);
        drop(prepared.launch_status_writer);
        let _ = codex_utils_pty::process_group::kill_process_group(process_id);
        let _ = child.wait().await;
        return Err(StartError::Launch {
            source: Error::new(source).context("secure sandbox launch status descriptor"),
            supervisor: None,
        });
    }
    let launch_commit_status_fd = prepared.launch_commit_status_writer.as_raw_fd();
    let launch_commit_status_flags = unsafe { libc::fcntl(launch_commit_status_fd, libc::F_GETFD) };
    if launch_commit_status_flags == -1
        || unsafe {
            libc::fcntl(
                launch_commit_status_fd,
                libc::F_SETFD,
                launch_commit_status_flags | libc::FD_CLOEXEC,
            )
        } == -1
    {
        let source = std::io::Error::last_os_error();
        drop(prepared.launch_gate);
        drop(prepared.launch_status_writer);
        drop(prepared.launch_commit_status_writer);
        let _ = codex_utils_pty::process_group::kill_process_group(process_id);
        let _ = child.wait().await;
        return Err(StartError::Launch {
            source: Error::new(source).context("secure sandbox launch commit descriptor"),
            supervisor: None,
        });
    }
    let watchdog = match ProcessGroupWatchdog::start(process_id).await {
        Ok(watchdog) => watchdog,
        Err(source) => {
            let message = format!("process-group watchdog startup failed: {source}");
            drop(prepared.launch_gate);
            drop(prepared.launch_status_writer);
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let _ = child.wait().await;
            return Err(StartError::Launch {
                source: anyhow::anyhow!(message),
                supervisor: None,
            });
        }
    };
    drop(prepared.launch_status_writer);
    drop(prepared.launch_commit_status_writer);
    let gated_launch_status = match prepared.launch_status.wait_for_gate().await {
        Ok(gated_launch_status) => gated_launch_status,
        Err(source) => {
            drop(prepared.launch_gate);
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let _ = watchdog.disarm().await;
            let _ = child.wait().await;
            return Err(StartError::Launch {
                source,
                supervisor: None,
            });
        }
    };
    let commit_gate = match prepared.launch_gate.release() {
        Ok(commit_gate) => commit_gate,
        Err(source) => {
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let _ = watchdog.disarm().await;
            let _ = child.wait().await;
            return Err(StartError::Launch {
                source,
                supervisor: None,
            });
        }
    };
    let (confirmation, target_completion) = gated_launch_status.confirm().await;
    let supervisor = Supervisor::start(
        child,
        process_id,
        launch.lifecycle.clone(),
        network.handle.take(),
        Some(watchdog),
        target_completion,
    );
    passed_stream_endpoints.release();
    let stream_close_error =
        crate::stdio::close_inherited_runner_streams(&launch.streams, control_fd).err();
    let confirmed_target = match confirmation {
        Ok(confirmed_target) => confirmed_target,
        Err(source) => {
            drop(commit_gate);
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let source = match stream_close_error {
                Some(stream_error) => anyhow::anyhow!(
                    "{source:#}; could not release inherited standard streams: {stream_error}"
                ),
                None => source,
            };
            return Err(StartError::Launch {
                source,
                supervisor: Some(supervisor),
            });
        }
    };
    #[cfg(target_os = "linux")]
    let _ = confirmed_target;
    #[cfg(target_os = "linux")]
    let root_process_id = None;
    #[cfg(not(target_os = "linux"))]
    let root_process_id = Some(confirmed_target.process_id);
    if let Some(source) = stream_close_error {
        drop(commit_gate);
        return Err(StartError::Launch {
            source: anyhow::anyhow!("could not release inherited standard streams: {source}"),
            supervisor: Some(supervisor),
        });
    }
    if let Err(source) = commit_gate.commit() {
        return Err(StartError::Launch {
            source,
            supervisor: Some(supervisor),
        });
    }
    if let Err(source) = prepared.launch_commit_status.confirm().await {
        return Err(StartError::Launch {
            source,
            supervisor: Some(supervisor),
        });
    }
    Ok(StartedTarget {
        root_process_id,
        supervisor,
    })
}

#[cfg(windows)]
pub fn start(
    target: &[OsString],
    launch: &LaunchRequest,
    setup: codex_windows_sandbox::WindowsSandboxStandaloneSetupRequest,
    network: &mut PreparedNetwork,
    environment: &NativeEnvironment,
    control_handle: u64,
    passed_stream_endpoints: &mut PassedStreamEndpoints,
) -> Result<StartedTarget, StartError> {
    let process = tokio::task::block_in_place(|| {
        crate::platform::spawn_windows_target(
            target,
            launch,
            setup,
            network,
            environment,
            passed_stream_endpoints,
        )
    })
    .map_err(|source| StartError::Launch {
        source,
        supervisor: None,
    })?;
    passed_stream_endpoints.release();
    let commit_process = process.clone();
    let supervisor = Supervisor::start(process, launch.lifecycle.clone(), network.handle.take());
    if let Err(source) =
        crate::stdio::close_inherited_runner_streams(&launch.streams, control_handle)
    {
        let _ = commit_process.force_terminate(std::time::Duration::from_millis(
            launch.lifecycle.force_timeout_ms,
        ));
        return Err(StartError::Launch {
            source: anyhow::anyhow!("could not release inherited standard streams: {source}"),
            supervisor: Some(supervisor),
        });
    }
    let process_id = match tokio::task::block_in_place(|| commit_process.wait_ready()) {
        Ok(process_id) => process_id,
        Err(source) => {
            return Err(StartError::Launch {
                source,
                supervisor: Some(supervisor),
            });
        }
    };
    if let Err(source) = commit_process.commit_launch() {
        return Err(StartError::Launch {
            source,
            supervisor: Some(supervisor),
        });
    }
    Ok(StartedTarget {
        root_process_id: Some(process_id),
        supervisor,
    })
}
