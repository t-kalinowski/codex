use super::*;
use codex_windows_sandbox::WindowsSandboxEmbeddingProcess;
use codex_windows_sandbox::WindowsSandboxEmbeddingProcessHandle;

impl SandboxExitStatus {
    fn from_code(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }
}

pub(super) struct WindowsProcess {
    session: Arc<WindowsSandboxEmbeddingProcessHandle>,
    completion: watch::Receiver<ProcessCompletion>,
    _runtime: Arc<RuntimeInner>,
}

impl WindowsProcess {
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
        if let ProcessCompletion::Exited(status) = *self.completion.borrow() {
            return Some(status);
        }
        self.session.exit_code().map(SandboxExitStatus::from_code)
    }

    pub(super) fn terminate(&self) -> Result<(), SandboxError> {
        self.session
            .terminate()
            .map_err(|source| SandboxError::io("terminating sandboxed process", source))
    }
}

impl Drop for WindowsProcess {
    fn drop(&mut self) {
        let _ = self.session.terminate();
    }
}

impl SandboxedChild {
    pub(crate) fn from_windows(
        spawned: WindowsSandboxEmbeddingProcess,
        backend: SandboxBackend,
        runtime: Arc<RuntimeInner>,
        stdin_mode: ChildStdinMode,
    ) -> Self {
        let WindowsSandboxEmbeddingProcess {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;
        let session = Arc::new(session);
        let (completion_tx, completion) = watch::channel(ProcessCompletion::Running);
        let runtime_lease = Arc::clone(&runtime);
        tokio::spawn(async move {
            let _runtime_lease = runtime_lease;
            let completion = match exit_rx.await {
                Ok(code) => ProcessCompletion::Exited(SandboxExitStatus::from_code(code)),
                Err(_) => ProcessCompletion::Failed {
                    kind: io::ErrorKind::BrokenPipe,
                    message: "process status channel closed".to_string(),
                },
            };
            completion_tx.send_replace(completion);
        });
        let process = ProcessLease::Windows(Arc::new(WindowsProcess {
            session: Arc::clone(&session),
            completion,
            _runtime: runtime,
        }));
        let stdin = match stdin_mode {
            ChildStdinMode::Open => Some(SandboxedStdin {
                inner: Some(StdinInner::Windows(Arc::clone(&session))),
                _process: process.clone(),
            }),
            ChildStdinMode::Closed => {
                session.close_stdin_without_waiting();
                None
            }
        };
        Self {
            backend,
            stdin,
            stdout: Some(SandboxedOutput {
                inner: OutputInner::Windows(stdout_rx),
                _process: process.clone(),
            }),
            stderr: Some(SandboxedOutput {
                inner: OutputInner::Windows(stderr_rx),
                _process: process.clone(),
            }),
            process,
        }
    }
}
