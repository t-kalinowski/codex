use super::*;
use crate::SandboxStdio;
use crate::SandboxStdioMode;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Command;

const ROOT_OBSERVATION_INTERVAL: Duration = Duration::from_millis(5);
const ROOT_REAP_TIMEOUT: Duration = Duration::from_secs(1);

impl SandboxExitStatus {
    fn from_native(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            signal: status.signal(),
        }
    }

    #[cfg(target_os = "macos")]
    fn from_observed(status: codex_utils_pty::process_group::ProcessExitStatus) -> Self {
        match status {
            codex_utils_pty::process_group::ProcessExitStatus::Exited(code) => Self {
                code: Some(code),
                signal: None,
            },
            codex_utils_pty::process_group::ProcessExitStatus::Signaled(signal) => Self {
                code: None,
                signal: Some(signal),
            },
        }
    }
}

enum UnixProcessMode {
    RootProcess {
        state: Arc<Mutex<RootUnixState>>,
    },
    #[cfg(target_os = "macos")]
    SupervisedProcessTree {
        state: Arc<Mutex<SupervisedUnixState>>,
    },
}

struct RootUnixState {
    process_id: u32,
    child: tokio::process::Child,
    completion: ProcessCompletion,
    completion_tx: watch::Sender<ProcessCompletion>,
}

pub(super) struct UnixProcess {
    mode: UnixProcessMode,
    completion: watch::Receiver<ProcessCompletion>,
    _runtime: Arc<RuntimeInner>,
}

#[cfg(target_os = "macos")]
struct SupervisedUnixState {
    process_id: u32,
    child: Option<tokio::process::Child>,
    root_completion: ProcessCompletion,
    retirement: Option<ProcessCompletion>,
    completion_tx: watch::Sender<ProcessCompletion>,
}

fn observe_root_process(state: &mut RootUnixState) {
    if !matches!(state.completion, ProcessCompletion::Running) {
        return;
    }
    let completion = match state.child.try_wait() {
        Ok(Some(status)) => Some(ProcessCompletion::Exited(SandboxExitStatus::from_native(
            status,
        ))),
        Ok(None) => None,
        Err(source) => Some(ProcessCompletion::Failed {
            kind: source.kind(),
            message: source.to_string(),
        }),
    };
    if let Some(completion) = completion {
        state.completion = completion.clone();
        state.completion_tx.send_replace(completion);
    }
}

fn reap_root_process_after_termination(state: &mut RootUnixState) {
    let deadline = Instant::now() + ROOT_REAP_TIMEOUT;
    while matches!(state.completion, ProcessCompletion::Running) && Instant::now() < deadline {
        observe_root_process(state);
        if matches!(state.completion, ProcessCompletion::Running) {
            std::thread::sleep(ROOT_OBSERVATION_INTERVAL);
        }
    }
}

#[cfg(target_os = "macos")]
fn observe_supervised_process(state: &mut SupervisedUnixState) {
    if !matches!(state.root_completion, ProcessCompletion::Running) {
        return;
    }
    let completion =
        match codex_utils_pty::process_group::try_process_exit_without_reaping(state.process_id) {
            Ok(Some(status)) => Some(ProcessCompletion::Exited(SandboxExitStatus::from_observed(
                status,
            ))),
            Ok(None) => None,
            Err(source) => Some(ProcessCompletion::Failed {
                kind: source.kind(),
                message: source.to_string(),
            }),
        };
    if let Some(completion) = completion {
        state.root_completion = completion.clone();
        state.completion_tx.send_replace(completion);
    }
}

impl UnixProcess {
    pub(super) async fn wait(&self) -> Result<SandboxExitStatus, SandboxError> {
        let completion = self.completion.clone();
        loop {
            self.observe_root_completion()?;
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
            tokio::time::sleep(ROOT_OBSERVATION_INTERVAL).await;
        }
    }

    fn observe_root_completion(&self) -> Result<(), SandboxError> {
        match &self.mode {
            UnixProcessMode::RootProcess { state } => {
                let mut state = state.lock().map_err(|_| {
                    SandboxError::io(
                        "observing sandboxed process root",
                        io::Error::other("root process state lock poisoned"),
                    )
                })?;
                observe_root_process(&mut state);
            }
            #[cfg(target_os = "macos")]
            UnixProcessMode::SupervisedProcessTree { state } => {
                let mut state = match state.try_lock() {
                    Ok(state) => state,
                    Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err(SandboxError::io(
                            "observing sandboxed process root",
                            io::Error::other("supervised process state lock poisoned"),
                        ));
                    }
                };
                observe_supervised_process(&mut state);
            }
        }
        Ok(())
    }

    pub(super) fn try_status(&self) -> Result<Option<SandboxExitStatus>, SandboxError> {
        self.observe_root_completion()?;
        completion_status(
            &self.completion.borrow(),
            "observing sandboxed process root",
        )
    }

    pub(super) fn interrupt(&self) -> Result<(), SandboxError> {
        match &self.mode {
            UnixProcessMode::RootProcess { state } => {
                let mut state = state.lock().map_err(|_| {
                    SandboxError::io(
                        "interrupting sandboxed process",
                        io::Error::other("root process state lock poisoned"),
                    )
                })?;
                observe_root_process(&mut state);
                if !matches!(state.completion, ProcessCompletion::Running) {
                    return Ok(());
                }
                signal_process(state.process_id, libc::SIGINT)
                    .map_err(|source| SandboxError::io("interrupting sandboxed process", source))
            }
            #[cfg(target_os = "macos")]
            UnixProcessMode::SupervisedProcessTree { state } => {
                let state = state.lock().map_err(|_| {
                    SandboxError::io(
                        "interrupting sandboxed process",
                        io::Error::other("supervised process state lock poisoned"),
                    )
                })?;
                if state.retirement.is_some() {
                    return Ok(());
                }
                codex_utils_pty::process_group::interrupt_process_group(state.process_id)
                    .map_err(|source| SandboxError::io("interrupting sandboxed process", source))
            }
        }
    }

    pub(super) fn terminate(&self) -> Result<(), SandboxError> {
        match &self.mode {
            UnixProcessMode::RootProcess { state } => {
                let mut state = state.lock().map_err(|_| {
                    SandboxError::io(
                        "terminating sandboxed process",
                        io::Error::other("root process state lock poisoned"),
                    )
                })?;
                observe_root_process(&mut state);
                if !matches!(state.completion, ProcessCompletion::Running) {
                    return Ok(());
                }
                signal_process(state.process_id, libc::SIGKILL)
                    .map_err(|source| SandboxError::io("terminating sandboxed process", source))
            }
            #[cfg(target_os = "macos")]
            UnixProcessMode::SupervisedProcessTree { state } => {
                let state = state.lock().map_err(|_| {
                    SandboxError::io(
                        "terminating sandboxed process",
                        io::Error::other("supervised process state lock poisoned"),
                    )
                })?;
                if state.retirement.is_some() {
                    return Ok(());
                }
                terminate_process_group(state.process_id)
                    .map_err(|source| SandboxError::io("terminating sandboxed process", source))
            }
        }
    }

    pub(super) async fn retire(&self) -> Result<SandboxExitStatus, SandboxError> {
        match &self.mode {
            UnixProcessMode::RootProcess { .. } => self.wait().await,
            #[cfg(target_os = "macos")]
            UnixProcessMode::SupervisedProcessTree { state } => {
                let state = Arc::clone(state);
                tokio::task::spawn_blocking(move || retire_supervised_process(&state))
                    .await
                    .map_err(|source| {
                        SandboxError::io(
                            "retiring sandboxed process group",
                            io::Error::other(source.to_string()),
                        )
                    })?
            }
        }
    }
}

impl Drop for UnixProcess {
    fn drop(&mut self) {
        match &self.mode {
            UnixProcessMode::RootProcess { state } => {
                if let Ok(mut state) = state.lock() {
                    observe_root_process(&mut state);
                    if matches!(state.completion, ProcessCompletion::Running) {
                        let _ = signal_process(state.process_id, libc::SIGKILL);
                        reap_root_process_after_termination(&mut state);
                    }
                }
            }
            #[cfg(target_os = "macos")]
            UnixProcessMode::SupervisedProcessTree { state } => {
                if retire_supervised_process(state).is_err() {
                    if let Ok(state) = state.lock() {
                        let _ = signal_process(state.process_id, libc::SIGSTOP);
                    }
                    let _ = retire_supervised_process(state);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn retire_supervised_process(
    state: &Arc<Mutex<SupervisedUnixState>>,
) -> Result<SandboxExitStatus, SandboxError> {
    let mut state = state.lock().map_err(|_| {
        SandboxError::io(
            "retiring sandboxed process group",
            io::Error::other("supervised process state lock poisoned"),
        )
    })?;
    if let Some(completion) = &state.retirement {
        return completion_result(completion, "retiring sandboxed process group");
    }

    terminate_process_group(state.process_id)
        .map_err(|source| SandboxError::io("quiescing sandboxed process group", source))?;
    if !matches!(state.root_completion, ProcessCompletion::Exited(_)) {
        state.root_completion =
            match codex_utils_pty::process_group::wait_for_process_exit_without_reaping(
                state.process_id,
            ) {
                Ok(status) => ProcessCompletion::Exited(SandboxExitStatus::from_observed(status)),
                Err(source) => ProcessCompletion::Failed {
                    kind: source.kind(),
                    message: source.to_string(),
                },
            };
        state
            .completion_tx
            .send_replace(state.root_completion.clone());
    }
    let observed = completion_result(&state.root_completion, "observing sandboxed process root")?;
    let reaped = match state.child.as_mut() {
        Some(child) => reap_supervised_root(child),
        None => Err(SandboxError::io(
            "reaping sandboxed process root",
            io::Error::new(
                io::ErrorKind::InvalidData,
                "supervised process root already consumed",
            ),
        )),
    }?;
    state.child.take();

    let retirement = if observed == reaped {
        ProcessCompletion::Exited(observed)
    } else {
        ProcessCompletion::Failed {
            kind: io::ErrorKind::InvalidData,
            message: format!(
                "observed root status {observed:?} contradicted reaped status {reaped:?}"
            ),
        }
    };
    state.retirement = Some(retirement.clone());
    completion_result(&retirement, "retiring sandboxed process group")
}

#[cfg(target_os = "macos")]
fn reap_supervised_root(
    child: &mut tokio::process::Child,
) -> Result<SandboxExitStatus, SandboxError> {
    let deadline = Instant::now() + ROOT_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(SandboxExitStatus::from_native(status)),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(ROOT_OBSERVATION_INTERVAL);
            }
            Ok(None) => {
                return Err(SandboxError::io(
                    "reaping sandboxed process root",
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "supervised process root remained waitable without an exit status",
                    ),
                ));
            }
            Err(source) => {
                return Err(SandboxError::io("reaping sandboxed process root", source));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn completion_result(
    completion: &ProcessCompletion,
    operation: &'static str,
) -> Result<SandboxExitStatus, SandboxError> {
    match completion {
        ProcessCompletion::Exited(status) => Ok(*status),
        ProcessCompletion::Failed { kind, message } => Err(SandboxError::io(
            operation,
            io::Error::new(*kind, message.clone()),
        )),
        ProcessCompletion::Running => Err(SandboxError::io(
            operation,
            io::Error::new(io::ErrorKind::WouldBlock, "process is still running"),
        )),
    }
}

fn completion_status(
    completion: &ProcessCompletion,
    operation: &'static str,
) -> Result<Option<SandboxExitStatus>, SandboxError> {
    match completion {
        ProcessCompletion::Exited(status) => Ok(Some(*status)),
        ProcessCompletion::Running => Ok(None),
        ProcessCompletion::Failed { kind, message } => Err(SandboxError::io(
            operation,
            io::Error::new(*kind, message.clone()),
        )),
    }
}

fn signal_process(process_id: u32, signal: libc::c_int) -> io::Result<()> {
    let process_id = libc::pid_t::try_from(process_id)
        .ok()
        .filter(|process_id| *process_id > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process ID"))?;
    if unsafe { libc::kill(process_id, signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn terminate_process_group(process_group_id: u32) -> io::Result<()> {
    codex_utils_pty::process_group::kill_process_group_until_quiescent(process_group_id)
}

pub(crate) async fn spawn_unix(
    mut command: Command,
    backend: SandboxBackend,
    runtime: Arc<RuntimeInner>,
    stdio: SandboxStdio,
    lifetime: SandboxLifetime,
) -> Result<SandboxedChild, SandboxError> {
    command.stdin(native_stdio(stdio.stdin));
    command.stdout(native_stdio(stdio.stdout));
    command.stderr(native_stdio(stdio.stderr));
    command.kill_on_drop(true);

    #[cfg(target_os = "linux")]
    let parent_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if lifetime == SandboxLifetime::SupervisedProcessTree {
                codex_utils_pty::process_group::detach_from_tty()?;
            }
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
    let process_id = child.id().ok_or_else(|| SandboxError::Spawn {
        backend,
        message: "sandboxed process did not expose a process ID".to_string(),
        source: None,
    })?;
    let stdin = child.stdin.take().map(StdinInner::Unix);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (completion_tx, completion) = watch::channel(ProcessCompletion::Running);
    let mode = match lifetime {
        SandboxLifetime::BackendDefault => UnixProcessMode::RootProcess {
            state: Arc::new(Mutex::new(RootUnixState {
                process_id,
                child,
                completion: ProcessCompletion::Running,
                completion_tx,
            })),
        },
        #[cfg(target_os = "macos")]
        SandboxLifetime::SupervisedProcessTree => {
            let state = Arc::new(Mutex::new(SupervisedUnixState {
                process_id,
                child: Some(child),
                root_completion: ProcessCompletion::Running,
                retirement: None,
                completion_tx,
            }));
            UnixProcessMode::SupervisedProcessTree { state }
        }
        #[cfg(not(target_os = "macos"))]
        SandboxLifetime::SupervisedProcessTree => {
            return Err(SandboxError::UnsupportedPolicy {
                backend,
                feature: crate::SandboxFeature::ProcessTreeTermination,
                message: "durable supervised process-tree ownership is unavailable".to_string(),
            });
        }
    };
    let process = ProcessLease::Unix(Arc::new(UnixProcess {
        mode,
        completion,
        _runtime: runtime,
    }));
    let process_controller = SandboxedProcess {
        backend,
        lifetime,
        process: process.clone(),
    };

    Ok(SandboxedChild {
        stdin: stdin.map(|stdin| SandboxedStdin {
            inner: Some(stdin),
            _process: process.clone(),
        }),
        stdout: stdout.map(|stdout| SandboxedOutput {
            inner: OutputInner::Unix(Box::new(stdout)),
            _process: process.clone(),
            operation: "reading sandboxed process stdout",
        }),
        stderr: stderr.map(|stderr| SandboxedOutput {
            inner: OutputInner::Unix(Box::new(stderr)),
            _process: process.clone(),
            operation: "reading sandboxed process stderr",
        }),
        process: process_controller,
    })
}

fn native_stdio(mode: SandboxStdioMode) -> Stdio {
    match mode {
        SandboxStdioMode::Inherit => Stdio::inherit(),
        SandboxStdioMode::Pipe => Stdio::piped(),
        SandboxStdioMode::Null => Stdio::null(),
    }
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
