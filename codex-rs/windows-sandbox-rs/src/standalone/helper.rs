use super::WindowsSandboxStandaloneOutcome;
use super::WindowsSandboxStandaloneRetirementOutcome;
use super::WindowsSandboxStandaloneRootOutcome;
use super::process_security::HelperProcessSecurity;
use super::setup::is_absolute_local_disk_path;
use super::wire::HelperMessage;
use super::wire::ParentMessage;
use super::wire::WireNativeString;
use super::wire::WireSpawnRequest;
use super::wire::WireStream;
use super::wire::read_wire_frame;
use super::wire::write_wire_frame;
use crate::process::ConsoleMode;
use crate::process::ProcessErrorDetail;
use crate::process::ProcessJobMode;
use crate::process::ProcessStartMode;
use crate::process::create_process_as_user_native;
use crate::token::LocalSid;
use crate::token::create_standalone_token_with_caps_from;
use crate::token::get_current_token_for_restriction;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use codex_utils_pty::JobObject;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectBasicAccountingInformation;
use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::GetProcessId;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::ResumeThread;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

#[cfg(test)]
#[path = "helper_tests.rs"]
mod tests;

const WAIT_OBJECT_0: u32 = 0;

struct HelperOwnedHandle(OwnedHandle);

impl HelperOwnedHandle {
    fn from_raw(handle: HANDLE, label: &str) -> Result<Self> {
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            anyhow::bail!("standalone helper received an invalid {label} handle");
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle as _) }))
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

#[derive(Clone, Copy)]
enum HelperStandardStream {
    Stdin,
    Stdout,
    Stderr,
}

impl HelperStandardStream {
    fn null_access(self) -> u32 {
        match self {
            Self::Stdin => GENERIC_READ,
            Self::Stdout | Self::Stderr => GENERIC_WRITE,
        }
    }
}

fn prepare_helper_stream(
    stream: WireStream,
    kind: HelperStandardStream,
) -> Result<HelperOwnedHandle> {
    let handle = match stream {
        WireStream::Handle(handle) => {
            let handle = usize::try_from(handle).context("standalone stream handle overflow")?;
            handle as HANDLE
        }
        WireStream::Null => {
            let null = to_wide(r"\\.\NUL");
            let handle = unsafe {
                CreateFileW(
                    null.as_ptr(),
                    kind.null_access(),
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    ptr::null_mut(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    0,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                anyhow::bail!(
                    "CreateFileW failed for standalone null stream: {}",
                    unsafe { GetLastError() }
                );
            }
            handle
        }
    };
    let handle = HelperOwnedHandle::from_raw(handle, "standard-stream")?;
    if unsafe { SetHandleInformation(handle.raw(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0
    {
        anyhow::bail!(
            "SetHandleInformation failed for standalone stream: {}",
            unsafe { GetLastError() }
        );
    }
    Ok(handle)
}

fn open_helper_pipe(name: &str, access: u32) -> Result<File> {
    let name = to_wide(name);
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            /*dw_share_mode*/ 0,
            ptr::null_mut(),
            OPEN_EXISTING,
            /*dw_flags_and_attributes*/ 0,
            /*h_template_file*/ 0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        anyhow::bail!(
            "CreateFileW failed for standalone control pipe: {}",
            unsafe { GetLastError() }
        );
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

fn helper_pipe_names() -> Result<(String, String)> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if !super::is_windows_sandbox_standalone_helper_invocation(&arguments) {
        anyhow::bail!("invalid standalone command-runner invocation");
    }
    let pipe_in = arguments[1]
        .to_str()
        .and_then(|argument| argument.strip_prefix("--pipe-in="))
        .ok_or_else(|| anyhow::anyhow!("standalone pipe-in is missing"))?;
    let pipe_out = arguments[2]
        .to_str()
        .and_then(|argument| argument.strip_prefix("--pipe-out="))
        .ok_or_else(|| anyhow::anyhow!("standalone pipe-out is missing"))?;
    Ok((pipe_in.to_string(), pipe_out.to_string()))
}

fn helper_error(writer: &mut File, stage: &str, error: &anyhow::Error) -> Result<()> {
    let windows_error_code = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .and_then(|code| u32::try_from(code).ok())
    });
    write_wire_frame(
        writer,
        HelperMessage::Error {
            stage: stage.to_string(),
            message: format!("{error:#}"),
            windows_error_code,
        },
    )
}

fn decode_wire_path(value: WireNativeString, label: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value.into_os_string()?);
    if !is_absolute_local_disk_path(&path) {
        anyhow::bail!("standalone helper received a non-absolute {label}");
    }
    Ok(path)
}

fn spawn_helper_target(request: WireSpawnRequest) -> Result<crate::process::CreatedProcess> {
    let program = decode_wire_path(request.program, "program")?;
    let cwd = decode_wire_path(request.cwd, "working directory")?;
    let state_dir = decode_wire_path(request.state_dir, "state directory")?;
    let args = request
        .args
        .into_iter()
        .map(WireNativeString::into_os_string)
        .collect::<Result<Vec<_>>>()?;
    let environment = request
        .environment
        .into_iter()
        .map(|(key, value)| Ok((key.into_os_string()?, value.into_os_string()?)))
        .collect::<Result<Vec<_>>>()?;
    let stdin = prepare_helper_stream(request.stdin, HelperStandardStream::Stdin)?;
    let stdout = prepare_helper_stream(request.stdout, HelperStandardStream::Stdout)?;
    let stderr = prepare_helper_stream(request.stderr, HelperStandardStream::Stderr)?;

    crate::hide_current_user_profile_dir(&state_dir);
    let capability_sids = request
        .capability_sids
        .iter()
        .map(|sid| LocalSid::from_string(sid))
        .collect::<Result<Vec<_>>>()?;
    if capability_sids.is_empty() {
        anyhow::bail!("standalone helper received no filesystem capability SID");
    }
    let network_sid = request
        .network_proxy_restricting_sid
        .as_deref()
        .map(LocalSid::from_string)
        .transpose()?;
    let capability_sid_ptrs = capability_sids
        .iter()
        .map(LocalSid::as_ptr)
        .collect::<Vec<_>>();
    let network_sid_ptrs = network_sid.iter().map(LocalSid::as_ptr).collect::<Vec<_>>();
    unsafe {
        for sid in &capability_sid_ptrs {
            crate::allow_null_device(*sid);
        }
    }
    let base_token = HelperOwnedHandle::from_raw(
        unsafe { get_current_token_for_restriction()? },
        "base token",
    )?;
    let target_token = HelperOwnedHandle::from_raw(
        unsafe {
            create_standalone_token_with_caps_from(
                base_token.raw(),
                &capability_sid_ptrs,
                &network_sid_ptrs,
            )
        }?,
        "target token",
    )?;
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.as_os_str().to_owned());
    argv.extend(args);
    let logs = state_dir.join(".sandbox");
    unsafe {
        create_process_as_user_native(
            target_token.raw(),
            Some(program.as_os_str()),
            &argv,
            &cwd,
            &environment,
            Some(&logs),
            Some((stdin.raw(), stdout.raw(), stderr.raw())),
            ConsoleMode::Inherit,
            request.use_private_desktop,
            ProcessJobMode::DenyBreakaway,
            ProcessErrorDetail::RedactCommand,
            ProcessStartMode::Suspended,
        )
    }
}

fn active_process_count(job: &JobObject) -> Result<u32> {
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectBasicAccountingInformation,
            ptr::addr_of_mut!(accounting).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    if queried == 0 {
        Err(std::io::Error::last_os_error()).context("query standalone process job")
    } else {
        Ok(accounting.ActiveProcesses)
    }
}

fn wait_for_job_empty(job: &JobObject, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if active_process_count(job)? == 0 {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn helper_input_loop(
    mut reader: File,
    forced: Arc<AtomicBool>,
    force_stop_timeout_ms: Arc<AtomicU64>,
    mut terminate_job: impl FnMut() -> std::io::Result<()>,
) -> Result<()> {
    loop {
        match read_wire_frame::<ParentMessage>(&mut reader) {
            Ok(Some(ParentMessage::ForceTerminate {
                force_stop_timeout_ms: requested_timeout_ms,
            })) => {
                force_stop_timeout_ms.store(requested_timeout_ms, Ordering::SeqCst);
                forced.store(true, Ordering::SeqCst);
                terminate_job().context("force-terminate standalone process job")?;
            }
            Ok(Some(ParentMessage::Spawn(_))) => {
                forced.store(true, Ordering::SeqCst);
                terminate_job()
                    .context("terminate standalone process job after duplicate spawn")?;
                anyhow::bail!("standalone helper received duplicate spawn");
            }
            Ok(Some(ParentMessage::CommitLaunch)) => {
                forced.store(true, Ordering::SeqCst);
                terminate_job()
                    .context("terminate standalone process job after duplicate launch commit")?;
                anyhow::bail!("standalone helper received duplicate launch commit");
            }
            Err(error) => {
                forced.store(true, Ordering::SeqCst);
                terminate_job()
                    .context("terminate standalone process job after control read failure")?;
                return Err(error).context("read standalone helper control message");
            }
            Ok(None) => {
                forced.store(true, Ordering::SeqCst);
                terminate_job().context("terminate standalone process job after control EOF")?;
                return Ok(());
            }
        }
    }
}

fn await_launch_commit(
    mut reader: File,
    mut terminate_job: impl FnMut() -> std::io::Result<()>,
) -> Result<File> {
    match read_wire_frame::<ParentMessage>(&mut reader) {
        Ok(Some(ParentMessage::CommitLaunch)) => Ok(reader),
        Ok(Some(ParentMessage::Spawn(_))) => {
            terminate_job().context("terminate standalone process job after duplicate spawn")?;
            anyhow::bail!("standalone helper received duplicate spawn before launch commit")
        }
        Ok(Some(ParentMessage::ForceTerminate { .. })) => {
            terminate_job().context("terminate standalone process job before launch commit")?;
            anyhow::bail!("standalone helper received termination before launch commit")
        }
        Ok(None) => {
            terminate_job().context("terminate standalone process job after pre-commit EOF")?;
            anyhow::bail!("standalone helper control channel closed before launch commit")
        }
        Err(error) => {
            terminate_job()
                .context("terminate standalone process job after pre-commit control failure")?;
            Err(error).context("read standalone launch commit")
        }
    }
}

fn root_outcome(process: HANDLE) -> WindowsSandboxStandaloneRootOutcome {
    let waited = unsafe { WaitForSingleObject(process, INFINITE) };
    if waited != WAIT_OBJECT_0 {
        return WindowsSandboxStandaloneRootOutcome::Unknown {
            error: format!("WaitForSingleObject failed for target root: {waited:#x}"),
        };
    }
    let mut code = 0;
    if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
        return WindowsSandboxStandaloneRootOutcome::Unknown {
            error: format!("GetExitCodeProcess failed: {}", unsafe { GetLastError() }),
        };
    }
    WindowsSandboxStandaloneRootOutcome::Exited { code }
}

fn retire_job(
    job: &JobObject,
    forced: &AtomicBool,
    descendant_grace: Duration,
    force_stop_timeout_ms: &AtomicU64,
) -> WindowsSandboxStandaloneRetirementOutcome {
    let grace = if forced.load(Ordering::SeqCst) {
        Duration::ZERO
    } else {
        descendant_grace
    };
    match wait_for_job_empty(job, grace) {
        Ok(true) => WindowsSandboxStandaloneRetirementOutcome {
            complete: true,
            forced: forced.load(Ordering::SeqCst),
            error: None,
        },
        Ok(false) => {
            forced.store(true, Ordering::SeqCst);
            if let Err(error) = job.terminate() {
                return WindowsSandboxStandaloneRetirementOutcome {
                    complete: false,
                    forced: true,
                    error: Some(format!(
                        "failed to terminate standalone process job: {error}"
                    )),
                };
            }
            let force_stop_timeout =
                Duration::from_millis(force_stop_timeout_ms.load(Ordering::SeqCst));
            match wait_for_job_empty(job, force_stop_timeout) {
                Ok(complete) => WindowsSandboxStandaloneRetirementOutcome {
                    complete,
                    forced: true,
                    error: (!complete).then(|| {
                        "standalone process job remained active after forced termination"
                            .to_string()
                    }),
                },
                Err(error) => WindowsSandboxStandaloneRetirementOutcome {
                    complete: false,
                    forced: true,
                    error: Some(format!(
                        "failed to observe standalone job retirement: {error}"
                    )),
                },
            }
        }
        Err(error) => {
            forced.store(true, Ordering::SeqCst);
            let termination_error = job.terminate().err();
            WindowsSandboxStandaloneRetirementOutcome {
                complete: false,
                forced: true,
                error: Some(match termination_error {
                    Some(termination_error) => format!(
                        "failed to observe job retirement: {error}; termination also failed: {termination_error}"
                    ),
                    None => format!("failed to observe job retirement: {error}"),
                }),
            }
        }
    }
}

fn helper_run(reader: &mut File, writer: &mut File) -> Result<()> {
    let mut request = match read_wire_frame::<ParentMessage>(reader)? {
        Some(ParentMessage::Spawn(request)) => *request,
        Some(ParentMessage::CommitLaunch) => {
            anyhow::bail!("standalone helper received launch commit before spawn")
        }
        Some(ParentMessage::ForceTerminate { .. }) => {
            anyhow::bail!("standalone helper received termination before spawn")
        }
        None => anyhow::bail!("standalone helper control channel closed before spawn"),
    };
    let mut process_security =
        HelperProcessSecurity::from_trusted_runner_sid(std::mem::take(&mut request.runner_sid))
            .context("validate trusted runner SID")?;
    let descendant_grace = Duration::from_millis(request.descendant_grace_ms);
    let force_stop_timeout_ms = Arc::new(AtomicU64::new(request.force_stop_timeout_ms));
    let created = spawn_helper_target(request)?;
    let process_id = unsafe { GetProcessId(created.process_info.hProcess) };
    if let Err(error) = write_wire_frame(writer, HelperMessage::Ready { process_id }) {
        let _ = created.job.terminate();
        return Err(error);
    }
    let reader = await_launch_commit(reader.try_clone()?, || created.job.terminate())?;
    let forced = Arc::new(AtomicBool::new(false));
    let input_forced = Arc::clone(&forced);
    let input_job = Arc::clone(&created.job);
    let input_force_stop_timeout_ms = Arc::clone(&force_stop_timeout_ms);
    let (control_gate_sender, control_gate_receiver) = std::sync::mpsc::channel();
    let control_thread = match std::thread::Builder::new()
        .name("standalone-windows-sandbox-control".to_string())
        .spawn(move || {
            if control_gate_receiver.recv().is_err() {
                return;
            }
            if helper_input_loop(reader, input_forced, input_force_stop_timeout_ms, || {
                input_job.terminate()
            })
            .is_err()
            {
                // This process owns the only helper-side Job handle. Exiting
                // closes it so KILL_ON_JOB_CLOSE retires the target tree, and
                // the parent converts control EOF into a structured failure.
                std::process::exit(1);
            }
        }) {
        Ok(thread) => thread,
        Err(error) => {
            let _ = created.job.terminate();
            return Err(error).context("spawn standalone helper control thread");
        }
    };
    if let Err(error) = process_security
        .seal_helper_owned_thread(control_thread.as_raw_handle() as HANDLE)
        .context("seal standalone helper control thread")
    {
        let _ = created.job.terminate();
        drop(control_gate_sender);
        let _ = control_thread.join();
        return Err(error);
    }
    if unsafe { ResumeThread(created.process_info.hThread) } == u32::MAX {
        let error = std::io::Error::last_os_error();
        let _ = created.job.terminate();
        drop(control_gate_sender);
        let _ = control_thread.join();
        return Err(error).context("resume committed standalone target");
    }
    if let Err(error) = write_wire_frame(writer, HelperMessage::Committed) {
        let _ = created.job.terminate();
        drop(control_gate_sender);
        let _ = control_thread.join();
        return Err(error);
    }
    if control_gate_sender.send(()).is_err() {
        let _ = created.job.terminate();
        let _ = control_thread.join();
        anyhow::bail!("standalone helper control thread exited before launch commit");
    }
    drop(control_thread);

    let target = root_outcome(created.process_info.hProcess);
    if let Err(error) = write_wire_frame(writer, HelperMessage::RootExited(target.clone())) {
        let _ = created.job.terminate();
        return Err(error);
    }
    let retirement = retire_job(
        &created.job,
        &forced,
        descendant_grace,
        &force_stop_timeout_ms,
    );
    let outcome = WindowsSandboxStandaloneOutcome {
        target,
        retirement,
        infrastructure_error: None,
    };
    write_wire_frame(writer, HelperMessage::Final(outcome))?;
    unsafe {
        CloseHandle(created.process_info.hThread);
        CloseHandle(created.process_info.hProcess);
    }
    Ok(())
}

/// Private entrypoint used only by `codex-command-runner --standalone-sandbox-runner`.
#[doc(hidden)]
pub fn run_windows_sandbox_standalone_helper() -> Result<()> {
    let (pipe_in, pipe_out) = helper_pipe_names()?;
    let mut reader = open_helper_pipe(&pipe_in, FILE_GENERIC_READ)?;
    let mut writer = open_helper_pipe(&pipe_out, FILE_GENERIC_WRITE)?;
    match helper_run(&mut reader, &mut writer) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = helper_error(&mut writer, "standalone_helper", &error);
            Err(error)
        }
    }
}
