use clap::Parser;
use codex_mcp_console_sandbox::PROTOCOL_VERSION;
use codex_mcp_console_sandbox::capabilities::backend_availability;
use codex_mcp_console_sandbox::capabilities::capabilities;
use codex_mcp_console_sandbox::capabilities::native_backend;
#[cfg(unix)]
use codex_mcp_console_sandbox::environment::target_environment;
use codex_mcp_console_sandbox::framing::FrameError;
use codex_mcp_console_sandbox::framing::read_frame;
use codex_mcp_console_sandbox::framing::write_frame;
use codex_mcp_console_sandbox::launch::StartError;
use codex_mcp_console_sandbox::launch::start;
#[cfg(unix)]
use codex_mcp_console_sandbox::network::prepare_network;
use codex_mcp_console_sandbox::policy::absolute_target;
use codex_mcp_console_sandbox::policy::validate_launch;
use codex_mcp_console_sandbox::policy::validate_launch_support;
use codex_mcp_console_sandbox::policy::validate_setup_support;
use codex_mcp_console_sandbox::protocol::AcknowledgedOperation;
use codex_mcp_console_sandbox::protocol::ClientRequest;
use codex_mcp_console_sandbox::protocol::ErrorCode;
use codex_mcp_console_sandbox::protocol::ErrorPhase;
use codex_mcp_console_sandbox::protocol::ProtocolError;
use codex_mcp_console_sandbox::protocol::RunnerPhase;
use codex_mcp_console_sandbox::protocol::RunnerStatus;
use codex_mcp_console_sandbox::protocol::ServerResponse;
#[cfg(windows)]
use codex_mcp_console_sandbox::protocol::SetupRequest;
use codex_mcp_console_sandbox::setup::SetupSession;
use codex_mcp_console_sandbox::stdio::PassedStreamEndpoints;
use codex_mcp_console_sandbox::supervisor::Supervisor;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

const MAX_CONTROL_WAIT_MS: u64 = 5 * 60 * 1000;
const SOURCE_REVISION: &str = match option_env!("STABLE_GIT_COMMIT") {
    Some(revision) => revision,
    None => env!("MCP_CONSOLE_SANDBOX_SOURCE_REVISION"),
};
const _: () = {
    let revision = SOURCE_REVISION.as_bytes();
    assert!(
        revision.len() == 40,
        "Codex source revision must be a full Git SHA"
    );
    let mut index = 0;
    while index < revision.len() {
        assert!(
            revision[index].is_ascii_hexdigit(),
            "Codex source revision must contain only hexadecimal digits"
        );
        index += 1;
    }
};

#[derive(Debug, Parser)]
#[command(name = "mcp-console-sandbox")]
struct Args {
    #[arg(long)]
    state_dir: PathBuf,
    #[cfg(unix)]
    #[arg(long)]
    control_fd: i32,
    #[cfg(unix)]
    #[arg(long = "stream-fd")]
    stream_fds: Vec<u64>,
    #[cfg(windows)]
    #[arg(long)]
    control_handle: u64,
    #[cfg(windows)]
    #[arg(long = "stream-handle")]
    stream_handles: Vec<u64>,
    #[arg(last = true)]
    target: Vec<OsString>,
}

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(|arg0| std::path::Path::new(arg0).file_name())
        == Some(std::ffi::OsStr::new("codex-linux-sandbox"))
    {
        codex_linux_sandbox::run_main();
    }

    if codex_mcp_console_sandbox::watchdog::dispatch_if_requested() {
        return;
    }

    #[cfg(unix)]
    if codex_mcp_console_sandbox::launch_bridge::dispatch_if_requested() {
        return;
    }

    let args = Args::parse();
    #[cfg(unix)]
    let passed_stream_endpoints = PassedStreamEndpoints::claim(&args.stream_fds, args.control_fd);
    #[cfg(windows)]
    let passed_stream_endpoints =
        PassedStreamEndpoints::claim(&args.stream_handles, args.control_handle);
    #[cfg(not(any(unix, windows)))]
    let passed_stream_endpoints = PassedStreamEndpoints::claim(&[], 0);
    let passed_stream_endpoints = match passed_stream_endpoints {
        Ok(endpoints) => endpoints,
        Err(error) => {
            eprintln!(
                "mcp-console-sandbox infrastructure error: invalid native bootstrap endpoint: {error}"
            );
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("mcp-console-sandbox infrastructure error: could not build runtime: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = runtime.block_on(run(args, passed_stream_endpoints)) {
        eprintln!("mcp-console-sandbox infrastructure error: {error}");
        std::process::exit(2);
    }
}

async fn run(args: Args, mut passed_stream_endpoints: PassedStreamEndpoints) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.state_dir.is_absolute(),
        "application state directory must be absolute"
    );
    anyhow::ensure!(
        args.state_dir.to_str().is_some(),
        "application state directory must be valid Unicode"
    );
    let runner_executable = std::env::current_exe()?.canonicalize()?;
    anyhow::ensure!(
        runner_executable.to_str().is_some(),
        "runner executable and companion-resource paths must be valid Unicode"
    );
    std::fs::create_dir_all(&args.state_dir)?;
    let state_dir = args.state_dir.canonicalize()?;
    anyhow::ensure!(
        state_dir.to_str().is_some(),
        "canonical application state directory must be valid Unicode"
    );
    let mut control = control_channel(&args)?;
    let mut supervisor = None;
    let mut generation_started = false;
    let mut setup_session = SetupSession::default();
    let loop_result = control_loop(
        &args,
        &state_dir,
        &mut control,
        &mut supervisor,
        &mut generation_started,
        &mut setup_session,
        &mut passed_stream_endpoints,
    )
    .await;
    if let Some(supervisor) = supervisor {
        supervisor.retire_on_control_loss().await?;
    }
    setup_session.shutdown().await?;
    loop_result
}

async fn control_loop(
    args: &Args,
    state_dir: &std::path::Path,
    control: &mut tokio::fs::File,
    supervisor: &mut Option<Supervisor>,
    generation_started: &mut bool,
    setup_session: &mut SetupSession,
    passed_stream_endpoints: &mut PassedStreamEndpoints,
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
                    supervisor.is_some(),
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
                    supervisor.is_some(),
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
            generation_started,
            setup_session,
            passed_stream_endpoints,
        )
        .await;
        write_frame(control, &response).await?;
    }
}

async fn handle_request(
    request: ClientRequest,
    args: &Args,
    state_dir: &std::path::Path,
    supervisor: &mut Option<Supervisor>,
    generation_started: &mut bool,
    setup_session: &mut SetupSession,
    passed_stream_endpoints: &mut PassedStreamEndpoints,
) -> ServerResponse {
    let id = request.id();
    match request {
        ClientRequest::Discover { .. } => ServerResponse::Capabilities {
            id,
            capabilities: capabilities(state_dir, SOURCE_REVISION),
        },
        ClientRequest::SetupStatus { setup, .. } => {
            if *generation_started {
                return error_response(
                    Some(id),
                    ErrorCode::InvalidState,
                    ErrorPhase::Protocol,
                    "setup operations must precede target launch".to_string(),
                    /*target_started*/ true,
                );
            }
            if let Err(error) = validate_setup_support(&setup) {
                return error_response(
                    Some(id),
                    ErrorCode::UnsupportedPolicy,
                    ErrorPhase::Validation,
                    error.to_string(),
                    /*target_started*/ false,
                );
            }
            match setup_session.inspect(*setup, state_dir).await {
                Ok(setup) => ServerResponse::SetupStatus { id, setup },
                Err(error) => error_response(
                    Some(id),
                    ErrorCode::SetupFailed,
                    ErrorPhase::Setup,
                    error.to_string(),
                    /*target_started*/ false,
                ),
            }
        }
        ClientRequest::Setup {
            operation, setup, ..
        } => {
            if *generation_started {
                return error_response(
                    Some(id),
                    ErrorCode::InvalidState,
                    ErrorPhase::Protocol,
                    "setup operations must precede target launch".to_string(),
                    /*target_started*/ true,
                );
            }
            if let Err(error) = validate_setup_support(&setup) {
                return error_response(
                    Some(id),
                    ErrorCode::UnsupportedPolicy,
                    ErrorPhase::Validation,
                    error.to_string(),
                    /*target_started*/ false,
                );
            }
            match setup_session.apply(operation, *setup, state_dir).await {
                Ok(operation) => ServerResponse::SetupCompleted { id, operation },
                Err(error) => error_response(
                    Some(id),
                    if cfg!(windows) {
                        ErrorCode::SetupFailed
                    } else {
                        ErrorCode::UnsupportedPlatform
                    },
                    ErrorPhase::Setup,
                    error.to_string(),
                    /*target_started*/ false,
                ),
            }
        }
        ClientRequest::Launch { launch, .. } => {
            if *generation_started {
                return error_response(
                    Some(id),
                    ErrorCode::InvalidState,
                    ErrorPhase::Protocol,
                    "one runner process owns at most one target generation".to_string(),
                    /*target_started*/ true,
                );
            }
            if let Err(error) = validate_launch_support(&launch) {
                return error_response(
                    Some(id),
                    ErrorCode::UnsupportedPolicy,
                    ErrorPhase::Validation,
                    error.to_string(),
                    /*target_started*/ false,
                );
            }
            if let Err(error) = backend_availability(state_dir) {
                return error_response(
                    Some(id),
                    if cfg!(any(target_os = "linux", target_os = "windows")) {
                        ErrorCode::CompanionMissing
                    } else {
                        ErrorCode::BackendUnavailable
                    },
                    ErrorPhase::Validation,
                    error,
                    /*target_started*/ false,
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
                        /*target_started*/ false,
                    );
                }
            };
            if let Err(error) = passed_stream_endpoints.validate_request(&launch.streams) {
                return error_response(
                    Some(id),
                    ErrorCode::InvalidRequest,
                    ErrorPhase::Validation,
                    error.to_string(),
                    /*target_started*/ false,
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
                        /*target_started*/ false,
                    );
                }
            };
            #[cfg(unix)]
            let (environment, mut network) = {
                let mut environment = match target_environment() {
                    Ok(environment) => environment,
                    Err(error) => {
                        return error_response(
                            Some(id),
                            ErrorCode::InvalidRequest,
                            ErrorPhase::Validation,
                            error.to_string(),
                            /*target_started*/ false,
                        );
                    }
                };
                let network = match prepare_network(&launch.network, &mut environment).await {
                    Ok(network) => network,
                    Err(error) => {
                        return error_response(
                            Some(id),
                            ErrorCode::BackendUnavailable,
                            ErrorPhase::ProxyStartup,
                            error.to_string(),
                            /*target_started*/ false,
                        );
                    }
                };
                (environment, network)
            };
            #[cfg(windows)]
            let (windows_setup, environment, mut network) = {
                let setup = match setup_session
                    .take_for_launch(SetupRequest::from(launch.as_ref()), &policy, state_dir)
                    .await
                {
                    Ok(setup) => setup,
                    Err(error) => {
                        return error_response(
                            Some(id),
                            ErrorCode::SetupFailed,
                            ErrorPhase::Setup,
                            error.to_string(),
                            /*target_started*/ false,
                        );
                    }
                };
                (setup.native, setup.environment, setup.network)
            };
            #[cfg(unix)]
            let started = start(
                &args.target,
                &launch,
                &policy,
                &mut network,
                &environment,
                state_dir,
                args.control_fd,
                passed_stream_endpoints,
            )
            .await;
            #[cfg(windows)]
            let started = {
                start(
                    &args.target,
                    &launch,
                    windows_setup,
                    &mut network,
                    &environment,
                    args.control_handle,
                    passed_stream_endpoints,
                )
            };
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
                        StartError::Launch {
                            source,
                            supervisor: failed_supervisor,
                        } => (
                            ErrorCode::LaunchFailed,
                            ErrorPhase::Launch,
                            format!("{source:#}"),
                            failed_supervisor,
                        ),
                    };
                    let target_started = failed_supervisor.is_some();
                    if let Some(failed_supervisor) = failed_supervisor {
                        *generation_started = true;
                        *supervisor = Some(failed_supervisor);
                    }
                    if let Some(handle) = network.handle.take() {
                        let _ = handle.shutdown().await;
                    }
                    return error_response(Some(id), code, phase, message, target_started);
                }
            };
            *generation_started = true;
            *supervisor = Some(started.supervisor);
            ServerResponse::LaunchAccepted {
                id,
                backend: native_backend(),
                root_process_id: started.root_process_id,
            }
        }
        ClientRequest::Status { .. } => ServerResponse::Status {
            id,
            status: supervisor.as_ref().map_or(
                RunnerStatus {
                    phase: RunnerPhase::Idle,
                    target: None,
                    retirement: None,
                },
                Supervisor::status,
            ),
        },
        ClientRequest::Interrupt { .. } => match supervisor.as_ref() {
            None => invalid_state(id),
            Some(_) if cfg!(target_os = "linux") => error_response(
                Some(id),
                ErrorCode::UnsupportedPolicy,
                ErrorPhase::Running,
                "interrupt cannot cross this release's Linux bubblewrap session boundary"
                    .to_string(),
                /*target_started*/ true,
            ),
            Some(_) if cfg!(windows) => error_response(
                Some(id),
                ErrorCode::UnsupportedPolicy,
                ErrorPhase::Running,
                "interrupt is unsupported by this release's Windows sandbox backend".to_string(),
                /*target_started*/ true,
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
                    /*target_started*/ true,
                ),
            },
        },
        ClientRequest::Terminate { deadlines, .. } => match supervisor.as_ref() {
            Some(supervisor) => {
                if deadlines.graceful_ms > MAX_CONTROL_WAIT_MS
                    || deadlines.force_ms > MAX_CONTROL_WAIT_MS
                {
                    error_response(
                        Some(id),
                        ErrorCode::InvalidRequest,
                        ErrorPhase::Validation,
                        format!("termination deadlines must not exceed {MAX_CONTROL_WAIT_MS} ms"),
                        /*target_started*/ true,
                    )
                } else if cfg!(any(target_os = "linux", windows)) && deadlines.graceful_ms != 0 {
                    error_response(
                        Some(id),
                        ErrorCode::UnsupportedPolicy,
                        ErrorPhase::Running,
                        "graceful termination is unsupported by the selected native backend"
                            .to_string(),
                        /*target_started*/ true,
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
                            /*target_started*/ true,
                        ),
                    }
                }
            }
            None => invalid_state(id),
        },
        ClientRequest::Wait {
            retirement_timeout_ms,
            ..
        } => match supervisor.as_ref() {
            Some(supervisor) => {
                if retirement_timeout_ms > MAX_CONTROL_WAIT_MS {
                    error_response(
                        Some(id),
                        ErrorCode::InvalidRequest,
                        ErrorPhase::Validation,
                        format!("wait timeout must not exceed {MAX_CONTROL_WAIT_MS} ms"),
                        /*target_started*/ true,
                    )
                } else {
                    match supervisor
                        .wait(Duration::from_millis(retirement_timeout_ms))
                        .await
                    {
                        Ok(outcome) => ServerResponse::Final { id, outcome },
                        Err(error) => error_response(
                            Some(id),
                            ErrorCode::CleanupFailed,
                            ErrorPhase::Retirement,
                            error.to_string(),
                            /*target_started*/ true,
                        ),
                    }
                }
            }
            None => invalid_state(id),
        },
    }
}

fn invalid_state(id: u64) -> ServerResponse {
    error_response(
        Some(id),
        ErrorCode::InvalidState,
        ErrorPhase::Protocol,
        "no target generation has been launched".to_string(),
        /*target_started*/ false,
    )
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
    target_started: bool,
) -> ServerResponse {
    ServerResponse::Error {
        id,
        error: ProtocolError {
            code,
            phase,
            message,
            target_started,
        },
    }
}

#[cfg(unix)]
fn control_channel(args: &Args) -> std::io::Result<tokio::fs::File> {
    use std::os::fd::FromRawFd;

    let flags = unsafe { libc::fcntl(args.control_fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(args.control_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(args.control_fd) };
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(windows)]
fn control_channel(args: &Args) -> std::io::Result<tokio::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::SetHandleInformation;

    let handle_value = usize::try_from(args.control_handle).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private control handle exceeds the native handle width",
        )
    })?;
    let handle = isize::from_ne_bytes(handle_value.to_ne_bytes());
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_handle(handle_value as *mut std::ffi::c_void) };
    Ok(tokio::fs::File::from_std(file))
}
