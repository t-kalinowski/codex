#![cfg(windows)]
#![allow(clippy::expect_used, clippy::new_without_default)]

use codex_utils_cargo_bin::cargo_bin;
use serde_json::Value;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use windows_sys::Win32::Foundation::DUPLICATE_CLOSE_SOURCE;
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineClose0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineOpen0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterDeleteByKey0;
use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Pipes::PIPE_READMODE_BYTE;
use windows_sys::Win32::System::Pipes::PIPE_TYPE_BYTE;
use windows_sys::Win32::System::Pipes::PIPE_WAIT;
use windows_sys::Win32::System::Threading::CreateProcessW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_DEFAULT;
use windows_sys::core::GUID;

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_BUFFER_SIZE: u32 = 64 * 1024;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const NATIVE_TEST_ENV: &str = "MCP_CONSOLE_SANDBOX_NATIVE_WINDOWS_TESTS";

static PIPE_NONCE: AtomicU64 = AtomicU64::new(0);
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

pub struct RunnerExecutable {
    path: PathBuf,
    _staging_directory: TempDir,
}

#[derive(Clone, Copy)]
pub enum IncompatibleCompanion {
    Setup,
    CommandRunner,
}

#[derive(Clone, Copy, Debug)]
pub enum CompanionCompatibilityBehavior {
    Timeout,
    NoisyOutput,
    OversizedOutput,
    PipeHoldingDescendant,
}

impl CompanionCompatibilityBehavior {
    fn marker_value(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::NoisyOutput => "noisy_output",
            Self::OversizedOutput => "oversized_output",
            Self::PipeHoldingDescendant => "pipe_holding_descendant",
        }
    }
}

impl RunnerExecutable {
    pub fn without_companions() -> Self {
        Self::stage(/*include_companions*/ false)
    }

    pub fn with_companions() -> Self {
        Self::stage(/*include_companions*/ true)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn with_ready_loss_companion(fixture: &Path) -> Self {
        let executable = Self::with_companions();
        let command_runner = executable
            .path
            .parent()
            .expect("runner parent directory")
            .join("codex-resources")
            .join("codex-command-runner.exe");
        copy_executable(fixture, &command_runner);
        executable
    }

    pub fn with_incompatible_companion(companion: IncompatibleCompanion) -> Self {
        let executable = Self::with_companions();
        let resources = executable
            .path
            .parent()
            .expect("runner parent directory")
            .join("codex-resources");
        let (source, destination) = match companion {
            IncompatibleCompanion::Setup => (
                resources.join("codex-command-runner.exe"),
                resources.join("codex-windows-sandbox-setup.exe"),
            ),
            IncompatibleCompanion::CommandRunner => (
                resources.join("codex-windows-sandbox-setup.exe"),
                resources.join("codex-command-runner.exe"),
            ),
        };
        copy_executable(&source, &destination);
        executable
    }

    pub fn with_misbehaving_command_runner(
        behavior: CompanionCompatibilityBehavior,
    ) -> (Self, PathBuf) {
        let executable = Self::with_companions();
        let command_runner = executable
            .path
            .parent()
            .expect("runner parent directory")
            .join("codex-resources")
            .join("codex-command-runner.exe");
        copy_executable(
            &cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary"),
            &command_runner,
        );
        std::fs::write(
            command_runner.with_extension("mcp-console-test-behavior"),
            behavior.marker_value(),
        )
        .expect("write command-runner compatibility behavior");
        let descendant_process_id =
            command_runner.with_extension("mcp-console-test-descendant-pid");
        (executable, descendant_process_id)
    }

    fn stage(include_companions: bool) -> Self {
        let staging_directory = TempDir::new().expect("runner staging directory");
        let path = staging_directory.path().join("mcp-console-sandbox.exe");
        copy_executable(
            &cargo_bin("mcp-console-sandbox").expect("runner binary"),
            &path,
        );
        if include_companions {
            let resources = staging_directory.path().join("codex-resources");
            std::fs::create_dir(&resources).expect("create runner resources directory");
            copy_executable(
                &cargo_bin("codex-windows-sandbox-setup").expect("setup companion"),
                &resources.join("codex-windows-sandbox-setup.exe"),
            );
            copy_executable(
                &cargo_bin("codex-command-runner").expect("command runner companion"),
                &resources.join("codex-command-runner.exe"),
            );
        }
        Self {
            path,
            _staging_directory: staging_directory,
        }
    }
}

pub fn run_bootstrap(
    executable: &RunnerExecutable,
    state_directory: &Path,
    target: &[OsString],
    stream_handle_arguments: &[u64],
    inherited_handles: &[u64],
) -> std::process::Output {
    let (control_for_runner, control) = duplex_pipe();
    let _spawn_guard = SPAWN_LOCK.lock().expect("runner spawn lock");
    set_inheritable(control_for_runner.as_raw_handle() as HANDLE, true);
    for handle in inherited_handles {
        set_inheritable(native_handle(*handle), true);
    }
    let control_handle = control_for_runner.as_raw_handle() as usize as u64;
    let mut command = Command::new(executable.path());
    command
        .arg("--state-dir")
        .arg(state_directory)
        .arg("--control-handle")
        .arg(control_handle.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for handle in stream_handle_arguments {
        command.arg("--stream-handle").arg(handle.to_string());
    }
    if !target.is_empty() {
        command.arg("--").args(target);
    }
    let child = command.spawn();
    set_inheritable(control_for_runner.as_raw_handle() as HANDLE, false);
    for handle in inherited_handles {
        set_inheritable(native_handle(*handle), false);
    }
    let child = child.expect("spawn sandbox runner");
    drop((control_for_runner, control));
    child.wait_with_output().expect("wait for sandbox runner")
}

pub fn run_bootstrap_with_duplicated_control_as_passed_stream(
    executable: &RunnerExecutable,
    state_directory: &Path,
    target: &[OsString],
) -> std::process::Output {
    let (control_for_runner, control) = duplex_pipe();
    let control_alias = control_for_runner
        .try_clone()
        .expect("duplicate runner control handle");
    let control_handle = control_for_runner.as_raw_handle() as usize as u64;
    let control_alias_handle = control_alias.as_raw_handle() as usize as u64;
    assert_ne!(control_alias_handle, control_handle);
    let _spawn_guard = SPAWN_LOCK.lock().expect("runner spawn lock");
    for handle in [&control_for_runner, &control_alias] {
        set_inheritable(handle.as_raw_handle() as HANDLE, true);
    }
    let mut command = Command::new(executable.path());
    command
        .arg("--state-dir")
        .arg(state_directory)
        .arg("--control-handle")
        .arg(control_handle.to_string())
        .arg("--stream-handle")
        .arg(control_alias_handle.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if !target.is_empty() {
        command.arg("--").args(target);
    }
    let child = command.spawn();
    for handle in [&control_for_runner, &control_alias] {
        set_inheritable(handle.as_raw_handle() as HANDLE, false);
    }
    let child = child.expect("spawn sandbox runner");
    drop((control_for_runner, control_alias, control));
    child.wait_with_output().expect("wait for sandbox runner")
}

pub fn run_bootstrap_with_duplicated_stdout_as_passed_stream(
    executable: &RunnerExecutable,
    state_directory: &Path,
    target: &[OsString],
) -> std::process::Output {
    let (control_for_runner, control) = duplex_pipe();
    let stdout = OutputPipe::new();
    drop(stdout.reader);
    let stdout_alias = stdout
        .writer
        .try_clone()
        .expect("duplicate runner stdout handle");
    let stdout_handle = stdout.writer.as_raw_handle() as usize as u64;
    let stdout_alias_handle = stdout_alias.as_raw_handle() as usize as u64;
    assert_ne!(stdout_alias_handle, stdout_handle);
    let _spawn_guard = SPAWN_LOCK.lock().expect("runner spawn lock");
    for handle in [&control_for_runner, &stdout_alias] {
        set_inheritable(handle.as_raw_handle() as HANDLE, true);
    }
    let control_handle = control_for_runner.as_raw_handle() as usize as u64;
    let mut command = Command::new(executable.path());
    command
        .arg("--state-dir")
        .arg(state_directory)
        .arg("--control-handle")
        .arg(control_handle.to_string())
        .arg("--stream-handle")
        .arg(stdout_alias_handle.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.writer))
        .stderr(Stdio::piped());
    if !target.is_empty() {
        command.arg("--").args(target);
    }
    let child = command.spawn();
    for handle in [&control_for_runner, &stdout_alias] {
        set_inheritable(handle.as_raw_handle() as HANDLE, false);
    }
    let child = child.expect("spawn sandbox runner");
    drop((control_for_runner, stdout_alias, control));
    child.wait_with_output().expect("wait for sandbox runner")
}

pub struct Runner {
    child: Child,
    control: Option<File>,
    reaped: bool,
}

impl Runner {
    pub fn spawn(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
    ) -> Self {
        Self::spawn_with_environment(executable, state_directory, target, &[], &[])
    }

    pub fn spawn_with_handles(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
        inherited_handles: &[u64],
    ) -> Self {
        Self::spawn_with_environment(executable, state_directory, target, &[], inherited_handles)
    }

    pub fn spawn_with_environment(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
        environment: &[(OsString, OsString)],
        inherited_handles: &[u64],
    ) -> Self {
        Self::spawn_with_environment_and_streams(
            executable,
            state_directory,
            target,
            environment,
            inherited_handles,
            Stdio::null(),
            Stdio::null(),
            Stdio::inherit(),
        )
    }

    pub fn spawn_with_inherited_output(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
    ) -> (Self, File, File) {
        let stdout = OutputPipe::new();
        let stderr = OutputPipe::new();
        let runner = Self::spawn_with_environment_and_streams(
            executable,
            state_directory,
            target,
            &[],
            &[],
            Stdio::null(),
            stdout.writer_stdio(),
            stderr.writer_stdio(),
        );
        (runner, stdout.into_reader(), stderr.into_reader())
    }

    pub fn spawn_with_inherited_input_and_output(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
    ) -> (Self, File, File) {
        let stdin = InputPipe::new();
        let stdout = OutputPipe::new();
        let runner = Self::spawn_with_environment_and_streams(
            executable,
            state_directory,
            target,
            &[],
            &[],
            stdin.reader_stdio(),
            stdout.writer_stdio(),
            Stdio::inherit(),
        );
        (runner, stdin.into_writer(), stdout.into_reader())
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_with_environment_and_streams(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
        environment: &[(OsString, OsString)],
        inherited_handles: &[u64],
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Self {
        let (control_for_runner, control) = duplex_pipe();
        let _spawn_guard = SPAWN_LOCK.lock().expect("runner spawn lock");
        set_inheritable(control_for_runner.as_raw_handle() as HANDLE, true);
        for handle in inherited_handles {
            set_inheritable(native_handle(*handle), true);
        }
        let control_handle = control_for_runner.as_raw_handle() as usize as u64;
        let mut command = Command::new(executable.path());
        command
            .arg("--state-dir")
            .arg(state_directory)
            .arg("--control-handle")
            .arg(control_handle.to_string())
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        for handle in inherited_handles {
            command.arg("--stream-handle").arg(handle.to_string());
        }
        command.envs(environment.iter().cloned());
        if !target.is_empty() {
            command.arg("--").args(target);
        }
        let child = command.spawn();
        set_inheritable(control_for_runner.as_raw_handle() as HANDLE, false);
        for handle in inherited_handles {
            set_inheritable(native_handle(*handle), false);
        }
        let child = child.expect("spawn sandbox runner");
        drop(control_for_runner);
        Self {
            child,
            control: Some(control),
            reaped: false,
        }
    }

    pub fn request(&mut self, request: Value) -> Value {
        let control = self.control.as_mut().expect("runner control is open");
        write_json_frame(control, request);
        read_json_frame(control)
    }

    pub fn close_control(&mut self) {
        drop(self.control.take());
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let timeout_ms = u32::try_from(timeout.as_millis()).expect("runner wait timeout");
        let wait = unsafe { WaitForSingleObject(self.child.as_raw_handle() as HANDLE, timeout_ms) };
        assert_ne!(wait, WAIT_TIMEOUT, "sandbox runner did not exit in time");
        assert_eq!(
            wait,
            WAIT_OBJECT_0,
            "WaitForSingleObject failed: {}",
            std::io::Error::last_os_error()
        );
        let status = self.child.wait().expect("wait for sandbox runner");
        self.reaped = true;
        status
    }

    pub fn kill(&mut self) -> ExitStatus {
        self.child.kill().expect("kill sandbox runner");
        let status = self.child.wait().expect("wait for killed sandbox runner");
        self.reaped = true;
        status
    }
}

pub fn open_process_for_wait(process_id: u32) -> std::io::Result<OwnedHandle> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
    if handle == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
}

pub fn wait_for_process_exit(process: &OwnedHandle, timeout: Duration) -> std::io::Result<()> {
    let timeout_ms = u32::try_from(timeout.as_millis())
        .map_err(|_| std::io::Error::other("process wait timeout exceeds u32"))?;
    match unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, timeout_ms) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out waiting for process exit",
        )),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        result => Err(std::io::Error::other(format!(
            "unexpected process wait result: {result}"
        ))),
    }
}

pub fn delete_one_standalone_wfp_filter() {
    const FILTER_KEY: GUID = GUID::from_u128(0x51b90ce5_2e26_47be_bd22_975898b4e09c);

    let mut engine = 0;
    let opened = unsafe {
        FwpmEngineOpen0(
            std::ptr::null(),
            RPC_C_AUTHN_DEFAULT as u32,
            std::ptr::null(),
            std::ptr::null(),
            &mut engine,
        )
    };
    assert_eq!(opened, 0, "open WFP engine: {opened:#x}");
    let deleted = unsafe { FwpmFilterDeleteByKey0(engine, &FILTER_KEY) };
    let closed = unsafe { FwpmEngineClose0(engine) };
    assert_eq!(deleted, 0, "delete standalone WFP filter: {deleted:#x}");
    assert_eq!(closed, 0, "close WFP engine: {closed:#x}");
}

pub struct ReadAclMutexGuard(HANDLE);

impl Drop for ReadAclMutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

pub fn hold_standalone_read_acl_mutex() -> ReadAclMutexGuard {
    let namespace = codex_windows_sandbox::WindowsSandboxPolicyNamespace::McpConsole;
    let name = std::ffi::OsStr::new(namespace.read_acl_mutex_name())
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    assert_ne!(
        handle,
        0,
        "create standalone read ACL mutex: {}",
        std::io::Error::last_os_error()
    );
    ReadAclMutexGuard(handle)
}

pub struct AliasedRunner {
    process: OwnedHandle,
    control: Option<File>,
}

enum AliasedStdout {
    Control,
    DuplicatedControl,
    Passed(OwnedHandle),
}

impl AliasedRunner {
    pub fn spawn_with_control_as_stdout(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
    ) -> Self {
        Self::spawn_with_stdout(executable, state_directory, target, AliasedStdout::Control).0
    }

    pub fn spawn_with_duplicated_control_as_stdout(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
    ) -> Self {
        Self::spawn_with_stdout(
            executable,
            state_directory,
            target,
            AliasedStdout::DuplicatedControl,
        )
        .0
    }

    pub fn spawn_with_passed_stdout(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
    ) -> (Self, u64) {
        let stdout = OutputPipe::new();
        drop(stdout.reader);
        Self::spawn_with_stdout(
            executable,
            state_directory,
            target,
            AliasedStdout::Passed(stdout.writer),
        )
    }

    fn spawn_with_stdout(
        executable: &RunnerExecutable,
        state_directory: &Path,
        target: &[OsString],
        stdout: AliasedStdout,
    ) -> (Self, u64) {
        let (control_for_runner, control) = duplex_pipe();
        let stdin = std::fs::OpenOptions::new()
            .read(true)
            .open("NUL")
            .expect("open null stdin");
        let stderr = std::fs::OpenOptions::new()
            .write(true)
            .open("NUL")
            .expect("open null stderr");
        let (passed_stdout, duplicated_control) = match stdout {
            AliasedStdout::Control => (None, None),
            AliasedStdout::DuplicatedControl => (
                None,
                Some(
                    control_for_runner
                        .try_clone()
                        .expect("duplicate runner control handle"),
                ),
            ),
            AliasedStdout::Passed(stdout) => (Some(stdout), None),
        };
        let _spawn_guard = SPAWN_LOCK.lock().expect("runner spawn lock");
        let stdout_handle = passed_stdout
            .as_ref()
            .map(AsRawHandle::as_raw_handle)
            .or_else(|| duplicated_control.as_ref().map(AsRawHandle::as_raw_handle))
            .unwrap_or_else(|| control_for_runner.as_raw_handle())
            as HANDLE;
        if duplicated_control.is_some() {
            assert_ne!(stdout_handle, control_for_runner.as_raw_handle() as HANDLE);
        }
        for handle in [
            control_for_runner.as_raw_handle() as HANDLE,
            stdin.as_raw_handle() as HANDLE,
            stdout_handle,
            stderr.as_raw_handle() as HANDLE,
        ] {
            set_inheritable(handle, true);
        }
        let control_handle = control_for_runner.as_raw_handle() as usize as u64;
        let arguments = std::iter::once(executable.path().as_os_str().to_owned())
            .chain([
                OsString::from("--state-dir"),
                state_directory.as_os_str().to_owned(),
                OsString::from("--control-handle"),
                OsString::from(control_handle.to_string()),
                OsString::from("--"),
            ])
            .chain(target.iter().cloned())
            .collect::<Vec<_>>();
        let mut command_line = native_command_line(&arguments);
        let application = executable
            .path()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = stdin.as_raw_handle() as HANDLE;
        startup.hStdOutput = stdout_handle;
        startup.hStdError = stderr.as_raw_handle() as HANDLE;
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        assert_ne!(
            unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command_line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    /*binherithandles*/ 1,
                    /*dwcreationflags*/ 0,
                    std::ptr::null(),
                    std::ptr::null(),
                    &startup,
                    &mut process,
                )
            },
            0,
            "CreateProcessW failed: {}",
            std::io::Error::last_os_error()
        );
        drop(unsafe { OwnedHandle::from_raw_handle(process.hThread as _) });
        drop(control_for_runner);
        (
            Self {
                process: unsafe { OwnedHandle::from_raw_handle(process.hProcess as _) },
                control: Some(control),
            },
            stdout_handle as usize as u64,
        )
    }

    pub fn request(&mut self, request: Value) -> Value {
        let control = self.control.as_mut().expect("runner control is open");
        write_json_frame(control, request);
        read_json_frame(control)
    }

    pub fn close_inherited_handle(&self, handle: u64) {
        let mut local_duplicate = 0;
        assert_ne!(
            unsafe {
                DuplicateHandle(
                    self.process.as_raw_handle() as HANDLE,
                    native_handle(handle),
                    GetCurrentProcess(),
                    &mut local_duplicate,
                    /*dwdesiredaccess*/ 0,
                    /*binherithandle*/ 0,
                    DUPLICATE_CLOSE_SOURCE | DUPLICATE_SAME_ACCESS,
                )
            },
            0,
            "DuplicateHandle failed: {}",
            std::io::Error::last_os_error()
        );
        drop(unsafe { OwnedHandle::from_raw_handle(local_duplicate as _) });
    }
}

impl Drop for AliasedRunner {
    fn drop(&mut self) {
        drop(self.control.take());
        if unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, 5_000) }
            == WAIT_TIMEOUT
        {
            unsafe {
                let _ = TerminateProcess(self.process.as_raw_handle() as HANDLE, 1);
                let _ = WaitForSingleObject(self.process.as_raw_handle() as HANDLE, 5_000);
            }
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if self
            .child
            .try_wait()
            .expect("query sandbox runner")
            .is_none()
        {
            self.child.kill().expect("kill sandbox runner");
        }
        self.child.wait().expect("wait for sandbox runner");
    }
}

pub struct OutputPipe {
    reader: File,
    writer: OwnedHandle,
}

pub struct InputPipe {
    reader: OwnedHandle,
    writer: File,
}

impl InputPipe {
    pub fn new() -> Self {
        let mut read_handle = 0;
        let mut write_handle = 0;
        let security = security_attributes();
        assert_ne!(
            unsafe {
                CreatePipe(
                    &mut read_handle,
                    &mut write_handle,
                    &security,
                    /*nsize*/ 0,
                )
            },
            0,
            "CreatePipe failed: {}",
            std::io::Error::last_os_error()
        );
        set_inheritable(write_handle, false);
        Self {
            reader: unsafe { OwnedHandle::from_raw_handle(read_handle as *mut std::ffi::c_void) },
            writer: unsafe { File::from_raw_handle(write_handle as *mut std::ffi::c_void) },
        }
    }

    pub fn reader_value(&self) -> u64 {
        self.reader.as_raw_handle() as usize as u64
    }

    fn reader_stdio(&self) -> Stdio {
        Stdio::from(
            self.reader
                .try_clone()
                .expect("duplicate input pipe reader"),
        )
    }

    pub fn into_writer(self) -> File {
        drop(self.reader);
        self.writer
    }
}

impl OutputPipe {
    pub fn new() -> Self {
        let mut read_handle = 0;
        let mut write_handle = 0;
        let security = security_attributes();
        assert_ne!(
            unsafe {
                CreatePipe(
                    &mut read_handle,
                    &mut write_handle,
                    &security,
                    /*nsize*/ 0,
                )
            },
            0,
            "CreatePipe failed: {}",
            std::io::Error::last_os_error()
        );
        set_inheritable(read_handle, false);
        Self {
            reader: unsafe { File::from_raw_handle(read_handle as *mut std::ffi::c_void) },
            writer: unsafe { OwnedHandle::from_raw_handle(write_handle as *mut std::ffi::c_void) },
        }
    }

    pub fn writer_value(&self) -> u64 {
        self.writer.as_raw_handle() as usize as u64
    }

    fn writer_stdio(&self) -> Stdio {
        Stdio::from(
            self.writer
                .try_clone()
                .expect("duplicate output pipe writer"),
        )
    }

    pub fn into_reader(self) -> File {
        drop(self.writer);
        self.reader
    }
}

pub fn native_tests_enabled() -> bool {
    match std::env::var(NATIVE_TEST_ENV) {
        Ok(value) => {
            assert_eq!(value, "1", "{NATIVE_TEST_ENV} must be exactly 1 when set");
            true
        }
        Err(std::env::VarError::NotPresent) => false,
        Err(error) => panic!("read {NATIVE_TEST_ENV}: {error}"),
    }
}

fn duplex_pipe() -> (OwnedHandle, File) {
    let name = format!(
        r"\\.\pipe\mcp-console-sandbox-contract-{}-{}",
        std::process::id(),
        PIPE_NONCE.fetch_add(1, Ordering::Relaxed)
    );
    let wide_name = name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let server = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            /*nmaxinstances*/ 1,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            /*ndefaulttimeout*/ 0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        server,
        INVALID_HANDLE_VALUE,
        "CreateNamedPipeW failed: {}",
        std::io::Error::last_os_error()
    );
    let server = unsafe { OwnedHandle::from_raw_handle(server as *mut std::ffi::c_void) };
    let connector = std::thread::spawn(move || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
    });
    let connected =
        unsafe { ConnectNamedPipe(server.as_raw_handle() as HANDLE, std::ptr::null_mut()) };
    if connected == 0 {
        assert_eq!(
            unsafe { GetLastError() },
            ERROR_PIPE_CONNECTED,
            "ConnectNamedPipe failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let client = connector
        .join()
        .expect("named-pipe connector thread")
        .expect("open named-pipe client");
    (server, client)
}

fn security_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 0,
    }
}

fn native_handle(handle: u64) -> HANDLE {
    let handle = usize::try_from(handle).expect("native handle width");
    isize::from_ne_bytes(handle.to_ne_bytes())
}

fn set_inheritable(handle: HANDLE, inheritable: bool) {
    assert_ne!(
        unsafe {
            SetHandleInformation(
                handle,
                HANDLE_FLAG_INHERIT,
                if inheritable { HANDLE_FLAG_INHERIT } else { 0 },
            )
        },
        0,
        "SetHandleInformation failed: {}",
        std::io::Error::last_os_error()
    );
}

fn copy_executable(source: &Path, destination: &Path) {
    std::fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "copy executable {} to {}: {error}",
            source.display(),
            destination.display()
        )
    });
}

fn native_command_line(arguments: &[OsString]) -> Vec<u16> {
    let mut command_line = Vec::new();
    for argument in arguments {
        if !command_line.is_empty() {
            command_line.push(b' ' as u16);
        }
        let argument = argument.encode_wide().collect::<Vec<_>>();
        command_line.push(b'"' as u16);
        let mut backslashes = 0;
        for unit in argument {
            if unit == b'\\' as u16 {
                backslashes += 1;
            } else if unit == b'"' as u16 {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
                command_line.push(unit);
                backslashes = 0;
            } else {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                command_line.push(unit);
                backslashes = 0;
            }
        }
        command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
        command_line.push(b'"' as u16);
    }
    command_line.push(0);
    command_line
}

fn write_json_frame(stream: &mut impl Write, value: Value) {
    let payload = serde_json::to_vec(&value).expect("serialize request");
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .expect("write frame length");
    stream.write_all(&payload).expect("write frame payload");
}

fn read_json_frame(stream: &mut impl Read) -> Value {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).expect("read frame length");
    let mut payload = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).expect("read frame payload");
    serde_json::from_slice(&payload).expect("parse response")
}
