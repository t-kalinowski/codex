#![cfg(unix)]

use crate::launch_bridge::TargetCompletion;
use crate::protocol::FinalOutcome;
use crate::protocol::InfrastructureOutcome;
use crate::protocol::LifecyclePolicy;
use crate::protocol::RetirementOutcome;
use crate::protocol::RunnerPhase;
use crate::protocol::RunnerStatus;
use crate::protocol::StopDeadlines;
use crate::protocol::TargetOutcome;
use crate::protocol::TargetOutcomeKind;
use anyhow::Context;
use anyhow::Result;
use codex_network_proxy::NetworkProxyHandle;
#[cfg(not(target_os = "macos"))]
use std::os::unix::process::ExitStatusExt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::process::Child;
#[cfg(not(target_os = "macos"))]
use tokio::sync::Notify;
use tokio::sync::watch;

#[cfg(target_os = "macos")]
const REAP_ALLOWANCE: Duration = Duration::from_secs(1);

#[cfg(not(target_os = "macos"))]
use crate::cleanup::CleanupDirectory;
#[cfg(target_os = "macos")]
use crate::lifetime::LifetimeControl;
#[cfg(target_os = "macos")]
use crate::lifetime::LifetimeManager;
#[cfg(target_os = "macos")]
use crate::lifetime::stop_phase_timeout;
#[cfg(target_os = "macos")]
use crate::stdio::ForegroundTerminal;

struct StopState {
    requested: AtomicBool,
    #[cfg(target_os = "macos")]
    control_lost: AtomicBool,
    #[cfg(not(target_os = "macos"))]
    forced: AtomicBool,
    #[cfg(not(target_os = "macos"))]
    force_at: Mutex<Option<tokio::time::Instant>>,
    #[cfg(not(target_os = "macos"))]
    force_timeout: Mutex<Option<Duration>>,
    #[cfg(target_os = "macos")]
    termination_timeout: Mutex<Option<Duration>>,
    #[cfg(not(target_os = "macos"))]
    wakeup: Notify,
}

pub struct Supervisor {
    process_group_id: Arc<Mutex<Option<u32>>>,
    #[cfg(not(target_os = "macos"))]
    force_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    lifecycle: LifecyclePolicy,
    stop: Arc<StopState>,
    status: watch::Receiver<RunnerStatus>,
    final_outcome: watch::Receiver<Option<FinalOutcome>>,
    #[cfg(target_os = "macos")]
    lifetime_control: LifetimeControl,
    #[cfg(target_os = "macos")]
    reap_release: watch::Sender<bool>,
    #[cfg(target_os = "macos")]
    reap_result: watch::Receiver<Option<Result<(), String>>>,
}

impl Supervisor {
    pub(crate) fn start(
        child: Child,
        process_group_id: u32,
        lifecycle: LifecyclePolicy,
        proxy_handle: Option<NetworkProxyHandle>,
        target_completion: TargetCompletion,
        #[cfg(target_os = "macos")] lifetime_manager: LifetimeManager,
        #[cfg(target_os = "macos")] foreground_terminal: Option<ForegroundTerminal>,
        #[cfg(not(target_os = "macos"))] cleanup_directory: CleanupDirectory,
    ) -> Self {
        #[cfg(target_os = "macos")]
        let target_process_id = process_group_id;
        let process_group_id = Arc::new(Mutex::new(Some(process_group_id)));
        #[cfg(not(target_os = "macos"))]
        let force_task = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<()>>));
        let stop = Arc::new(StopState {
            requested: AtomicBool::new(false),
            #[cfg(target_os = "macos")]
            control_lost: AtomicBool::new(false),
            #[cfg(not(target_os = "macos"))]
            forced: AtomicBool::new(false),
            #[cfg(not(target_os = "macos"))]
            force_at: Mutex::new(None),
            #[cfg(not(target_os = "macos"))]
            force_timeout: Mutex::new(None),
            #[cfg(target_os = "macos")]
            termination_timeout: Mutex::new(None),
            #[cfg(not(target_os = "macos"))]
            wakeup: Notify::new(),
        });
        let (status_sender, status) = watch::channel(RunnerStatus {
            phase: RunnerPhase::Running,
            target: None,
            retirement: None,
        });
        let (final_sender, final_outcome) = watch::channel(None);
        #[cfg(target_os = "macos")]
        let (reap_release, mut task_reap_release) = watch::channel(false);
        #[cfg(target_os = "macos")]
        let (reap_result_sender, reap_result) = watch::channel(None);
        let task_process_group_id = Arc::clone(&process_group_id);
        #[cfg(not(target_os = "macos"))]
        let task_force_task = Arc::clone(&force_task);
        let task_stop = Arc::clone(&stop);
        let task_lifecycle = lifecycle.clone();
        #[cfg(target_os = "macos")]
        let supervisor_lifetime_control = lifetime_manager.control();
        tokio::spawn(async move {
            let mut child = child;
            #[cfg(target_os = "macos")]
            let target_status = observe_child_without_reaping(target_process_id);
            #[cfg(not(target_os = "macos"))]
            let target_status = wait_child(&mut child);
            let (target_status, exec_status) =
                tokio::join!(target_status, target_completion.wait());
            let (target, infrastructure_error) = target_observation(target_status, exec_status);
            #[cfg(target_os = "macos")]
            let mut cleanup_error = foreground_terminal
                .and_then(|terminal| terminal.restore().err())
                .map(|error| format!("failed to restore the foreground terminal: {error}"));
            #[cfg(not(target_os = "macos"))]
            let mut cleanup_error = None;
            status_sender.send_replace(RunnerStatus {
                phase: RunnerPhase::RootExited,
                target: target.clone(),
                retirement: None,
            });
            #[cfg(not(target_os = "macos"))]
            let mut retirement = retire_process_group(
                process_group_id_value(&task_process_group_id),
                &task_lifecycle,
                &task_stop,
            )
            .await;
            #[cfg(target_os = "macos")]
            let mut retirement = complete_retirement(/*forced*/ false);
            if task_stop.requested.load(Ordering::Acquire)
                && target
                    .as_ref()
                    .is_some_and(|target| target.signal == Some(libc::SIGKILL))
            {
                retirement.forced = true;
            }
            #[cfg(target_os = "macos")]
            match if task_stop.control_lost.load(Ordering::Acquire) {
                lifetime_manager.stop()
            } else if task_stop.requested.load(Ordering::Acquire) {
                let timeout = task_stop
                    .termination_timeout
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .unwrap_or_else(|| Duration::from_millis(task_lifecycle.force_timeout_ms));
                lifetime_manager.finish_with_timeout(timeout)
            } else {
                lifetime_manager.finish()
            } {
                Ok(outcome) => {
                    retirement.forced |= outcome.forced;
                    cleanup_error = combine_errors(cleanup_error, outcome.cleanup_error);
                }
                Err(error) => {
                    retirement.complete = false;
                    retirement.error = Some(match retirement.error.take() {
                        Some(prior) => format!("{prior}; additionally, {error}"),
                        None => error,
                    });
                }
            }
            #[cfg(not(target_os = "macos"))]
            if retirement.complete {
                cleanup_error = cleanup_directory
                    .remove()
                    .err()
                    .map(|error| error.to_string());
            } else {
                cleanup_directory.preserve();
            }
            #[cfg(not(target_os = "macos"))]
            {
                *task_process_group_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            }
            #[cfg(not(target_os = "macos"))]
            {
                if let Some(task) = task_force_task
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    task.abort();
                }
            }
            status_sender.send_replace(RunnerStatus {
                phase: RunnerPhase::Retired,
                target: target.clone(),
                retirement: Some(retirement.clone()),
            });
            let proxy_cleanup_error = match proxy_handle {
                Some(handle) => handle.shutdown().await.err().map(|error| error.to_string()),
                None => None,
            };
            let cleanup_error = combine_errors(cleanup_error, proxy_cleanup_error);
            final_sender.send_replace(Some(FinalOutcome {
                target,
                retirement,
                infrastructure: InfrastructureOutcome {
                    error: infrastructure_error,
                    cleanup_error,
                },
            }));
            #[cfg(target_os = "macos")]
            {
                while !*task_reap_release.borrow() {
                    if task_reap_release.changed().await.is_err() {
                        break;
                    }
                }
                let result = child
                    .wait()
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("failed to reap the sandbox target: {error}"));
                *task_process_group_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                reap_result_sender.send_replace(Some(result));
            }
        });
        Self {
            process_group_id,
            #[cfg(not(target_os = "macos"))]
            force_task,
            lifecycle,
            stop,
            status,
            final_outcome,
            #[cfg(target_os = "macos")]
            lifetime_control: supervisor_lifetime_control,
            #[cfg(target_os = "macos")]
            reap_release,
            #[cfg(target_os = "macos")]
            reap_result,
        }
    }

    pub fn status(&self) -> RunnerStatus {
        self.status.borrow().clone()
    }

    pub fn interrupt(&self) -> Result<()> {
        let process_group = self
            .process_group_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(process_group_id) = *process_group else {
            return Ok(());
        };
        let result = codex_utils_pty::process_group::interrupt_process_group(process_group_id)
            .context("target interrupt failed");
        drop(process_group);
        #[cfg(target_os = "macos")]
        if result.is_err()
            && (self.status.borrow().phase != RunnerPhase::Running
                || !process_tree_exists(process_group_id))
        {
            return Ok(());
        }
        result
    }

    pub fn terminate(&self, deadlines: StopDeadlines) -> Result<()> {
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            deadlines.graceful_ms == 0,
            "Linux termination does not support a graceful deadline"
        );
        #[cfg(target_os = "macos")]
        {
            let process_group_id = match *self
                .process_group_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                Some(process_group_id) => process_group_id,
                None => return Ok(()),
            };
            self.stop.requested.store(true, Ordering::Release);
            *self
                .stop
                .termination_timeout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
                Duration::from_millis(deadlines.graceful_ms)
                    .saturating_add(Duration::from_millis(deadlines.force_ms)),
            );
            if let Err(manager_error) = self.lifetime_control.terminate(&deadlines)
                && let Err(signal_error) = signal_terminate(process_group_id)
                && process_tree_exists(process_group_id)
            {
                anyhow::bail!(
                    "target lifetime termination failed: {manager_error}; additionally, {signal_error}"
                );
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            if let Some(task) = self
                .force_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                task.abort();
            }
            let process_group = self
                .process_group_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let process_group_id =
                (*process_group).context("target generation is already retired")?;
            self.stop.requested.store(true, Ordering::Release);
            *self
                .stop
                .force_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(tokio::time::Instant::now() + Duration::from_millis(deadlines.graceful_ms));
            *self
                .stop
                .force_timeout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(Duration::from_millis(deadlines.force_ms));
            self.force(process_group_id)?;
            self.stop.wakeup.notify_one();
            drop(process_group);
            Ok(())
        }
    }

    pub async fn wait(&self, timeout: Duration) -> Result<FinalOutcome> {
        let mut receiver = self.final_outcome.clone();
        if let Some(outcome) = receiver.borrow().clone() {
            return Ok(outcome);
        }
        tokio::time::timeout(timeout, async {
            loop {
                receiver
                    .changed()
                    .await
                    .context("target outcome channel closed")?;
                if let Some(outcome) = receiver.borrow().clone() {
                    return Ok(outcome);
                }
            }
        })
        .await
        .context("target retirement observation timed out")?
    }

    pub async fn retire_on_control_loss(&self) -> Result<FinalOutcome> {
        #[cfg(target_os = "macos")]
        {
            if let Some(outcome) = self.final_outcome.borrow().clone() {
                self.release_root().await?;
                return Ok(outcome);
            }
            let started = tokio::time::Instant::now();
            let phase_timeout = stop_phase_timeout(&self.lifecycle);
            let retirement_timeout = phase_timeout.saturating_mul(2);
            self.stop.requested.store(true, Ordering::Release);
            self.stop.control_lost.store(true, Ordering::Release);
            let manager_error = self.lifetime_control.stop().err();
            let first_wait = phase_timeout
                .saturating_sub(tokio::time::Instant::now().saturating_duration_since(started));
            let mut outcome = self.wait(first_wait).await;
            let mut recovery_error = None;
            if outcome.is_err() {
                recovery_error = self.lifetime_control.force_manager().err();
                let remaining = retirement_timeout
                    .saturating_sub(tokio::time::Instant::now().saturating_duration_since(started));
                outcome = self.wait(remaining).await;
            }
            let reap_result = self.release_root().await;
            match (outcome, manager_error, recovery_error, reap_result) {
                (Ok(outcome), _, _, Ok(())) => Ok(outcome),
                (Ok(_), _, _, Err(error)) => Err(error),
                (Err(error), Some(manager_error), Some(recovery_error), _) => {
                    Err(error.context(format!("{manager_error}; additionally, {recovery_error}")))
                }
                (Err(error), Some(manager_error), None, _) => Err(error.context(manager_error)),
                (Err(error), None, Some(recovery_error), _) => Err(error.context(recovery_error)),
                (Err(error), None, None, _) => Err(error),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            if let Some(task) = self
                .force_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                task.abort();
            }
            {
                let process_group = self
                    .process_group_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(process_group_id) = *process_group {
                    self.stop.requested.store(true, Ordering::Release);
                    *self
                        .stop
                        .force_at
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(tokio::time::Instant::now());
                    *self
                        .stop
                        .force_timeout
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(Duration::from_millis(self.lifecycle.force_timeout_ms));
                    if let Err(error) = self.force(process_group_id)
                        && process_tree_exists(process_group_id)
                    {
                        return Err(error);
                    }
                    self.stop.wakeup.notify_one();
                }
            }
            self.wait(
                Duration::from_millis(self.lifecycle.force_timeout_ms) + Duration::from_secs(1),
            )
            .await
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn force(&self, process_group_id: u32) -> Result<()> {
        self.stop.forced.store(true, Ordering::Release);
        signal_kill(process_group_id).context("target forced termination failed")
    }

    #[cfg(target_os = "macos")]
    async fn release_root(&self) -> Result<()> {
        self.reap_release.send_replace(true);
        let mut result = self.reap_result.clone();
        if let Some(result) = result.borrow().clone() {
            return result.map_err(anyhow::Error::msg);
        }
        tokio::time::timeout(REAP_ALLOWANCE, async {
            loop {
                result
                    .changed()
                    .await
                    .context("sandbox target reaper ended without a result")?;
                if let Some(result) = result.borrow().clone() {
                    return result.map_err(anyhow::Error::msg);
                }
            }
        })
        .await
        .context("timed out reaping the sandbox target")?
    }
}

#[cfg(target_os = "macos")]
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.reap_release.send_replace(true);
    }
}

fn combine_errors(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; additionally, {second}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

#[cfg(not(target_os = "macos"))]
async fn wait_child(child: &mut Child) -> std::io::Result<TargetOutcome> {
    child.wait().await.map(target_outcome)
}

#[cfg(target_os = "macos")]
async fn observe_child_without_reaping(process_id: u32) -> std::io::Result<TargetOutcome> {
    tokio::task::spawn_blocking(move || {
        loop {
            let information = wait_for_child_notification(
                process_id,
                libc::WEXITED | libc::WSTOPPED | libc::WCONTINUED | libc::WNOWAIT,
            )?;
            let status = information.si_status;
            match information.si_code {
                libc::CLD_EXITED => {
                    return Ok(TargetOutcome {
                        kind: TargetOutcomeKind::Exited,
                        code: Some(i64::from(status)),
                        signal: None,
                        error: None,
                    });
                }
                libc::CLD_KILLED | libc::CLD_DUMPED => {
                    return Ok(TargetOutcome {
                        kind: TargetOutcomeKind::Signaled,
                        code: None,
                        signal: Some(status),
                        error: None,
                    });
                }
                libc::CLD_STOPPED => {
                    wait_for_child_notification(process_id, libc::WSTOPPED | libc::WNOHANG)?;
                }
                libc::CLD_CONTINUED => {
                    wait_for_child_notification(process_id, libc::WCONTINUED | libc::WNOHANG)?;
                }
                code => {
                    return Ok(TargetOutcome {
                        kind: TargetOutcomeKind::Unknown,
                        code: None,
                        signal: None,
                        error: Some(format!("target returned unrecognized wait code {code}")),
                    });
                }
            }
        }
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(target_os = "macos")]
fn wait_for_child_notification(
    process_id: u32,
    options: libc::c_int,
) -> std::io::Result<libc::siginfo_t> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result =
        unsafe { libc::waitid(libc::P_PID, process_id, information.as_mut_ptr(), options) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { information.assume_init() })
}

fn target_observation(
    child_status: std::io::Result<TargetOutcome>,
    exec_status: Result<Option<String>>,
) -> (Option<TargetOutcome>, Option<String>) {
    match exec_status {
        Ok(Some(error)) => (
            None,
            Some(format!(
                "target executable could not start inside the sandbox: {error}"
            )),
        ),
        Err(error) => (
            None,
            Some(format!(
                "target execution status observation failed: {error:#}"
            )),
        ),
        Ok(None) => match child_status {
            Ok(status) => (Some(status), None),
            Err(error) => (
                None,
                Some(format!("sandbox target status observation failed: {error}")),
            ),
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn target_outcome(status: std::process::ExitStatus) -> TargetOutcome {
    if let Some(code) = status.code() {
        TargetOutcome {
            kind: TargetOutcomeKind::Exited,
            code: Some(i64::from(code)),
            signal: None,
            error: None,
        }
    } else if let Some(signal) = status.signal() {
        TargetOutcome {
            kind: TargetOutcomeKind::Signaled,
            code: None,
            signal: Some(signal),
            error: None,
        }
    } else {
        TargetOutcome {
            kind: TargetOutcomeKind::Unknown,
            code: None,
            signal: None,
            error: Some("target returned an unrecognized wait status".to_string()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn retire_process_group(
    process_group_id: Option<u32>,
    lifecycle: &LifecyclePolicy,
    stop: &StopState,
) -> RetirementOutcome {
    let Some(process_group_id) = process_group_id else {
        return failed_retirement(
            /*forced*/ false,
            "sandbox process group was not recorded".to_string(),
        );
    };
    if !process_tree_exists(process_group_id) {
        return complete_retirement(stop.forced.load(Ordering::Acquire));
    }
    if !stop.requested.load(Ordering::Acquire) {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(lifecycle.root_exit_grace_ms)) => {}
            () = stop.wakeup.notified() => {}
        }
    }
    if !process_tree_exists(process_group_id) {
        return complete_retirement(stop.forced.load(Ordering::Acquire));
    }
    if stop.requested.load(Ordering::Acquire) {
        let force_at = *stop
            .force_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(force_at) = force_at {
            while tokio::time::Instant::now() < force_at && process_tree_exists(process_group_id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    } else {
        #[cfg(target_os = "macos")]
        {
            if let Err(error) = signal_terminate(process_group_id) {
                return failed_retirement(/*forced*/ false, error.to_string());
            }
            tokio::time::sleep(Duration::from_millis(lifecycle.terminate_grace_ms)).await;
        }
    }
    if process_tree_exists(process_group_id) {
        stop.forced.store(true, Ordering::Release);
        if let Err(error) = signal_kill(process_group_id)
            && process_tree_exists(process_group_id)
        {
            return failed_retirement(/*forced*/ true, error.to_string());
        }
    }
    let force_timeout = stop
        .force_timeout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unwrap_or_else(|| Duration::from_millis(lifecycle.force_timeout_ms));
    let deadline = tokio::time::Instant::now() + force_timeout;
    while process_tree_exists(process_group_id) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let complete = !process_tree_exists(process_group_id);
    RetirementOutcome {
        complete,
        forced: stop.forced.load(Ordering::Acquire),
        error: (!complete)
            .then(|| "target process group remained alive after the force deadline".to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
fn process_group_id_value(process_group_id: &Mutex<Option<u32>>) -> Option<u32> {
    *process_group_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(target_os = "macos")]
fn signal_terminate(process_group_id: u32) -> std::io::Result<()> {
    codex_utils_pty::process_group::terminate_process_group_with_member_fallback(process_group_id)
        .map(|_| ())
}

#[cfg(not(target_os = "macos"))]
fn signal_kill(process_group_id: u32) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        codex_utils_pty::process_group::kill_process_group_with_member_fallback(process_group_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        codex_utils_pty::process_group::kill_process_group(process_group_id)
    }
}

fn process_tree_exists(process_group_id: u32) -> bool {
    let result = unsafe { libc::kill(-(process_group_id as libc::pid_t), 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn complete_retirement(forced: bool) -> RetirementOutcome {
    RetirementOutcome {
        complete: true,
        forced,
        error: None,
    }
}

#[cfg(not(target_os = "macos"))]
fn failed_retirement(forced: bool, error: String) -> RetirementOutcome {
    RetirementOutcome {
        complete: false,
        forced,
        error: Some(error),
    }
}
