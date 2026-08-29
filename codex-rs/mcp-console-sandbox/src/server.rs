use super::ActiveSupervisor;
use super::Args;
use codex_mcp_console_sandbox::PROTOCOL_VERSION;
use codex_mcp_console_sandbox::capabilities::capabilities;
use codex_mcp_console_sandbox::capabilities::setup_status;
use codex_mcp_console_sandbox::cleanup::CleanupDirectory;
use codex_mcp_console_sandbox::framing::FrameError;
use codex_mcp_console_sandbox::framing::read_frame;
use codex_mcp_console_sandbox::framing::write_frame;
use codex_mcp_console_sandbox::policy::validate_setup_support;
use codex_mcp_console_sandbox::protocol::AcknowledgedOperation;
use codex_mcp_console_sandbox::protocol::ClientRequest;
use codex_mcp_console_sandbox::protocol::ErrorCode;
use codex_mcp_console_sandbox::protocol::ErrorPhase;
use codex_mcp_console_sandbox::protocol::ProtocolError;
use codex_mcp_console_sandbox::protocol::RunnerPhase;
use codex_mcp_console_sandbox::protocol::RunnerStatus;
use codex_mcp_console_sandbox::protocol::ServerResponse;
use codex_mcp_console_sandbox::protocol::SetupCompletedOperation;
use codex_mcp_console_sandbox::stdio::PassedStreamEndpoints;
use std::time::Duration;

#[cfg(unix)]
use codex_mcp_console_sandbox::capabilities::BackendAvailabilityError;
#[cfg(unix)]
use codex_mcp_console_sandbox::capabilities::backend_availability;
#[cfg(unix)]
use codex_mcp_console_sandbox::capabilities::native_backend;
#[cfg(unix)]
use codex_mcp_console_sandbox::environment::target_environment;
#[cfg(unix)]
use codex_mcp_console_sandbox::launch::StartError;
#[cfg(unix)]
use codex_mcp_console_sandbox::launch::start;
#[cfg(unix)]
use codex_mcp_console_sandbox::network::prepare_network;
#[cfg(unix)]
use codex_mcp_console_sandbox::policy::absolute_target;
#[cfg(unix)]
use codex_mcp_console_sandbox::policy::validate_launch;
#[cfg(unix)]
use codex_mcp_console_sandbox::policy::validate_launch_support;
#[cfg(unix)]
use codex_mcp_console_sandbox::supervisor::Supervisor;

const MAX_CONTROL_WAIT_MS: u64 = 5 * 60 * 1000;

pub(crate) async fn control_loop(
    args: &Args,
    state_dir: &std::path::Path,
    control: &mut tokio::fs::File,
    supervisor: &mut Option<ActiveSupervisor>,
    endpoints: &mut PassedStreamEndpoints,
    cleanup_directory: &mut Option<CleanupDirectory>,
) -> anyhow::Result<()> {
    loop {
        let request: ClientRequest = match read_frame(control).await {
            Ok(request) => request,
            Err(FrameError::Closed) => return Ok(()),
            Err(error) => {
                let response = error_response(
                    /*id*/ None,
                    frame_error_code(&error),
                    ErrorPhase::Protocol,
                    error.to_string(),
                    generation_state(supervisor, cleanup_directory),
                );
                let _ = write_frame(control, &response).await;
                return Err(error.into());
            }
        };
        let id = request.id();
        if request.protocol_version() != PROTOCOL_VERSION {
            write_frame(
                control,
                &error_response(
                    Some(id),
                    ErrorCode::VersionMismatch,
                    ErrorPhase::Protocol,
                    format!(
                        "protocol version {} is unsupported; this runner requires {PROTOCOL_VERSION}",
                        request.protocol_version()
                    ),
                    generation_state(supervisor, cleanup_directory),
                ),
            )
            .await?;
            continue;
        }
        let response = handle_request(
            request,
            args,
            state_dir,
            supervisor,
            endpoints,
            cleanup_directory,
        )
        .await;
        write_frame(control, &response).await?;
    }
}

async fn handle_request(
    request: ClientRequest,
    args: &Args,
    state_dir: &std::path::Path,
    supervisor: &mut Option<ActiveSupervisor>,
    endpoints: &mut PassedStreamEndpoints,
    cleanup_directory: &mut Option<CleanupDirectory>,
) -> ServerResponse {
    let id = request.id();
    match request {
        ClientRequest::Discover { .. } => ServerResponse::Capabilities {
            id,
            capabilities: capabilities(state_dir, super::SOURCE_REVISION),
        },
        ClientRequest::SetupStatus { setup, .. } => {
            if supervisor.is_some() || cleanup_directory.is_none() {
                return error_response(
                    Some(id),
                    ErrorCode::InvalidState,
                    ErrorPhase::Protocol,
                    "setup operations must precede target launch".to_string(),
                    TargetState::Started,
                );
            }
            if let Err(error) = validate_setup_support(&setup) {
                return error_response(
                    Some(id),
                    ErrorCode::UnsupportedPolicy,
                    ErrorPhase::Validation,
                    error.to_string(),
                    TargetState::NotStarted,
                );
            }
            ServerResponse::SetupStatus {
                id,
                setup: setup_status(state_dir),
            }
        }
        ClientRequest::Setup { setup, .. } => {
            if supervisor.is_some() || cleanup_directory.is_none() {
                return error_response(
                    Some(id),
                    ErrorCode::InvalidState,
                    ErrorPhase::Protocol,
                    "setup operations must precede target launch".to_string(),
                    TargetState::Started,
                );
            }
            if let Err(error) = validate_setup_support(&setup) {
                return error_response(
                    Some(id),
                    ErrorCode::UnsupportedPolicy,
                    ErrorPhase::Validation,
                    error.to_string(),
                    TargetState::NotStarted,
                );
            }
            #[cfg(unix)]
            {
                ServerResponse::SetupCompleted {
                    id,
                    operation: SetupCompletedOperation::AlreadyReady,
                }
            }
            #[cfg(windows)]
            {
                error_response(
                    Some(id),
                    ErrorCode::UnsupportedPlatform,
                    ErrorPhase::Setup,
                    "Windows sandbox setup is deferred in this release".to_string(),
                    TargetState::NotStarted,
                )
            }
        }
        ClientRequest::Launch { launch, .. } => {
            if supervisor.is_some() || cleanup_directory.is_none() {
                return error_response(
                    Some(id),
                    ErrorCode::InvalidState,
                    ErrorPhase::Protocol,
                    "one runner process owns at most one target generation".to_string(),
                    TargetState::Started,
                );
            }
            #[cfg(windows)]
            {
                let _ = (launch, args, state_dir, supervisor, endpoints);
                return error_response(
                    Some(id),
                    ErrorCode::UnsupportedPlatform,
                    ErrorPhase::Validation,
                    "Windows sandbox launch is deferred in this release".to_string(),
                    TargetState::NotStarted,
                );
            }
            #[cfg(unix)]
            {
                if let Err(error) = validate_launch_support(&launch) {
                    return error_response(
                        Some(id),
                        ErrorCode::UnsupportedPolicy,
                        ErrorPhase::Validation,
                        error.to_string(),
                        TargetState::NotStarted,
                    );
                }
                if let Err(error) = backend_availability(state_dir) {
                    let code = match error {
                        BackendAvailabilityError::Companion(_) => ErrorCode::CompanionMissing,
                        BackendAvailabilityError::Backend(_) => ErrorCode::BackendUnavailable,
                    };
                    return error_response(
                        Some(id),
                        code,
                        ErrorPhase::Validation,
                        error.to_string(),
                        TargetState::NotStarted,
                    );
                }
                let target_executable = match absolute_target(&args.target) {
                    Ok((program, _)) => std::path::Path::new(program),
                    Err(error) => {
                        return error_response(
                            Some(id),
                            ErrorCode::InvalidRequest,
                            ErrorPhase::Validation,
                            error.to_string(),
                            TargetState::NotStarted,
                        );
                    }
                };
                if let Err(error) = endpoints.validate_request(&launch.streams) {
                    return error_response(
                        Some(id),
                        ErrorCode::InvalidRequest,
                        ErrorPhase::Validation,
                        error.to_string(),
                        TargetState::NotStarted,
                    );
                }
                let policy = match validate_launch(&launch, state_dir, target_executable) {
                    Ok(policy) => policy,
                    Err(error) => {
                        return error_response(
                            Some(id),
                            ErrorCode::InvalidRequest,
                            ErrorPhase::Validation,
                            error.to_string(),
                            TargetState::NotStarted,
                        );
                    }
                };
                let mut environment = match target_environment() {
                    Ok(environment) => environment,
                    Err(error) => {
                        return error_response(
                            Some(id),
                            ErrorCode::InvalidRequest,
                            ErrorPhase::Validation,
                            error.to_string(),
                            TargetState::NotStarted,
                        );
                    }
                };
                let mut network = match prepare_network(&launch.network, &mut environment).await {
                    Ok(network) => network,
                    Err(error) => {
                        return error_response(
                            Some(id),
                            ErrorCode::BackendUnavailable,
                            ErrorPhase::ProxyStartup,
                            error.to_string(),
                            TargetState::NotStarted,
                        );
                    }
                };
                let started = start(
                    &args.target,
                    &launch,
                    &policy,
                    &mut network,
                    &environment,
                    state_dir,
                    args.control_fd,
                    endpoints,
                    cleanup_directory,
                )
                .await;
                let started = match started {
                    Ok(started) => started,
                    Err(error) => {
                        let (code, phase, message, failed_supervisor) = match error {
                            StartError::Preparation(source) => (
                                ErrorCode::BackendUnavailable,
                                ErrorPhase::SandboxPreparation,
                                format!("{source:#}"),
                                None,
                            ),
                            StartError::Launch { source, supervisor } => (
                                ErrorCode::LaunchFailed,
                                ErrorPhase::Launch,
                                format!("{source:#}"),
                                supervisor,
                            ),
                        };
                        let target_state = TargetState::from(
                            failed_supervisor.is_some() || cleanup_directory.is_none(),
                        );
                        if let Some(failed_supervisor) = failed_supervisor {
                            *supervisor = Some(failed_supervisor);
                        }
                        if let Some(handle) = network.handle.take() {
                            let _ = handle.shutdown().await;
                        }
                        return error_response(Some(id), code, phase, message, target_state);
                    }
                };
                *supervisor = Some(started.supervisor);
                ServerResponse::LaunchAccepted {
                    id,
                    backend: native_backend(),
                    root_process_id: started.root_process_id,
                }
            }
        }
        ClientRequest::Status { .. } => {
            #[cfg(unix)]
            let status = supervisor.as_ref().map_or_else(
                || {
                    if cleanup_directory.is_none() {
                        failed_status()
                    } else {
                        idle_status()
                    }
                },
                Supervisor::status,
            );
            #[cfg(windows)]
            let status = idle_status();
            ServerResponse::Status { id, status }
        }
        ClientRequest::Interrupt { .. } => {
            #[cfg(windows)]
            return invalid_state(id, cleanup_directory.is_none());
            #[cfg(unix)]
            match supervisor.as_ref() {
                None => invalid_state(id, cleanup_directory.is_none()),
                Some(_) if cfg!(target_os = "linux") => error_response(
                    Some(id),
                    ErrorCode::UnsupportedPolicy,
                    ErrorPhase::Running,
                    "interrupt cannot cross this release's Linux bubblewrap session boundary"
                        .to_string(),
                    TargetState::Started,
                ),
                Some(supervisor) => match supervisor.interrupt() {
                    Ok(()) => ServerResponse::Acknowledged {
                        id,
                        operation: AcknowledgedOperation::Interrupt,
                    },
                    Err(error) => error_response(
                        Some(id),
                        ErrorCode::InvalidState,
                        ErrorPhase::Running,
                        error.to_string(),
                        TargetState::Started,
                    ),
                },
            }
        }
        ClientRequest::Terminate { deadlines, .. } => {
            #[cfg(windows)]
            return invalid_state(id, cleanup_directory.is_none());
            #[cfg(unix)]
            match supervisor.as_ref() {
                None => invalid_state(id, cleanup_directory.is_none()),
                Some(supervisor) => {
                    if deadlines.graceful_ms > MAX_CONTROL_WAIT_MS
                        || deadlines.force_ms > MAX_CONTROL_WAIT_MS
                    {
                        error_response(
                            Some(id),
                            ErrorCode::InvalidRequest,
                            ErrorPhase::Validation,
                            format!(
                                "termination deadlines must not exceed {MAX_CONTROL_WAIT_MS} ms"
                            ),
                            TargetState::Started,
                        )
                    } else if cfg!(target_os = "linux") && deadlines.graceful_ms != 0 {
                        error_response(
                            Some(id),
                            ErrorCode::UnsupportedPolicy,
                            ErrorPhase::Running,
                            "graceful termination is unsupported by the selected native backend"
                                .to_string(),
                            TargetState::Started,
                        )
                    } else {
                        match supervisor.terminate(deadlines) {
                            Ok(()) => ServerResponse::Acknowledged {
                                id,
                                operation: AcknowledgedOperation::Terminate,
                            },
                            Err(error) => error_response(
                                Some(id),
                                ErrorCode::InvalidState,
                                ErrorPhase::Running,
                                error.to_string(),
                                TargetState::Started,
                            ),
                        }
                    }
                }
            }
        }
        ClientRequest::Wait {
            retirement_timeout_ms,
            ..
        } => {
            #[cfg(windows)]
            return invalid_state(id, cleanup_directory.is_none());
            #[cfg(unix)]
            match supervisor.as_ref() {
                None => invalid_state(id, cleanup_directory.is_none()),
                Some(_) if retirement_timeout_ms > MAX_CONTROL_WAIT_MS => error_response(
                    Some(id),
                    ErrorCode::InvalidRequest,
                    ErrorPhase::Validation,
                    format!("wait timeout must not exceed {MAX_CONTROL_WAIT_MS} ms"),
                    TargetState::Started,
                ),
                Some(supervisor) => match supervisor
                    .wait(Duration::from_millis(retirement_timeout_ms))
                    .await
                {
                    Ok(outcome) => ServerResponse::Final { id, outcome },
                    Err(error) => error_response(
                        Some(id),
                        ErrorCode::CleanupFailed,
                        ErrorPhase::Retirement,
                        error.to_string(),
                        TargetState::Started,
                    ),
                },
            }
        }
    }
}

fn idle_status() -> RunnerStatus {
    RunnerStatus {
        phase: RunnerPhase::Idle,
        target: None,
        retirement: None,
    }
}

fn failed_status() -> RunnerStatus {
    RunnerStatus {
        phase: RunnerPhase::Failed,
        target: None,
        retirement: None,
    }
}

fn generation_state(
    supervisor: &Option<ActiveSupervisor>,
    cleanup_directory: &Option<CleanupDirectory>,
) -> TargetState {
    TargetState::from(supervisor.is_some() || cleanup_directory.is_none())
}

fn invalid_state(id: u64, generation_consumed: bool) -> ServerResponse {
    error_response(
        Some(id),
        ErrorCode::InvalidState,
        ErrorPhase::Protocol,
        if generation_consumed {
            "target generation launch failed"
        } else {
            "no target generation has been launched"
        }
        .to_string(),
        generation_consumed.into(),
    )
}

#[derive(Clone, Copy)]
enum TargetState {
    NotStarted,
    Started,
}

impl From<bool> for TargetState {
    fn from(started: bool) -> Self {
        if started {
            Self::Started
        } else {
            Self::NotStarted
        }
    }
}

fn frame_error_code(error: &FrameError) -> ErrorCode {
    match error {
        FrameError::MalformedJson(_) => ErrorCode::MalformedJson,
        FrameError::Closed
        | FrameError::TruncatedLength
        | FrameError::Oversized { .. }
        | FrameError::TruncatedPayload { .. }
        | FrameError::Io(_) => ErrorCode::MalformedFrame,
    }
}

fn error_response(
    id: Option<u64>,
    code: ErrorCode,
    phase: ErrorPhase,
    message: String,
    target_state: TargetState,
) -> ServerResponse {
    ServerResponse::Error {
        id,
        error: ProtocolError {
            code,
            phase,
            message,
            target_started: matches!(target_state, TargetState::Started),
        },
    }
}
