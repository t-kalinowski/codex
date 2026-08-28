use super::unix::BridgeCompletion;
use super::unix::reported_target;
use super::unix::signal_kill_owned;
use super::unix::target_observation;
use crate::launch_bridge::TargetCompletion;
use crate::linux_process::LinuxProcess;
use crate::protocol::LifecyclePolicy;
use crate::protocol::RetirementOutcome;
use crate::protocol::RunnerPhase;
use crate::protocol::RunnerStatus;
use crate::protocol::TargetOutcome;
use std::process::ExitStatus;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::process::Child;
use tokio::sync::Notify;
use tokio::sync::watch;

#[allow(clippy::too_many_arguments)]
pub(super) async fn supervise(
    mut child: Child,
    process_group_id: u32,
    lifecycle: &LifecyclePolicy,
    forced: &AtomicBool,
    termination_deadline: &Mutex<Option<tokio::time::Instant>>,
    retirement_wakeup: &Notify,
    status_sender: &watch::Sender<RunnerStatus>,
    target_completion: TargetCompletion,
    namespace_process: &LinuxProcess,
) -> (Option<TargetOutcome>, RetirementOutcome, Option<String>) {
    let target_completion = target_completion.wait();
    tokio::pin!(target_completion);
    let (reported_outcome, completion_timed_out) = loop {
        if forced.load(Ordering::Acquire) {
            let deadline = force_deadline(termination_deadline, lifecycle.force_timeout_ms);
            break match tokio::time::timeout_at(deadline, &mut target_completion).await {
                Ok(outcome) => (outcome, false),
                Err(_) => (
                    Err(anyhow::anyhow!(
                        "target outcome observation remained incomplete after the force deadline"
                    )),
                    true,
                ),
            };
        }
        tokio::select! {
            outcome = &mut target_completion => break (outcome, false),
            () = retirement_wakeup.notified() => {}
        }
    };
    if !completion_timed_out {
        status_sender.send_replace(RunnerStatus {
            phase: RunnerPhase::RootExited,
            target: reported_target(&reported_outcome),
            retirement: None,
        });
    }
    let (retirement, bridge_status) = retire_after_root(
        &mut child,
        process_group_id,
        lifecycle,
        forced,
        termination_deadline,
        retirement_wakeup,
        namespace_process,
    )
    .await;
    let (target, infrastructure_error) = target_observation(
        bridge_status,
        reported_outcome,
        BridgeCompletion::NamespaceRetirement {
            forced: retirement.forced,
        },
    );
    (target, retirement, infrastructure_error)
}

async fn retire_after_root(
    child: &mut Child,
    process_group_id: u32,
    lifecycle: &LifecyclePolicy,
    forced: &AtomicBool,
    termination_deadline: &Mutex<Option<tokio::time::Instant>>,
    retirement_wakeup: &Notify,
    namespace_process: &LinuxProcess,
) -> (RetirementOutcome, std::io::Result<ExitStatus>) {
    if forced.load(Ordering::Acquire) {
        return observe_forced_retirement(
            child,
            namespace_process,
            force_deadline(termination_deadline, lifecycle.force_timeout_ms),
        )
        .await;
    }

    let grace_deadline =
        tokio::time::Instant::now() + Duration::from_millis(lifecycle.root_exit_grace_ms);
    loop {
        tokio::select! {
            status = child.wait() => {
                return complete_after_bridge_exit(
                    status,
                    namespace_process,
                    tokio::time::Instant::now()
                        + Duration::from_millis(lifecycle.force_timeout_ms),
                    forced.load(Ordering::Acquire),
                ).await;
            }
            () = tokio::time::sleep_until(grace_deadline) => break,
            () = retirement_wakeup.notified() => {
                if forced.load(Ordering::Acquire) {
                    return observe_forced_retirement(
                        child,
                        namespace_process,
                        force_deadline(termination_deadline, lifecycle.force_timeout_ms),
                    ).await;
                }
            }
        }
    }

    if forced.load(Ordering::Acquire) {
        return observe_forced_retirement(
            child,
            namespace_process,
            force_deadline(termination_deadline, lifecycle.force_timeout_ms),
        )
        .await;
    }

    forced.store(true, Ordering::Release);
    let signal_error = signal_kill_owned(namespace_process, process_group_id).err();
    let (mut retirement, bridge_status) = observe_forced_retirement(
        child,
        namespace_process,
        tokio::time::Instant::now() + Duration::from_millis(lifecycle.force_timeout_ms),
    )
    .await;
    if let Some(error) = signal_error {
        retirement.error = Some(match retirement.error {
            Some(retirement_error) => {
                format!("target retirement signal failed: {error}; {retirement_error}")
            }
            None => format!("target retirement signal failed: {error}"),
        });
    }
    (retirement, bridge_status)
}

fn force_deadline(
    termination_deadline: &Mutex<Option<tokio::time::Instant>>,
    force_timeout_ms: u64,
) -> tokio::time::Instant {
    termination_deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_millis(force_timeout_ms))
}

async fn observe_forced_retirement(
    child: &mut Child,
    namespace_process: &LinuxProcess,
    deadline: tokio::time::Instant,
) -> (RetirementOutcome, std::io::Result<ExitStatus>) {
    let (bridge_status, namespace_status) = tokio::join!(
        tokio::time::timeout_at(deadline, child.wait()),
        namespace_process.wait_until(deadline),
    );
    let bridge_status = match bridge_status {
        Ok(status) => status,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "outer sandbox helper remained alive after the force deadline",
        )),
    };
    retirement_from_observations(bridge_status, namespace_status, /*forced*/ true)
}

async fn complete_after_bridge_exit(
    bridge_status: std::io::Result<ExitStatus>,
    namespace_process: &LinuxProcess,
    deadline: tokio::time::Instant,
    forced: bool,
) -> (RetirementOutcome, std::io::Result<ExitStatus>) {
    let namespace_status = namespace_process.wait_until(deadline).await;
    retirement_from_observations(bridge_status, namespace_status, forced)
}

fn retirement_from_observations(
    bridge_status: std::io::Result<ExitStatus>,
    namespace_status: std::io::Result<bool>,
    forced: bool,
) -> (RetirementOutcome, std::io::Result<ExitStatus>) {
    let mut errors = Vec::new();
    if let Err(error) = bridge_status.as_ref() {
        errors.push(format!("outer sandbox helper retirement failed: {error}"));
    }
    match namespace_status {
        Ok(true) => {}
        Ok(false) => errors.push("sandbox namespace remained alive after the deadline".to_string()),
        Err(error) => errors.push(format!(
            "sandbox namespace retirement observation failed: {error}"
        )),
    }
    let retirement = RetirementOutcome {
        complete: errors.is_empty(),
        forced,
        error: errors
            .into_iter()
            .reduce(|left, right| format!("{left}; {right}")),
    };
    (retirement, bridge_status)
}
