use super::STANDALONE_HELPER_SWITCH;
use super::WindowsSandboxStandaloneLaunchRequest;
use super::WindowsSandboxStandaloneOutcome;
use super::WindowsSandboxStandaloneRetirementOutcome;
use super::WindowsSandboxStandaloneRootOutcome;
use super::WindowsSandboxStandaloneStream;
use super::setup::WindowsSandboxStandaloneNetworkIdentity;
use super::setup::WindowsSandboxStandaloneSetupRequest;
use super::setup::native_network_identity;
use super::setup::refresh_windows_sandbox_standalone_with_policy_lease;
use super::setup::validate_setup_request;
use super::setup::verify_windows_sandbox_standalone_network_with_policy_lease;
use super::setup::windows_sandbox_standalone_setup_status;
use super::wire::HelperMessage;
use super::wire::ParentMessage;
use super::wire::WireNativeString;
use super::wire::WireSpawnRequest;
use super::wire::WireStream;
use super::wire::WireTokenMode;
use super::wire::read_wire_frame;
use super::wire::write_wire_frame;
use crate::identity::SandboxCreds;
use crate::identity::load_prepared_sandbox_creds;
use crate::runner_client::connect_pipe_with_timeout;
use crate::runner_pipe::PIPE_ACCESS_INBOUND;
use crate::runner_pipe::PIPE_ACCESS_OUTBOUND;
use crate::runner_pipe::create_named_pipe;
use crate::runner_pipe::pipe_pair;
use crate::winutil::native_argv_to_command_line;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use std::ffi::OsString;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessWithLogonW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const RUNNER_ERROR_MODE_FLAGS: u32 = 0x0001 | 0x0002;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

fn duration_millis(duration: Duration, label: &str) -> Result<u64> {
    let millis = u64::try_from(duration.as_millis())
        .with_context(|| format!("{label} exceeds the Windows helper duration range"))?;
    if millis > u32::MAX as u64 {
        anyhow::bail!("{label} exceeds the Windows wait range");
    }
    Ok(millis)
}

fn capability_sids(request: &WindowsSandboxStandaloneSetupRequest) -> Result<Vec<String>> {
    if request.write_roots.is_empty() {
        return Ok(vec![
            crate::cap::load_or_create_cap_sids(&request.state_dir)?.readonly,
        ]);
    }
    let mut roots = request.write_roots.clone();
    roots.sort();
    roots.dedup();
    let sids = roots
        .iter()
        .map(|root| {
            crate::cap::workspace_write_cap_sid_for_root(
                &request.state_dir,
                &request.command_cwd,
                root,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    if sids.is_empty() {
        anyhow::bail!("writable standalone policy produced no capability SIDs");
    }
    Ok(sids)
}

fn spawn_request_wire(
    request: &WindowsSandboxStandaloneLaunchRequest<'_>,
    helper_process: HANDLE,
    capability_sids: Vec<String>,
) -> Result<WireSpawnRequest> {
    let mut args = Vec::with_capacity(request.command.args.len());
    for argument in &request.command.args {
        args.push(WireNativeString::from_os_str(argument)?);
    }
    let mut environment = Vec::with_capacity(request.command.environment.len());
    for (key, value) in &request.command.environment {
        environment.push((
            WireNativeString::from_os_str(key)?,
            WireNativeString::from_os_str(value)?,
        ));
    }
    Ok(WireSpawnRequest {
        program: WireNativeString::from_os_str(request.command.program.as_os_str())?,
        args,
        environment,
        cwd: WireNativeString::from_os_str(request.command.cwd.as_os_str())?,
        state_dir: WireNativeString::from_os_str(request.setup.state_dir.as_os_str())?,
        token_mode: if request.setup.write_roots.is_empty() {
            WireTokenMode::ReadOnly
        } else {
            WireTokenMode::WritableRoots
        },
        capability_sids,
        network_proxy_restricting_sid: request.network_proxy_restricting_sid.clone(),
        stdin: duplicate_stream(
            &request.stdio.stdin,
            STD_INPUT_HANDLE,
            helper_process,
            "stdin",
        )?,
        stdout: duplicate_stream(
            &request.stdio.stdout,
            STD_OUTPUT_HANDLE,
            helper_process,
            "stdout",
        )?,
        stderr: duplicate_stream(
            &request.stdio.stderr,
            STD_ERROR_HANDLE,
            helper_process,
            "stderr",
        )?,
        use_private_desktop: request.use_private_desktop,
        descendant_grace_ms: duration_millis(request.descendant_grace, "descendant grace")?,
        force_stop_timeout_ms: duration_millis(request.force_stop_timeout, "force-stop timeout")?,
    })
}

fn duplicate_stream(
    stream: &WindowsSandboxStandaloneStream<'_>,
    inherited_id: u32,
    helper_process: HANDLE,
    label: &str,
) -> Result<WireStream> {
    let source = match stream {
        WindowsSandboxStandaloneStream::Inherited => unsafe { GetStdHandle(inherited_id) },
        WindowsSandboxStandaloneStream::Passed(handle) => handle.as_raw_handle() as HANDLE,
        WindowsSandboxStandaloneStream::Null => return Ok(WireStream::Null),
    };
    if source == 0 || source == INVALID_HANDLE_VALUE {
        anyhow::bail!("standalone {label} source handle is unavailable");
    }
    let mut duplicate = 0;
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            helper_process,
            &mut duplicate,
            /*dw_desired_access*/ 0,
            /*b_inherit_handle*/ 0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if duplicated == 0 {
        anyhow::bail!(
            "DuplicateHandle failed for standalone {label}: {}",
            unsafe { GetLastError() }
        );
    }
    Ok(WireStream::Handle(duplicate as usize as u64))
}

fn minimal_helper_environment(state_dir: &Path) -> Result<Vec<u16>> {
    let mut environment = Vec::new();
    for key in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            environment.push((OsString::from(key), value));
        }
    }
    let temp = state_dir.join(".sandbox");
    environment.push((OsString::from("TEMP"), temp.as_os_str().to_owned()));
    environment.push((OsString::from("TMP"), temp.as_os_str().to_owned()));
    crate::process::make_native_env_block(&environment)
}

fn spawn_helper(
    request: &WindowsSandboxStandaloneSetupRequest,
    creds: &SandboxCreds,
) -> Result<(PROCESS_INFORMATION, File, File)> {
    let (pipe_in_name, pipe_out_name) = pipe_pair();
    let input_pipe = create_named_pipe(&pipe_in_name, PIPE_ACCESS_OUTBOUND, &creds.username)?;
    let output_pipe = create_named_pipe(&pipe_out_name, PIPE_ACCESS_INBOUND, &creds.username)?;
    let argv = vec![
        request
            .resources
            .command_runner_executable
            .as_os_str()
            .to_owned(),
        OsString::from(STANDALONE_HELPER_SWITCH),
        OsString::from(format!("--pipe-in={pipe_in_name}")),
        OsString::from(format!("--pipe-out={pipe_out_name}")),
    ];
    let mut command_line = native_argv_to_command_line(&argv)?;
    let executable = to_wide(&request.resources.command_runner_executable);
    let cwd = to_wide(&request.state_dir);
    let username = to_wide(&creds.username);
    let domain = to_wide(".");
    let password = to_wide(&creds.password);
    let environment = minimal_helper_environment(&request.state_dir)?;
    let mut startup_info: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let previous_error_mode = unsafe { SetErrorMode(RUNNER_ERROR_MODE_FLAGS) };
    let created = unsafe {
        CreateProcessWithLogonW(
            username.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            /*dwlogonflags*/ 0,
            executable.as_ptr(),
            command_line.as_mut_ptr(),
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup_info,
            &mut process_info,
        )
    };
    unsafe {
        SetErrorMode(previous_error_mode);
    }
    if created == 0 {
        let error = unsafe { GetLastError() };
        unsafe {
            CloseHandle(input_pipe);
            CloseHandle(output_pipe);
        }
        anyhow::bail!("CreateProcessWithLogonW failed for standalone helper: {error}");
    }

    let connect_result = (|| -> Result<()> {
        connect_pipe_with_timeout(input_pipe, process_info.dwProcessId, "standalone pipe-in")?;
        connect_pipe_with_timeout(output_pipe, process_info.dwProcessId, "standalone pipe-out")?;
        Ok(())
    })();
    unsafe {
        CloseHandle(process_info.hThread);
        process_info.hThread = 0;
    }
    if let Err(error) = connect_result {
        unsafe {
            TerminateProcess(process_info.hProcess, 1);
            CloseHandle(process_info.hProcess);
            CloseHandle(input_pipe);
            CloseHandle(output_pipe);
        }
        return Err(error);
    }
    Ok((
        process_info,
        unsafe { File::from_raw_handle(input_pipe as _) },
        unsafe { File::from_raw_handle(output_pipe as _) },
    ))
}

#[derive(Default)]
struct Observation {
    ready: Option<Result<u32, String>>,
    target: Option<WindowsSandboxStandaloneRootOutcome>,
    final_outcome: Option<WindowsSandboxStandaloneOutcome>,
    committed: bool,
}

#[derive(Default)]
struct ObservationState {
    observation: Mutex<Observation>,
    changed: Condvar,
}

struct StandaloneProcessInner {
    control: Mutex<Option<File>>,
    helper: OwnedHandle,
    commit_sent: AtomicBool,
    policy_lease: Mutex<Option<crate::policy_lease::McpConsoleSandboxPolicyLease>>,
}

impl Drop for StandaloneProcessInner {
    fn drop(&mut self) {
        self.fail_safe_retire();
    }
}

impl StandaloneProcessInner {
    fn fail_safe_retire(&self) {
        if let Ok(mut control) = self.control.lock() {
            control.take();
        }
        let helper = self.helper.as_raw_handle() as HANDLE;
        if unsafe { WaitForSingleObject(helper, 5_000) } == WAIT_TIMEOUT {
            unsafe {
                let _ = TerminateProcess(helper, 1);
                let _ = WaitForSingleObject(helper, 5_000);
            }
        }
    }
}

/// Controller for one standalone Windows target generation.
#[derive(Clone)]
pub struct WindowsSandboxStandaloneProcess {
    inner: Arc<StandaloneProcessInner>,
    observation: Arc<ObservationState>,
}

impl std::fmt::Debug for WindowsSandboxStandaloneProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsSandboxStandaloneProcess")
            .finish_non_exhaustive()
    }
}

impl WindowsSandboxStandaloneProcess {
    /// Releases exclusive ownership of the shared Windows policy state.
    ///
    /// Embedding supervisors call this only after full native retirement and
    /// proxy cleanup, immediately before publishing the final outcome.
    #[doc(hidden)]
    pub fn release_policy_lease(&self) {
        self.inner
            .policy_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Waits until the helper confirms that the target exists suspended in the
    /// non-breakaway Job Object. Failure still consumes this launch attempt.
    pub fn wait_ready(&self) -> Result<u32> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut observation = self
            .observation
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while observation.ready.is_none() {
            let now = Instant::now();
            if now >= deadline {
                let message = "timed out waiting for standalone helper startup".to_string();
                observation.ready = Some(Err(message.clone()));
                self.observation.changed.notify_all();
                drop(observation);
                self.inner.fail_safe_retire();
                anyhow::bail!(message);
            }
            let (current, timeout) = self
                .observation
                .changed
                .wait_timeout(observation, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            observation = current;
            if timeout.timed_out() && observation.ready.is_none() {
                let message = "timed out waiting for standalone helper startup".to_string();
                observation.ready = Some(Err(message.clone()));
                self.observation.changed.notify_all();
                drop(observation);
                self.inner.fail_safe_retire();
                anyhow::bail!(message);
            }
        }
        match observation.ready.clone() {
            Some(result) => result.map_err(anyhow::Error::msg),
            None => unreachable!("standalone readiness loop exited without a result"),
        }
    }

    /// Commits a prepared suspended launch after the embedding owner has
    /// installed supervision. The target cannot execute before this call.
    pub fn commit_launch(&self) -> Result<()> {
        if self
            .inner
            .commit_sent
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            anyhow::bail!("standalone target launch was already committed");
        }
        let write_result = {
            let mut control = self
                .inner
                .control
                .lock()
                .map_err(|_| anyhow::anyhow!("standalone control lock poisoned"))?;
            let control = control
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("standalone control channel is closed"))?;
            write_wire_frame(control, ParentMessage::CommitLaunch)
        };
        if let Err(error) = write_result {
            self.inner.fail_safe_retire();
            return Err(error).context("commit standalone target launch");
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut observation = self
            .observation
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !observation.committed && observation.final_outcome.is_none() {
            let now = Instant::now();
            if now >= deadline {
                drop(observation);
                self.inner.fail_safe_retire();
                anyhow::bail!("timed out waiting for standalone target launch commit");
            }
            let (current, timeout) = self
                .observation
                .changed
                .wait_timeout(observation, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            observation = current;
            if timeout.timed_out() && !observation.committed {
                drop(observation);
                self.inner.fail_safe_retire();
                anyhow::bail!("timed out waiting for standalone target launch commit");
            }
        }
        if observation.committed {
            Ok(())
        } else {
            let error = observation
                .final_outcome
                .as_ref()
                .and_then(|outcome| outcome.infrastructure_error.as_deref())
                .unwrap_or("standalone helper exited before launch commit");
            anyhow::bail!(error.to_string())
        }
    }

    /// Requests immediate full-job termination. Windows interrupt projection is
    /// deliberately not claimed by this standalone boundary.
    pub fn force_terminate(&self, force_stop_timeout: Duration) -> Result<()> {
        let mut control = self
            .inner
            .control
            .lock()
            .map_err(|_| anyhow::anyhow!("standalone control lock poisoned"))?;
        let control = control
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("standalone control channel is closed"))?;
        write_wire_frame(
            control,
            ParentMessage::ForceTerminate {
                force_stop_timeout_ms: duration_millis(force_stop_timeout, "force-stop timeout")?,
            },
        )
    }

    pub fn try_root_outcome(&self) -> Option<WindowsSandboxStandaloneRootOutcome> {
        self.observation
            .observation
            .lock()
            .ok()
            .and_then(|observation| observation.target.clone())
    }

    pub fn wait_root_outcome(&self) -> WindowsSandboxStandaloneRootOutcome {
        let mut observation = self
            .observation
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while observation.target.is_none() {
            observation = self
                .observation
                .changed
                .wait(observation)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        observation
            .target
            .clone()
            .unwrap_or_else(|| WindowsSandboxStandaloneRootOutcome::Unknown {
                error: "root outcome notification missing".to_string(),
            })
    }

    pub fn try_outcome(&self) -> Option<WindowsSandboxStandaloneOutcome> {
        self.observation
            .observation
            .lock()
            .ok()
            .and_then(|observation| observation.final_outcome.clone())
    }

    pub fn wait(&self) -> WindowsSandboxStandaloneOutcome {
        let mut observation = self
            .observation
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while observation.final_outcome.is_none() {
            observation = self
                .observation
                .changed
                .wait(observation)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        observation
            .final_outcome
            .clone()
            .unwrap_or_else(|| control_failure_outcome("final outcome notification missing"))
    }
}

fn control_failure_outcome(message: impl Into<String>) -> WindowsSandboxStandaloneOutcome {
    let message = message.into();
    WindowsSandboxStandaloneOutcome {
        target: WindowsSandboxStandaloneRootOutcome::Unknown {
            error: message.clone(),
        },
        retirement: WindowsSandboxStandaloneRetirementOutcome {
            complete: false,
            forced: false,
            error: Some("standalone helper retirement could not be observed".to_string()),
        },
        infrastructure_error: Some(message),
    }
}

fn observe_helper(
    mut reader: File,
    observation: Arc<ObservationState>,
    process: Arc<StandaloneProcessInner>,
) {
    let result = (|| -> Result<()> {
        loop {
            let Some(message) = read_wire_frame::<HelperMessage>(&mut reader)? else {
                anyhow::bail!("standalone helper control channel closed before final outcome");
            };
            match message {
                HelperMessage::Ready { process_id } => {
                    let mut current = observation
                        .observation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if current.ready.is_some() {
                        anyhow::bail!("standalone helper sent duplicate ready");
                    }
                    current.ready = Some(Ok(process_id));
                    observation.changed.notify_all();
                }
                HelperMessage::Committed => {
                    let mut current = observation
                        .observation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !matches!(current.ready, Some(Ok(_))) {
                        anyhow::bail!("standalone helper committed before ready");
                    }
                    if current.committed {
                        anyhow::bail!("standalone helper sent duplicate launch commit");
                    }
                    current.committed = true;
                    observation.changed.notify_all();
                }
                HelperMessage::RootExited(target) => {
                    let mut current = observation
                        .observation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !matches!(current.ready, Some(Ok(_))) || !current.committed {
                        anyhow::bail!("standalone helper reported root exit before launch commit");
                    }
                    current.target = Some(target);
                    observation.changed.notify_all();
                }
                HelperMessage::Final(outcome) => {
                    let mut current = observation
                        .observation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !matches!(current.ready, Some(Ok(_))) || !current.committed {
                        anyhow::bail!(
                            "standalone helper reported final outcome before launch commit"
                        );
                    }
                    if current.target.is_none() {
                        current.target = Some(outcome.target.clone());
                    }
                    current.final_outcome = Some(outcome);
                    observation.changed.notify_all();
                    return Ok(());
                }
                HelperMessage::Error {
                    stage,
                    message,
                    windows_error_code,
                } => anyhow::bail!(
                    "standalone helper failed during {stage}: {message}{}",
                    windows_error_code
                        .map(|code| format!(" (Windows error {code})"))
                        .unwrap_or_default()
                ),
            }
        }
    })();
    if let Err(error) = result {
        process.fail_safe_retire();
        let message = format!("{error:#}");
        let mut current = observation
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.ready.is_none() {
            current.ready = Some(Err(message.clone()));
        }
        let target = current.target.clone().unwrap_or_else(|| {
            WindowsSandboxStandaloneRootOutcome::Unknown {
                error: message.clone(),
            }
        });
        current.target = Some(target.clone());
        current.final_outcome = Some(WindowsSandboxStandaloneOutcome {
            target,
            retirement: WindowsSandboxStandaloneRetirementOutcome {
                complete: false,
                forced: false,
                error: Some("standalone helper retirement could not be observed".to_string()),
            },
            infrastructure_error: Some(message),
        });
        observation.changed.notify_all();
    }
}

/// Launches one target through the prepared elevated Windows sandbox account.
/// ACL refresh is performed first without UAC; an unprepared state is rejected.
/// An `Ok` result means the target-creation request was sent or may have been
/// sent and therefore consumes the generation. The caller must install
/// supervision, call [`WindowsSandboxStandaloneProcess::wait_ready`], and only
/// then call [`WindowsSandboxStandaloneProcess::commit_launch`].
pub fn spawn_windows_sandbox_standalone(
    request: WindowsSandboxStandaloneLaunchRequest<'_>,
) -> Result<WindowsSandboxStandaloneProcess> {
    validate_setup_request(&request.setup)?;
    request.command.validate()?;
    match (
        request.setup.network.identity,
        request.network_proxy_restricting_sid.is_some(),
        request.setup.network.proxy_ports.is_empty(),
        request.setup.network.allow_local_binding,
    ) {
        (WindowsSandboxStandaloneNetworkIdentity::Online, false, true, false)
        | (WindowsSandboxStandaloneNetworkIdentity::Offline, false, true, false)
        | (WindowsSandboxStandaloneNetworkIdentity::Offline, true, false, _) => {}
        (WindowsSandboxStandaloneNetworkIdentity::Online, true, _, _) => {
            anyhow::bail!("unrestricted networking cannot use a proxy restricting SID");
        }
        (WindowsSandboxStandaloneNetworkIdentity::Offline, true, true, _) => {
            anyhow::bail!("managed-proxy restriction requires an explicit proxy port");
        }
        (WindowsSandboxStandaloneNetworkIdentity::Offline, false, false, _)
        | (WindowsSandboxStandaloneNetworkIdentity::Offline, false, true, true) => {
            anyhow::bail!("offline network exceptions require a proxy restricting SID");
        }
        (WindowsSandboxStandaloneNetworkIdentity::Online, false, false, _)
        | (WindowsSandboxStandaloneNetworkIdentity::Online, false, true, true) => {
            unreachable!("setup validation rejects online network exceptions")
        }
    }
    if crate::path_normalization::canonicalize_path(&request.command.cwd)
        != crate::path_normalization::canonicalize_path(&request.setup.command_cwd)
    {
        anyhow::bail!("launch working directory differs from the refreshed setup policy");
    }
    let policy_lease = crate::policy_lease::acquire_mcp_console_sandbox_policy_lease()
        .context("acquire Windows sandbox policy generation lease")?;
    verify_windows_sandbox_standalone_network_with_policy_lease(&request.setup, &policy_lease)
        .context("verify machine-global Windows sandbox firewall policy")?;
    refresh_windows_sandbox_standalone_with_policy_lease(&request.setup, &policy_lease)?;
    match windows_sandbox_standalone_setup_status(&request.setup) {
        super::setup::WindowsSandboxStandaloneSetupState::Ready => {}
        state => anyhow::bail!(
            "standalone Windows setup changed before the policy lease was acquired: {state:?}"
        ),
    }
    let creds = load_prepared_sandbox_creds(
        native_network_identity(request.setup.network.identity),
        &request.setup.state_dir,
    )?
    .ok_or_else(|| anyhow::anyhow!("prepared standalone sandbox credentials are unavailable"))?;
    let capability_sids = capability_sids(&request.setup)?;
    let (process_info, control, events) = spawn_helper(&request.setup, &creds)?;
    let helper = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess as _) };
    let inner = Arc::new(StandaloneProcessInner {
        control: Mutex::new(Some(control)),
        helper,
        commit_sent: AtomicBool::new(false),
        policy_lease: Mutex::new(Some(policy_lease)),
    });
    let observation = Arc::new(ObservationState::default());
    let observer_state = Arc::clone(&observation);
    let observer_process = Arc::clone(&inner);
    std::thread::Builder::new()
        .name("standalone-windows-sandbox-observer".to_string())
        .spawn(move || observe_helper(events, observer_state, observer_process))?;

    let spawn = match spawn_request_wire(
        &request,
        inner.helper.as_raw_handle() as HANDLE,
        capability_sids,
    ) {
        Ok(spawn) => spawn,
        Err(error) => {
            inner.fail_safe_retire();
            return Err(error);
        }
    };
    let process = WindowsSandboxStandaloneProcess {
        inner: Arc::clone(&inner),
        observation: Arc::clone(&observation),
    };
    let mut control = match inner.control.lock() {
        Ok(control) => control,
        Err(_) => {
            inner.fail_safe_retire();
            anyhow::bail!("standalone control lock poisoned");
        }
    };
    let Some(control_writer) = control.as_mut() else {
        drop(control);
        inner.fail_safe_retire();
        anyhow::bail!("standalone control channel is closed");
    };
    let spawn_result = write_wire_frame(control_writer, ParentMessage::Spawn(Box::new(spawn)));
    drop(control);
    if let Err(error) = spawn_result {
        let message = format!("send standalone target spawn request: {error:#}");
        inner.fail_safe_retire();
        let mut current = observation
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.ready.is_none() {
            current.ready = Some(Err(message));
            observation.changed.notify_all();
        }
    }
    Ok(process)
}
