use super::*;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::process::Stdio;
use tokio::process::Command;

impl SandboxExitStatus {
    fn from_native(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            signal: status.signal(),
        }
    }
}

pub(super) struct UnixProcess {
    process_group_id: u32,
    completion: watch::Receiver<ProcessCompletion>,
    _runtime: Arc<RuntimeInner>,
}

impl UnixProcess {
    pub(super) async fn wait(&self) -> Result<SandboxExitStatus, SandboxError> {
        let mut completion = self.completion.clone();
        loop {
            match completion.borrow().clone() {
                ProcessCompletion::Running => {}
                ProcessCompletion::Exited(status) => return Ok(status),
                ProcessCompletion::Failed { kind, message } => {
                    return Err(SandboxError::io(
                        "waiting for sandboxed process",
                        io::Error::new(kind, message),
                    ));
                }
            }
            completion.changed().await.map_err(|_| {
                SandboxError::io(
                    "waiting for sandboxed process",
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "process completion channel closed",
                    ),
                )
            })?;
        }
    }

    pub(super) fn try_status(&self) -> Option<SandboxExitStatus> {
        match *self.completion.borrow() {
            ProcessCompletion::Exited(status) => Some(status),
            ProcessCompletion::Running | ProcessCompletion::Failed { .. } => None,
        }
    }

    pub(super) fn interrupt(&self) -> Result<(), SandboxError> {
        if self.try_status().is_some() {
            return Ok(());
        }
        codex_utils_pty::process_group::interrupt_process_group(self.process_group_id)
            .map_err(|source| SandboxError::io("interrupting sandboxed process", source))
    }

    pub(super) fn terminate(&self) -> Result<(), SandboxError> {
        if self.try_status().is_some() {
            return Ok(());
        }
        terminate_process_group(self.process_group_id)
            .map_err(|source| SandboxError::io("terminating sandboxed process", source))
    }
}

impl Drop for UnixProcess {
    fn drop(&mut self) {
        if self.try_status().is_none() {
            let _ = terminate_process_group(self.process_group_id);
        }
    }
}

#[cfg(target_os = "macos")]
fn terminate_process_group(process_group_id: u32) -> io::Result<()> {
    codex_utils_pty::process_group::kill_process_group_with_member_fallback(process_group_id)
}

#[cfg(not(target_os = "macos"))]
fn terminate_process_group(process_group_id: u32) -> io::Result<()> {
    codex_utils_pty::process_group::kill_process_group(process_group_id)
}

pub(crate) async fn spawn_unix(
    mut command: Command,
    backend: SandboxBackend,
    runtime: Arc<RuntimeInner>,
    stdin_mode: ChildStdinMode,
) -> Result<SandboxedChild, SandboxError> {
    match stdin_mode {
        ChildStdinMode::Open => command.stdin(Stdio::piped()),
        ChildStdinMode::Closed => command.stdin(Stdio::null()),
    };
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    #[cfg(target_os = "linux")]
    let parent_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            codex_utils_pty::process_group::detach_from_tty()?;
            #[cfg(target_os = "linux")]
            codex_utils_pty::process_group::set_parent_death_signal(parent_pid)?;
            close_inherited_fds_before_exec()?;
            Ok(())
        });
    }

    let mut child = command.spawn().map_err(|source| SandboxError::Spawn {
        backend,
        message: "failed to launch sandboxed process".to_string(),
        source: Some(Box::new(source)),
    })?;
    let process_group_id = child.id().ok_or_else(|| SandboxError::Spawn {
        backend,
        message: "sandboxed process did not expose a process ID".to_string(),
        source: None,
    })?;
    let stdin = child.stdin.take().map(StdinInner::Unix);
    let stdout = child.stdout.take().ok_or_else(|| SandboxError::Spawn {
        backend,
        message: "sandboxed process did not expose stdout".to_string(),
        source: None,
    })?;
    let stderr = child.stderr.take().ok_or_else(|| SandboxError::Spawn {
        backend,
        message: "sandboxed process did not expose stderr".to_string(),
        source: None,
    })?;
    let (completion_tx, completion) = watch::channel(ProcessCompletion::Running);
    let runtime_lease = Arc::clone(&runtime);
    tokio::spawn(async move {
        let _runtime_lease = runtime_lease;
        let completion = match child.wait().await {
            Ok(status) => ProcessCompletion::Exited(SandboxExitStatus::from_native(status)),
            Err(source) => ProcessCompletion::Failed {
                kind: source.kind(),
                message: source.to_string(),
            },
        };
        completion_tx.send_replace(completion);
    });
    let process = ProcessLease::Unix(Arc::new(UnixProcess {
        process_group_id,
        completion,
        _runtime: runtime,
    }));

    Ok(SandboxedChild {
        backend,
        stdin: stdin.map(|stdin| SandboxedStdin {
            inner: Some(stdin),
            _process: process.clone(),
        }),
        stdout: Some(SandboxedOutput {
            inner: OutputInner::Unix(Box::new(stdout)),
            _process: process.clone(),
            operation: "reading sandboxed process stdout",
        }),
        stderr: Some(SandboxedOutput {
            inner: OutputInner::Unix(Box::new(stderr)),
            _process: process.clone(),
            operation: "reading sandboxed process stderr",
        }),
        process,
    })
}

#[cfg(target_os = "linux")]
fn close_inherited_fds_before_exec() -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            (libc::STDERR_FILENO + 1) as libc::c_uint,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn close_inherited_fds_before_exec() -> io::Result<()> {
    codex_utils_pty::pty::close_inherited_fds_except_checked(&[])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn close_inherited_fds_before_exec() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "inherited descriptor cleanup is unavailable on this platform",
    ))
}
