#[cfg(target_os = "linux")]
#[path = "supervisor/linux.rs"]
mod linux;

#[cfg(unix)]
mod unix {
    use crate::launch_bridge::ReportedTargetOutcome;
    use crate::launch_bridge::TargetCompletion;
    #[cfg(target_os = "linux")]
    use crate::linux_process::LinuxProcess;
    use crate::protocol::FinalOutcome;
    use crate::protocol::InfrastructureOutcome;
    use crate::protocol::LifecyclePolicy;
    #[cfg(not(target_os = "linux"))]
    use crate::protocol::RetirementOutcome;
    use crate::protocol::RunnerPhase;
    use crate::protocol::RunnerStatus;
    use crate::protocol::StopDeadlines;
    use crate::protocol::TargetOutcome;
    use crate::protocol::TargetOutcomeKind;
    use anyhow::Context;
    use anyhow::Result;
    use anyhow::bail;
    use codex_network_proxy::NetworkProxyHandle;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::process::Child;
    use tokio::sync::Notify;
    use tokio::sync::watch;

    pub struct Supervisor {
        owned_process_group_id: Arc<Mutex<Option<u32>>>,
        force_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
        force_timeout: Duration,
        status: watch::Receiver<RunnerStatus>,
        final_outcome: watch::Receiver<Option<FinalOutcome>>,
        forced: Arc<AtomicBool>,
        termination_deadline: Arc<Mutex<Option<tokio::time::Instant>>>,
        retirement_wakeup: Arc<Notify>,
        #[cfg(target_os = "linux")]
        namespace_process: Arc<LinuxProcess>,
    }

    impl Supervisor {
        pub(crate) fn start(
            child: Child,
            process_group_id: u32,
            lifecycle: LifecyclePolicy,
            proxy_handle: Option<NetworkProxyHandle>,
            watchdog: Option<crate::watchdog::ProcessGroupWatchdog>,
            target_completion: TargetCompletion,
            #[cfg(target_os = "linux")] namespace_process: LinuxProcess,
        ) -> Self {
            let force_timeout = Duration::from_millis(lifecycle.force_timeout_ms);
            let forced = Arc::new(AtomicBool::new(false));
            let supervisor_forced = Arc::clone(&forced);
            let termination_deadline = Arc::new(Mutex::new(None));
            let supervisor_termination_deadline = Arc::clone(&termination_deadline);
            let retirement_wakeup = Arc::new(Notify::new());
            let supervisor_retirement_wakeup = Arc::clone(&retirement_wakeup);
            let owned_process_group_id = Arc::new(Mutex::new(Some(process_group_id)));
            let supervisor_process_group_id = Arc::clone(&owned_process_group_id);
            let force_task = Arc::new(Mutex::new(None));
            let supervisor_force_task = Arc::clone(&force_task);
            #[cfg(target_os = "linux")]
            let namespace_process = Arc::new(namespace_process);
            #[cfg(target_os = "linux")]
            let supervisor_namespace_process = Arc::clone(&namespace_process);
            let (status_sender, status) = watch::channel(RunnerStatus {
                phase: RunnerPhase::Running,
                target: None,
                retirement: None,
            });
            let (final_sender, final_outcome) = watch::channel(None);
            let watchdog_force_timeout = force_timeout;
            tokio::spawn(async move {
                let mut watchdog_exit = watchdog
                    .as_ref()
                    .map(crate::watchdog::ProcessGroupWatchdog::exit_receiver);
                let supervision = async {
                    #[cfg(target_os = "linux")]
                    {
                        super::linux::supervise(
                            child,
                            process_group_id,
                            &lifecycle,
                            &supervisor_forced,
                            &supervisor_termination_deadline,
                            &supervisor_retirement_wakeup,
                            &status_sender,
                            target_completion,
                            &supervisor_namespace_process,
                        )
                        .await
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let mut child = child;
                        let (bridge_status, reported_outcome) =
                            tokio::join!(child.wait(), target_completion.wait());
                        let (target, infrastructure_error) = target_observation(
                            bridge_status,
                            reported_outcome,
                            BridgeCompletion::TargetProjection,
                        );
                        status_sender.send_replace(RunnerStatus {
                            phase: RunnerPhase::RootExited,
                            target: target.clone(),
                            retirement: None,
                        });
                        let retirement = retire_after_root(
                            process_group_id,
                            &lifecycle,
                            &supervisor_forced,
                            &supervisor_termination_deadline,
                            &supervisor_retirement_wakeup,
                        )
                        .await;
                        (target, retirement, infrastructure_error)
                    }
                };
                tokio::pin!(supervision);
                let (target, retirement, infrastructure_error) = match watchdog_exit.as_mut() {
                    Some(watchdog_exit) => tokio::select! {
                        outcome = &mut supervision => outcome,
                        watchdog_result = watchdog_exit.wait() => {
                            {
                                let mut deadline = supervisor_termination_deadline
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                if deadline.is_none() {
                                    *deadline = Some(
                                        tokio::time::Instant::now() + watchdog_force_timeout
                                    );
                                }
                            }
                            supervisor_forced.store(true, Ordering::Release);
                            supervisor_retirement_wakeup.notify_one();
                            let retirement_result = {
                                #[cfg(target_os = "linux")]
                                {
                                    signal_kill_owned(
                                        &supervisor_namespace_process,
                                        process_group_id,
                                    )
                                }
                                #[cfg(not(target_os = "linux"))]
                                {
                                    signal_kill(process_group_id)
                                }
                            };
                            let mut outcome = supervision.await;
                            let watchdog_error = match watchdog_result {
                                Ok(()) => "process-group watchdog exited unexpectedly".to_string(),
                                Err(error) => format!(
                                    "process-group watchdog exited unexpectedly: {error:#}"
                                ),
                            };
                            let watchdog_error = match retirement_result {
                                Ok(()) => watchdog_error,
                                Err(error) => format!(
                                    "{watchdog_error}; fail-safe target retirement failed: {error}"
                                ),
                            };
                            outcome.2 = Some(match outcome.2 {
                                Some(error) => format!("{error}; {watchdog_error}"),
                                None => watchdog_error,
                            });
                            outcome
                        }
                    },
                    None => supervision.await,
                };
                retire_process_group(&supervisor_process_group_id, &supervisor_force_task);
                status_sender.send_replace(RunnerStatus {
                    phase: RunnerPhase::Retired,
                    target: target.clone(),
                    retirement: Some(retirement.clone()),
                });
                let proxy_cleanup_error = match proxy_handle {
                    Some(handle) => handle.shutdown().await.err().map(|error| error.to_string()),
                    None => None,
                };
                let watchdog_cleanup_error = match watchdog {
                    Some(watchdog) => watchdog.disarm().await.err().map(|error| error.to_string()),
                    None => None,
                };
                let cleanup_error = [proxy_cleanup_error, watchdog_cleanup_error]
                    .into_iter()
                    .flatten()
                    .reduce(|left, right| format!("{left}; {right}"));
                let outcome = FinalOutcome {
                    target,
                    retirement,
                    infrastructure: InfrastructureOutcome {
                        error: infrastructure_error,
                        cleanup_error,
                    },
                };
                final_sender.send_replace(Some(outcome));
            });
            Self {
                owned_process_group_id,
                force_task,
                force_timeout,
                status,
                final_outcome,
                forced,
                termination_deadline,
                retirement_wakeup,
                #[cfg(target_os = "linux")]
                namespace_process,
            }
        }

        pub fn status(&self) -> RunnerStatus {
            self.status.borrow().clone()
        }

        pub fn interrupt(&self) -> Result<()> {
            let process_group = self
                .owned_process_group_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(process_group_id) = *process_group else {
                bail!("target generation is already retired")
            };
            let result = signal_interrupt(process_group_id);
            drop(process_group);
            result.context("target interrupt failed")
        }

        pub fn terminate(&self, deadlines: StopDeadlines) -> Result<()> {
            #[cfg(target_os = "linux")]
            anyhow::ensure!(
                deadlines.graceful_ms == 0,
                "Linux termination does not support a graceful deadline"
            );
            let mut force_task = self
                .force_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(task) = force_task.take() {
                task.abort();
            }
            let process_group = self
                .owned_process_group_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(process_group_id) = *process_group else {
                bail!("target generation is already retired")
            };
            let force_deadline = tokio::time::Instant::now()
                + Duration::from_millis(deadlines.graceful_ms)
                + Duration::from_millis(deadlines.force_ms);
            *self
                .termination_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(force_deadline);

            #[cfg(target_os = "linux")]
            {
                self.forced.store(true, Ordering::Release);
                self.retirement_wakeup.notify_one();
                let result = signal_kill_owned(&self.namespace_process, process_group_id);
                drop(process_group);
                drop(force_task);
                result.context("target forced termination failed")
            }

            #[cfg(not(target_os = "linux"))]
            {
                self.retirement_wakeup.notify_one();
                let result = signal_terminate(process_group_id);
                drop(process_group);
                result.context("target graceful termination failed")?;
                let owned_process_group_id = Arc::clone(&self.owned_process_group_id);
                let forced = Arc::clone(&self.forced);
                *force_task = Some(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(deadlines.graceful_ms)).await;
                    let process_group = owned_process_group_id
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(process_group_id) = *process_group
                        && process_tree_exists(process_group_id)
                    {
                        forced.store(true, Ordering::Release);
                        let _ = signal_kill(process_group_id);
                    }
                    drop(process_group);
                }));
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
            let signal_result = {
                let mut force_task = self
                    .force_task
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(task) = force_task.take() {
                    task.abort();
                }
                let process_group = self
                    .owned_process_group_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(process_group_id) = *process_group {
                    let mut deadline = self
                        .termination_deadline
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if deadline.is_none() {
                        *deadline = Some(tokio::time::Instant::now() + self.force_timeout);
                    }
                    drop(deadline);
                    self.forced.store(true, Ordering::Release);
                    self.retirement_wakeup.notify_one();
                    #[cfg(target_os = "linux")]
                    {
                        signal_kill_owned(&self.namespace_process, process_group_id)
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        signal_kill(process_group_id)
                    }
                } else {
                    Ok(())
                }
            };
            signal_result.context("control-loss target retirement failed")?;
            self.wait(self.force_timeout + Duration::from_secs(1)).await
        }
    }

    fn retire_process_group(
        process_group_id: &Mutex<Option<u32>>,
        force_task: &Mutex<Option<tokio::task::JoinHandle<()>>>,
    ) {
        let mut force_task = force_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut process_group_id = process_group_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *process_group_id = None;
        if let Some(task) = force_task.take() {
            task.abort();
        }
    }

    #[cfg(not(target_os = "linux"))]
    async fn retire_after_root(
        process_group_id: u32,
        lifecycle: &LifecyclePolicy,
        forced: &AtomicBool,
        termination_deadline: &Mutex<Option<tokio::time::Instant>>,
        retirement_wakeup: &Notify,
    ) -> RetirementOutcome {
        if !process_tree_exists(process_group_id) {
            return complete_retirement(forced.load(Ordering::Acquire));
        }
        let explicit_deadline = *termination_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(deadline) = explicit_deadline {
            while tokio::time::Instant::now() < deadline {
                if !process_tree_exists(process_group_id) {
                    return complete_retirement(forced.load(Ordering::Acquire));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            return RetirementOutcome {
                complete: !process_tree_exists(process_group_id),
                forced: forced.load(Ordering::Acquire),
                error: process_tree_exists(process_group_id).then(|| {
                    "target process group remained alive after the requested force deadline"
                        .to_string()
                }),
            };
        }
        if forced.load(Ordering::Acquire) {
            return observe_forced_retirement(process_group_id, lifecycle.force_timeout_ms).await;
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(lifecycle.root_exit_grace_ms)) => {}
            () = retirement_wakeup.notified() => {}
        }
        let explicit_deadline = *termination_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(deadline) = explicit_deadline {
            while tokio::time::Instant::now() < deadline {
                if !process_tree_exists(process_group_id) {
                    return complete_retirement(forced.load(Ordering::Acquire));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            return RetirementOutcome {
                complete: !process_tree_exists(process_group_id),
                forced: forced.load(Ordering::Acquire),
                error: process_tree_exists(process_group_id).then(|| {
                    "target process group remained alive after the requested force deadline"
                        .to_string()
                }),
            };
        }
        if forced.load(Ordering::Acquire) {
            return observe_forced_retirement(process_group_id, lifecycle.force_timeout_ms).await;
        }
        if !process_tree_exists(process_group_id) {
            return complete_retirement(forced.load(Ordering::Acquire));
        }
        if let Err(error) = signal_terminate(process_group_id) {
            return failed_retirement(false, error);
        }
        tokio::time::sleep(Duration::from_millis(lifecycle.terminate_grace_ms)).await;
        if !process_tree_exists(process_group_id) {
            return complete_retirement(forced.load(Ordering::Acquire));
        }
        if let Err(error) = signal_kill(process_group_id) {
            return failed_retirement(true, error);
        }
        forced.store(true, Ordering::Release);
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(lifecycle.force_timeout_ms);
        while tokio::time::Instant::now() < deadline {
            if !process_tree_exists(process_group_id) {
                return complete_retirement(forced.load(Ordering::Acquire));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        RetirementOutcome {
            complete: !process_tree_exists(process_group_id),
            forced: true,
            error: process_tree_exists(process_group_id).then(|| {
                "target process group remained alive after the force deadline".to_string()
            }),
        }
    }

    #[cfg(not(target_os = "linux"))]
    async fn observe_forced_retirement(
        process_group_id: u32,
        force_timeout_ms: u64,
    ) -> RetirementOutcome {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(force_timeout_ms);
        while process_tree_exists(process_group_id) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let complete = !process_tree_exists(process_group_id);
        RetirementOutcome {
            complete,
            forced: true,
            error: (!complete).then(|| {
                "target process group remained alive after the force deadline".to_string()
            }),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn complete_retirement(forced: bool) -> RetirementOutcome {
        RetirementOutcome {
            complete: true,
            forced,
            error: None,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn failed_retirement(forced: bool, error: std::io::Error) -> RetirementOutcome {
        RetirementOutcome {
            complete: false,
            forced,
            error: Some(error.to_string()),
        }
    }

    #[derive(Clone, Copy)]
    pub(super) enum BridgeCompletion {
        #[cfg(not(target_os = "linux"))]
        TargetProjection,
        #[cfg(target_os = "linux")]
        NamespaceRetirement { forced: bool },
    }

    #[cfg(target_os = "linux")]
    pub(super) fn reported_target(
        reported_outcome: &Result<ReportedTargetOutcome>,
    ) -> Option<TargetOutcome> {
        match reported_outcome {
            Ok(ReportedTargetOutcome::Exited(code)) => Some(TargetOutcome {
                kind: TargetOutcomeKind::Exited,
                code: Some(i64::from(*code)),
                signal: None,
                error: None,
            }),
            Ok(ReportedTargetOutcome::Signaled(signal)) => Some(TargetOutcome {
                kind: TargetOutcomeKind::Signaled,
                code: None,
                signal: Some(*signal),
                error: None,
            }),
            Ok(ReportedTargetOutcome::InfrastructureError(_)) | Err(_) => None,
        }
    }

    pub(super) fn target_observation(
        bridge_status: std::io::Result<ExitStatus>,
        reported_outcome: Result<ReportedTargetOutcome>,
        bridge_completion: BridgeCompletion,
    ) -> (Option<TargetOutcome>, Option<String>) {
        match reported_outcome {
            Ok(ReportedTargetOutcome::Exited(code)) => {
                let infrastructure_error = match bridge_status {
                    Ok(status)
                        if status.code() == Some(code)
                            || accepts_forced_namespace_retirement(bridge_completion, status) =>
                    {
                        None
                    }
                    Ok(status) => Some(format!(
                        "launch bridge projected {status} after reporting target exit {code}"
                    )),
                    Err(error) => Some(format!(
                        "launch bridge status observation failed after target exit {code}: {error}"
                    )),
                };
                (
                    Some(TargetOutcome {
                        kind: TargetOutcomeKind::Exited,
                        code: Some(i64::from(code)),
                        signal: None,
                        error: None,
                    }),
                    infrastructure_error,
                )
            }
            Ok(ReportedTargetOutcome::Signaled(signal)) => {
                let infrastructure_error = match bridge_status {
                    Ok(status)
                        if status.signal() == Some(signal)
                            || accepts_forced_namespace_retirement(bridge_completion, status) =>
                    {
                        None
                    }
                    Ok(status) => Some(format!(
                        "launch bridge projected {status} after reporting target signal {signal}"
                    )),
                    Err(error) => Some(format!(
                        "launch bridge status observation failed after target signal {signal}: {error}"
                    )),
                };
                (
                    Some(TargetOutcome {
                        kind: TargetOutcomeKind::Signaled,
                        code: None,
                        signal: Some(signal),
                        error: None,
                    }),
                    infrastructure_error,
                )
            }
            Ok(ReportedTargetOutcome::InfrastructureError(error)) => (
                None,
                Some(match bridge_status {
                    Ok(_) => format!("launch bridge target wait failed: {error}"),
                    Err(status_error) => format!(
                        "launch bridge target wait failed: {error}; bridge status observation failed: {status_error}"
                    ),
                }),
            ),
            Err(error) => match bridge_status {
                Ok(status) => (
                    None,
                    Some(format!(
                        "launch bridge target outcome observation failed: {error:#}; bridge projected {status}"
                    )),
                ),
                Err(status_error) => (
                    None,
                    Some(format!(
                        "launch bridge target outcome observation failed: {error:#}; bridge status observation failed: {status_error}"
                    )),
                ),
            },
        }
    }

    fn accepts_forced_namespace_retirement(
        bridge_completion: BridgeCompletion,
        status: ExitStatus,
    ) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(
                bridge_completion,
                BridgeCompletion::NamespaceRetirement { forced: true }
            ) && status.signal() == Some(libc::SIGKILL)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (bridge_completion, status);
            false
        }
    }

    fn signal_interrupt(process_group_id: u32) -> std::io::Result<()> {
        codex_utils_pty::process_group::interrupt_process_group(process_group_id)
    }

    #[cfg(not(target_os = "linux"))]
    fn signal_terminate(process_group_id: u32) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            codex_utils_pty::process_group::terminate_process_group_with_member_fallback(
                process_group_id,
            )?;
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            codex_utils_pty::process_group::terminate_process_group(process_group_id)?;
            Ok(())
        }
    }

    pub(super) fn signal_kill(process_group_id: u32) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let result = codex_utils_pty::process_group::kill_process_group_with_member_fallback(
                process_group_id,
            );
            match result {
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                result => result,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let result = codex_utils_pty::process_group::kill_process_group(process_group_id);
            match result {
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                result => result,
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn signal_kill_owned(
        namespace_process: &LinuxProcess,
        process_group_id: u32,
    ) -> std::io::Result<()> {
        let namespace_result = namespace_process.kill();
        let process_group_result = signal_kill(process_group_id);
        match (namespace_result, process_group_result) {
            (Err(namespace_error), Err(process_group_error)) => {
                Err(std::io::Error::other(format!(
                    "sandbox namespace retirement failed: {namespace_error}; outer process-group retirement failed: {process_group_error}"
                )))
            }
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn process_tree_exists(process_group_id: u32) -> bool {
        let result = unsafe { libc::kill(-(process_group_id as libc::pid_t), 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(unix)]
pub use unix::Supervisor;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Supervisor;
