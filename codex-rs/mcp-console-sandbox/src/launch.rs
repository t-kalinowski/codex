use crate::cleanup::CleanupDirectory;
use crate::environment::TargetEnvironment;
use crate::network::PreparedNetwork;
use crate::policy::ValidatedPolicy;
use crate::protocol::LaunchRequest;
use crate::stdio::PassedStreamEndpoints;
use crate::supervisor::Supervisor;
use anyhow::Error;
use std::ffi::OsString;
use std::path::Path;

#[cfg(target_os = "macos")]
use crate::lifetime::LifetimeManager;

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

#[allow(clippy::too_many_arguments)]
pub async fn start(
    target: &[OsString],
    launch: &LaunchRequest,
    policy: &ValidatedPolicy,
    network: &mut PreparedNetwork,
    environment: &TargetEnvironment,
    state_directory: &Path,
    control_fd: i32,
    passed_stream_endpoints: &mut PassedStreamEndpoints,
    cleanup_directory: &mut Option<CleanupDirectory>,
) -> Result<StartedTarget, StartError> {
    if cleanup_directory.is_none() {
        return Err(StartError::Preparation(anyhow::anyhow!(
            "target cleanup directory ownership was already consumed"
        )));
    }
    let mut prepared = crate::platform::prepare_command(
        target,
        launch,
        policy,
        network,
        environment,
        state_directory,
        passed_stream_endpoints,
    )
    .map_err(StartError::Preparation)?;
    #[cfg(target_os = "macos")]
    let foreground_terminal = if launch.lifecycle.kind == crate::protocol::LaunchKind::Command {
        passed_stream_endpoints
            .foreground_terminal(&launch.streams)
            .map_err(|error| StartError::Preparation(error.into()))?
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let mut lifetime_manager =
        LifetimeManager::spawn(&launch.lifecycle, foreground_terminal.as_ref())
            .map_err(|error| StartError::Preparation(anyhow::anyhow!(error)))?;
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
    let cleanup_directory = cleanup_directory.take().ok_or_else(|| {
        StartError::Preparation(anyhow::anyhow!(
            "target cleanup directory ownership was already consumed"
        ))
    })?;
    #[cfg(target_os = "macos")]
    {
        if let Err(error) =
            lifetime_manager.observe(process_id, &launch.lifecycle, &cleanup_directory)
        {
            drop(prepared.launch_gate);
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let _ = child.wait().await;
            let _ = lifetime_manager.stop();
            return Err(StartError::Launch {
                source: anyhow::anyhow!(error),
                supervisor: None,
            });
        }
        if let Err(error) = lifetime_manager.commit() {
            drop(prepared.launch_gate);
            let manager_error = lifetime_manager.stop().err();
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let _ = child.wait().await;
            let source = match manager_error {
                Some(manager_error) => {
                    anyhow::anyhow!("{error}; additionally, {manager_error}")
                }
                None => anyhow::anyhow!(error),
            };
            return Err(StartError::Launch {
                source,
                supervisor: None,
            });
        }
        if let Err(error) = lifetime_manager.monitor(process_id, cleanup_directory) {
            drop(prepared.launch_gate);
            let manager_error = lifetime_manager.stop().err();
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let _ = child.wait().await;
            let source = match manager_error {
                Some(manager_error) => anyhow::anyhow!("{error}; additionally, {manager_error}"),
                None => anyhow::anyhow!(error),
            };
            return Err(StartError::Launch {
                source,
                supervisor: None,
            });
        }
    }
    drop(prepared.launch_gate_reader);
    drop(prepared.launch_status_writer);
    let target_completion = match prepared.launch_status.wait_for_gate().await {
        Ok(target_completion) => target_completion,
        Err(source) => {
            drop(prepared.launch_gate);
            #[cfg(target_os = "macos")]
            let manager_error = lifetime_manager.stop().err();
            #[cfg(not(target_os = "macos"))]
            let manager_error: Option<String> = None;
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
            let _ = child.wait().await;
            let source = match manager_error {
                Some(manager_error) => {
                    anyhow::anyhow!("{source:#}; additionally, {manager_error}")
                }
                None => source,
            };
            return Err(StartError::Launch {
                source,
                supervisor: None,
            });
        }
    };
    drop(prepared.sandbox_canary);
    #[cfg(target_os = "macos")]
    let foreground_error = foreground_terminal
        .as_ref()
        .and_then(|terminal| terminal.assign(process_id as libc::pid_t).err());
    let supervisor = Supervisor::start(
        child,
        process_id,
        launch.lifecycle.clone(),
        network.handle.take(),
        target_completion,
        #[cfg(target_os = "macos")]
        lifetime_manager,
        #[cfg(target_os = "macos")]
        foreground_terminal,
        #[cfg(not(target_os = "macos"))]
        cleanup_directory,
    );
    #[cfg(target_os = "macos")]
    if let Some(source) = foreground_error {
        drop(prepared.launch_gate);
        return Err(StartError::Launch {
            source: anyhow::anyhow!("could not foreground the sandbox target: {source}"),
            supervisor: Some(supervisor),
        });
    }
    passed_stream_endpoints.release();
    if let Err(source) = crate::stdio::close_inherited_runner_streams(&launch.streams, control_fd) {
        drop(prepared.launch_gate);
        return Err(StartError::Launch {
            source: anyhow::anyhow!("could not release inherited standard streams: {source}"),
            supervisor: Some(supervisor),
        });
    }
    if let Err(source) = prepared.launch_gate.release() {
        return Err(StartError::Launch {
            source,
            supervisor: Some(supervisor),
        });
    }
    Ok(StartedTarget {
        #[cfg(target_os = "linux")]
        root_process_id: None,
        #[cfg(not(target_os = "linux"))]
        root_process_id: Some(process_id),
        supervisor,
    })
}
