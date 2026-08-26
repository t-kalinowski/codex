use crate::SandboxBackend;
use crate::SandboxError;
#[cfg(windows)]
use crate::SandboxFeature;
use crate::SandboxLifetime;
use crate::runtime::RuntimeInner;
use std::io;
use std::sync::Arc;

#[cfg(windows)]
use codex_windows_sandbox::WindowsSandboxEmbeddingProcessHandle;
#[cfg(unix)]
use tokio::io::AsyncRead;
#[cfg(unix)]
use tokio::io::AsyncReadExt;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
#[cfg(windows)]
use tokio::sync::mpsc;
use tokio::sync::watch;

#[cfg(unix)]
const OUTPUT_CHUNK_SIZE: usize = 8 * 1024;

#[cfg(unix)]
#[path = "process/unix.rs"]
mod unix;
#[cfg(unix)]
pub(crate) use unix::spawn_unix;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod windows;

/// Normalized result of a sandboxed process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SandboxExitStatus {
    code: Option<i32>,
    signal: Option<i32>,
}

impl SandboxExitStatus {
    /// Returns the ordinary process exit code, or `None` when a signal ended the process.
    pub fn code(self) -> Option<i32> {
        self.code
    }

    /// Returns the terminating Unix signal when the native process API exposes one.
    pub fn signal(self) -> Option<i32> {
        self.signal
    }

    /// Returns whether the process completed normally with exit code zero.
    pub fn success(self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }
}

#[derive(Clone, Debug)]
enum ProcessCompletion {
    Running,
    Exited(SandboxExitStatus),
    Failed {
        kind: io::ErrorKind,
        message: String,
    },
}

#[derive(Clone)]
enum ProcessLease {
    #[cfg(unix)]
    Unix(Arc<unix::UnixProcess>),
    #[cfg(windows)]
    Windows(Arc<windows::WindowsProcess>),
}

impl ProcessLease {
    async fn wait(&self) -> Result<SandboxExitStatus, SandboxError> {
        match self {
            #[cfg(unix)]
            Self::Unix(process) => process.wait().await,
            #[cfg(windows)]
            Self::Windows(process) => process.wait().await,
        }
    }

    async fn retire(&self) -> Result<SandboxExitStatus, SandboxError> {
        match self {
            #[cfg(unix)]
            Self::Unix(process) => process.retire().await,
            #[cfg(windows)]
            Self::Windows(process) => process.retire().await,
        }
    }

    fn try_status(&self) -> Result<Option<SandboxExitStatus>, SandboxError> {
        match self {
            #[cfg(unix)]
            Self::Unix(process) => process.try_status(),
            #[cfg(windows)]
            Self::Windows(process) => process.try_status(),
        }
    }

    fn interrupt(&self, backend: SandboxBackend) -> Result<(), SandboxError> {
        #[cfg(unix)]
        let _ = backend;
        match self {
            #[cfg(unix)]
            Self::Unix(process) => process.interrupt(),
            #[cfg(windows)]
            Self::Windows(_) => Err(SandboxError::UnsupportedPolicy {
                backend,
                feature: SandboxFeature::Interrupt,
                message: "the selected Windows process transport does not deliver interrupts"
                    .to_string(),
            }),
        }
    }

    fn terminate(&self) -> Result<(), SandboxError> {
        match self {
            #[cfg(unix)]
            Self::Unix(process) => process.terminate(),
            #[cfg(windows)]
            Self::Windows(process) => process.terminate(),
        }
    }
}

enum StdinInner {
    #[cfg(unix)]
    Unix(tokio::process::ChildStdin),
    #[cfg(windows)]
    Windows(Arc<WindowsSandboxEmbeddingProcessHandle>),
}

/// Writable raw-byte stream connected to a sandboxed process's standard input.
pub struct SandboxedStdin {
    inner: Option<StdinInner>,
    _process: ProcessLease,
}

impl SandboxedStdin {
    /// Writes every byte in `bytes` to the child without text decoding or framing.
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), SandboxError> {
        match self.inner.as_mut() {
            #[cfg(unix)]
            Some(StdinInner::Unix(stdin)) => stdin
                .write_all(bytes)
                .await
                .map_err(|source| SandboxError::io("writing sandboxed process stdin", source)),
            #[cfg(windows)]
            Some(StdinInner::Windows(stdin)) => stdin
                .write_all(bytes)
                .await
                .map_err(|source| SandboxError::io("writing sandboxed process stdin", source)),
            None => Err(SandboxError::io(
                "writing sandboxed process stdin",
                io::Error::new(io::ErrorKind::BrokenPipe, "process stdin closed"),
            )),
        }
    }

    /// Closes the child input stream after all preceding writes.
    pub async fn close(mut self) -> Result<(), SandboxError> {
        let Some(stdin) = self.inner.take() else {
            return Ok(());
        };
        match stdin {
            #[cfg(unix)]
            StdinInner::Unix(mut stdin) => stdin
                .shutdown()
                .await
                .map_err(|source| SandboxError::io("closing sandboxed process stdin", source)),
            #[cfg(windows)]
            StdinInner::Windows(stdin) => stdin
                .close_stdin()
                .await
                .map_err(|source| SandboxError::io("closing sandboxed process stdin", source)),
        }
    }
}

impl Drop for SandboxedStdin {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(StdinInner::Windows(stdin)) = &self.inner {
            stdin.close_stdin_without_waiting();
        }
    }
}

enum OutputInner {
    #[cfg(unix)]
    Unix(Box<dyn AsyncRead + Send + Unpin>),
    #[cfg(windows)]
    Windows(mpsc::Receiver<Vec<u8>>),
}

/// Independently movable raw-byte output stream from a sandboxed process.
pub struct SandboxedOutput {
    inner: OutputInner,
    _process: ProcessLease,
    #[cfg(unix)]
    operation: &'static str,
}

impl SandboxedOutput {
    /// Reads the next available raw byte chunk, returning `None` at end of stream.
    pub async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, SandboxError> {
        match &mut self.inner {
            #[cfg(unix)]
            OutputInner::Unix(output) => {
                let mut bytes = vec![0; OUTPUT_CHUNK_SIZE];
                let count = output
                    .read(&mut bytes)
                    .await
                    .map_err(|source| SandboxError::io(self.operation, source))?;
                if count == 0 {
                    return Ok(None);
                }
                bytes.truncate(count);
                Ok(Some(bytes))
            }
            #[cfg(windows)]
            OutputInner::Windows(output) => Ok(output.recv().await),
        }
    }
}

/// Opaque owner of one sandboxed launch and its optional pipe handles.
pub struct SandboxedChild {
    process: SandboxedProcess,
    stdin: Option<SandboxedStdin>,
    stdout: Option<SandboxedOutput>,
    stderr: Option<SandboxedOutput>,
}

impl SandboxedChild {
    /// Takes the writable raw-byte input stream when stdin was configured as a pipe.
    pub fn take_stdin(&mut self) -> Option<SandboxedStdin> {
        self.stdin.take()
    }

    /// Takes the raw standard-output stream when stdout was configured as a pipe.
    pub fn take_stdout(&mut self) -> Option<SandboxedOutput> {
        self.stdout.take()
    }

    /// Takes the raw standard-error stream when stderr was configured as a pipe.
    pub fn take_stderr(&mut self) -> Option<SandboxedOutput> {
        self.stderr.take()
    }

    /// Returns a cloneable controller for observing and retiring the process lifetime.
    pub fn process(&self) -> SandboxedProcess {
        self.process.clone()
    }
}

/// Cloneable controller for one sandboxed process lifetime.
#[derive(Clone)]
pub struct SandboxedProcess {
    backend: SandboxBackend,
    lifetime: SandboxLifetime,
    process: ProcessLease,
}

impl SandboxedProcess {
    /// Observes the direct root's exit without retiring a supervised process group.
    pub async fn wait_root(&self) -> Result<SandboxExitStatus, SandboxError> {
        self.process.wait().await
    }

    /// Polls and returns the direct root status without blocking.
    pub fn try_root_status(&self) -> Result<Option<SandboxExitStatus>, SandboxError> {
        self.process.try_status()
    }

    /// Sends an interrupt when the selected process backend supports it.
    pub fn interrupt(&self) -> Result<(), SandboxError> {
        self.process.interrupt(self.backend)
    }

    /// Requests immediate termination of the owned process or supervised process tree.
    pub fn terminate(&self) -> Result<(), SandboxError> {
        self.process.terminate()
    }

    /// Retires the selected lifetime and returns the root status.
    ///
    /// A supervised lifetime first quiesces the process tree and reaps the root once.
    /// The backend-default lifetime is equivalent to waiting for the direct root.
    pub async fn retire(&self) -> Result<SandboxExitStatus, SandboxError> {
        self.process.retire().await
    }

    /// Returns the native sandbox backend enforcing this process.
    pub fn backend(&self) -> SandboxBackend {
        self.backend
    }

    /// Returns the ownership contract selected for this launch.
    pub fn lifetime(&self) -> SandboxLifetime {
        self.lifetime
    }
}
