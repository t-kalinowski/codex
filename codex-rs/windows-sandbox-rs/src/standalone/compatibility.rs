use super::setup::WindowsSandboxStandaloneResources;
use anyhow::Context;
use anyhow::Result;
use codex_utils_pty::JobObject;
use std::ffi::OsString;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

const COMPATIBILITY_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_COMPATIBILITY_OUTPUT_BYTES: usize = 1024;
const PRIVATE_COMPANION_ABI_VERSION: u32 = 1;
const CODEX_RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identifies one private standalone Windows companion executable.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsSandboxStandaloneHelperKind {
    Setup,
    CommandRunner,
}

impl WindowsSandboxStandaloneHelperKind {
    fn query(self) -> &'static str {
        match self {
            Self::Setup => "--codex-mcp-console-sandbox-windows-setup-compatibility-v1",
            Self::CommandRunner => {
                "--codex-mcp-console-sandbox-windows-command-runner-compatibility-v1"
            }
        }
    }

    fn response(self) -> String {
        match self {
            Self::Setup => format!(
                "mcp-console-sandbox-windows-setup/{PRIVATE_COMPANION_ABI_VERSION} codex/{CODEX_RELEASE_VERSION}\n"
            ),
            Self::CommandRunner => format!(
                "mcp-console-sandbox-windows-command-runner/{PRIVATE_COMPANION_ABI_VERSION} codex/{CODEX_RELEASE_VERSION}\n"
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::CommandRunner => "command-runner",
        }
    }
}

/// Returns the exact response for a side-effect-free private compatibility query.
#[doc(hidden)]
pub fn windows_sandbox_standalone_helper_compatibility_response(
    helper: WindowsSandboxStandaloneHelperKind,
    arguments: &[OsString],
) -> Option<String> {
    (arguments.len() == 1 && arguments[0] == helper.query()).then(|| helper.response())
}

/// Verifies the exact helper layout and both closed private companion ABIs.
pub fn verify_windows_sandbox_standalone_resources(
    resources: &WindowsSandboxStandaloneResources,
) -> Result<()> {
    super::setup::validate_resource_layout(resources)?;
    for (helper, path) in [
        (
            WindowsSandboxStandaloneHelperKind::Setup,
            resources.setup_executable.as_path(),
        ),
        (
            WindowsSandboxStandaloneHelperKind::CommandRunner,
            resources.command_runner_executable.as_path(),
        ),
    ] {
        verify_helper(path, helper).with_context(|| {
            format!(
                "verify packaged Windows {} helper compatibility",
                helper.label()
            )
        })?;
    }
    Ok(())
}

fn verify_helper(path: &Path, helper: WindowsSandboxStandaloneHelperKind) -> Result<()> {
    let job = JobObject::create_without_breakaway()
        .context("create packaged Windows helper compatibility Job")?;
    let mut command = Command::new(path);
    command
        .arg(helper.query())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = command.spawn().with_context(|| {
        format!(
            "start packaged Windows {} helper compatibility query at {}",
            helper.label(),
            path.display()
        )
    })?;
    job.assign_and_resume_std_process(&mut child)
        .context("contain packaged Windows helper compatibility query")?;
    let mut stdout = child
        .stdout
        .take()
        .context("packaged Windows helper compatibility stdout was unavailable")?;
    let mut stderr = child
        .stderr
        .take()
        .context("packaged Windows helper compatibility stderr was unavailable")?;
    let deadline = Instant::now() + COMPATIBILITY_QUERY_TIMEOUT;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut status = None;

    loop {
        if let Err(error) =
            read_available(&mut stdout, &mut stdout_bytes, &mut stdout_eof, "stdout").and_then(
                |()| read_available(&mut stderr, &mut stderr_bytes, &mut stderr_eof, "stderr"),
            )
        {
            terminate_query(&job, &mut child);
            return Err(error);
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => {
                    terminate_query(&job, &mut child);
                    return Err(error)
                        .context("inspect packaged Windows helper compatibility query");
                }
            }
        }
        if status.is_some() && stdout_eof && stderr_eof {
            break;
        }
        if Instant::now() >= deadline {
            terminate_query(&job, &mut child);
            anyhow::bail!(
                "packaged Windows {} helper compatibility query timed out",
                helper.label()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let status =
        status.context("completed Windows helper compatibility query has no exit status")?;
    require_compatible_output(path, helper, status, &stdout_bytes, &stderr_bytes)
}

fn read_available(
    pipe: &mut (impl Read + AsRawHandle),
    output: &mut Vec<u8>,
    eof: &mut bool,
    label: &str,
) -> Result<()> {
    if *eof {
        return Ok(());
    }
    let mut available = 0;
    let peeked = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if peeked == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_BROKEN_PIPE {
            *eof = true;
            return Ok(());
        }
        anyhow::bail!(
            "inspect packaged Windows helper compatibility {label}: Windows error {error}"
        );
    }
    if available == 0 {
        return Ok(());
    }
    let mut buffer = [0_u8; 256];
    let read_length = buffer.len().min(available as usize);
    let read = pipe
        .read(&mut buffer[..read_length])
        .with_context(|| format!("read packaged Windows helper compatibility {label}"))?;
    if read == 0 {
        *eof = true;
        return Ok(());
    }
    output.extend_from_slice(&buffer[..read]);
    if output.len() > MAX_COMPATIBILITY_OUTPUT_BYTES {
        anyhow::bail!(
            "packaged Windows helper compatibility {label} exceeded {MAX_COMPATIBILITY_OUTPUT_BYTES} bytes"
        );
    }
    Ok(())
}

fn require_compatible_output(
    path: &Path,
    helper: WindowsSandboxStandaloneHelperKind,
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<()> {
    if !status.success() || stdout != helper.response().as_bytes() || !stderr.is_empty() {
        anyhow::bail!(
            "packaged Windows {} helper is incompatible with private companion ABI {PRIVATE_COMPANION_ABI_VERSION} for Codex {CODEX_RELEASE_VERSION}: {}",
            helper.label(),
            path.display()
        );
    }
    Ok(())
}

fn terminate_query(job: &JobObject, child: &mut Child) {
    let _ = job.terminate();
    let _ = child.kill();
    let _ = child.wait();
}
