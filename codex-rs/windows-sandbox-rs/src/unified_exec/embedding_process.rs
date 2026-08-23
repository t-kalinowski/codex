use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::embedding_acl::EmbeddingAclLease;

type Terminator = Box<dyn FnMut() -> io::Result<()> + Send + Sync>;

pub(crate) enum EmbeddingStdinRequest {
    Write {
        bytes: Vec<u8>,
        completed: oneshot::Sender<io::Result<()>>,
    },
    Close {
        completed: oneshot::Sender<io::Result<()>>,
    },
}

struct EmbeddingProcessInner {
    writer_tx: Mutex<Option<mpsc::Sender<EmbeddingStdinRequest>>>,
    terminator: Mutex<Option<Terminator>>,
    writer_handle: Mutex<Option<JoinHandle<()>>>,
    wait_handle: Mutex<Option<JoinHandle<()>>>,
    exit_status: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    acl_lease: Mutex<Option<EmbeddingAclLease>>,
    _session_state: Arc<TempDir>,
}

impl Drop for EmbeddingProcessInner {
    fn drop(&mut self) {
        if let Ok(writer_tx) = self.writer_tx.get_mut() {
            writer_tx.take();
        }
        let terminated = match self.terminator.get_mut() {
            Ok(terminator) => terminator
                .take()
                .is_none_or(|mut terminator| terminator().is_ok()),
            Err(_) => false,
        };
        if let Ok(writer_handle) = self.writer_handle.get_mut()
            && let Some(writer_handle) = writer_handle.take()
        {
            writer_handle.abort();
        }
        if let Ok(wait_handle) = self.wait_handle.get_mut() {
            wait_handle.take();
        }
        if let Ok(acl_lease) = self.acl_lease.get_mut()
            && let Some(acl_lease) = acl_lease.take()
            && !terminated
        {
            std::mem::forget(acl_lease);
        }
    }
}

/// Controller for a restricted-token process launched through the embedding path.
#[derive(Clone)]
pub struct WindowsSandboxEmbeddingProcessHandle {
    inner: Arc<EmbeddingProcessInner>,
}

impl fmt::Debug for WindowsSandboxEmbeddingProcessHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsSandboxEmbeddingProcessHandle")
            .finish()
    }
}

impl WindowsSandboxEmbeddingProcessHandle {
    /// Writes exact bytes and returns only after the pipe write completes.
    pub async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let writer_tx = self
            .inner
            .writer_tx
            .lock()
            .map_err(|_| io::Error::other("embedding stdin lock poisoned"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "process stdin closed"))?;
        let (completed, completion) = oneshot::channel();
        writer_tx
            .send(EmbeddingStdinRequest::Write {
                bytes: bytes.to_vec(),
                completed,
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "process stdin closed"))?;
        completion.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process stdin writer stopped before acknowledging the write",
            )
        })?
    }

    /// Closes the child stdin after all preceding acknowledged writes.
    pub async fn close_stdin(&self) -> io::Result<()> {
        let writer_tx = self
            .inner
            .writer_tx
            .lock()
            .map_err(|_| io::Error::other("embedding stdin lock poisoned"))?
            .take();
        let Some(writer_tx) = writer_tx else {
            return Ok(());
        };
        let (completed, completion) = oneshot::channel();
        writer_tx
            .send(EmbeddingStdinRequest::Close { completed })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "process stdin closed"))?;
        completion.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process stdin writer stopped before acknowledging close",
            )
        })?
    }

    /// Stops accepting input without waiting for the writer task to close the pipe.
    pub fn close_stdin_without_waiting(&self) {
        if let Ok(mut writer_tx) = self.inner.writer_tx.lock() {
            writer_tx.take();
        }
    }

    /// Returns whether process completion has been reported.
    pub fn has_exited(&self) -> bool {
        self.inner.exit_status.load(Ordering::SeqCst)
    }

    /// Returns the root process exit code when known.
    pub fn exit_code(&self) -> Option<i32> {
        self.inner.exit_code.lock().ok().and_then(|code| *code)
    }

    /// Terminates the retained process job, including descendants after the root exits.
    pub fn terminate(&self) -> io::Result<()> {
        let mut terminator = self
            .inner
            .terminator
            .lock()
            .map_err(|_| io::Error::other("embedding process terminator lock poisoned"))?;
        let Some(mut terminate) = terminator.take() else {
            return Ok(());
        };
        if let Err(error) = terminate() {
            *terminator = Some(terminate);
            return Err(error);
        }
        Ok(())
    }
}

/// Restricted-token child transport used by the embedding facade.
#[derive(Debug)]
pub struct WindowsSandboxEmbeddingProcess {
    pub session: WindowsSandboxEmbeddingProcessHandle,
    pub stdout_rx: mpsc::Receiver<Vec<u8>>,
    pub stderr_rx: mpsc::Receiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<i32>,
}

impl WindowsSandboxEmbeddingProcess {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        writer_tx: Option<mpsc::Sender<EmbeddingStdinRequest>>,
        stdout_rx: mpsc::Receiver<Vec<u8>>,
        stderr_rx: mpsc::Receiver<Vec<u8>>,
        driver_exit_rx: oneshot::Receiver<i32>,
        terminator: Terminator,
        writer_handle: JoinHandle<()>,
        session_state: TempDir,
        acl_lease: EmbeddingAclLease,
    ) -> Self {
        let (exit_tx, exit_rx) = oneshot::channel();
        let exit_status = Arc::new(AtomicBool::new(false));
        let wait_exit_status = Arc::clone(&exit_status);
        let exit_code = Arc::new(Mutex::new(None));
        let wait_exit_code = Arc::clone(&exit_code);
        let session_state = Arc::new(session_state);
        let wait_session_state = Arc::clone(&session_state);
        let wait_handle = tokio::spawn(async move {
            let _session_state = wait_session_state;
            let code = driver_exit_rx.await.unwrap_or(-1);
            wait_exit_status.store(true, Ordering::SeqCst);
            if let Ok(mut exit_code) = wait_exit_code.lock() {
                *exit_code = Some(code);
            }
            let _ = exit_tx.send(code);
        });
        let session = WindowsSandboxEmbeddingProcessHandle {
            inner: Arc::new(EmbeddingProcessInner {
                writer_tx: Mutex::new(writer_tx),
                terminator: Mutex::new(Some(terminator)),
                writer_handle: Mutex::new(Some(writer_handle)),
                wait_handle: Mutex::new(Some(wait_handle)),
                exit_status,
                exit_code,
                acl_lease: Mutex::new(Some(acl_lease)),
                _session_state: session_state,
            }),
        };
        Self {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        }
    }
}
