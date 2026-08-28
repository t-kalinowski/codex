use super::unix::BridgeCompletion;
use super::unix::reported_target;
use super::unix::signal_kill;
use super::unix::target_observation;
use crate::launch_bridge::TargetCompletion;
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
) -> (Option<TargetOutcome>, RetirementOutcome, Option<String>) {
    let reported_outcome = target_completion.wait().await;
    status_sender.send_replace(RunnerStatus {
        phase: RunnerPhase::RootExited,
        target: reported_target(&reported_outcome),
        retirement: None,
    });
    let (retirement, bridge_status) = retire_after_root(
        &mut child,
        process_group_id,
        lifecycle,
        forced,
        termination_deadline,
        retirement_wakeup,
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
) -> (RetirementOutcome, std::io::Result<ExitStatus>) {
    if forced.load(Ordering::Acquire) {
        return observe_forced_retirement(
            child,
            force_deadline(termination_deadline, lifecycle.force_timeout_ms),
        )
        .await;
    }

    let grace_deadline =
        tokio::time::Instant::now() + Duration::from_millis(lifecycle.root_exit_grace_ms);
    loop {
        tokio::select! {
            status = child.wait() => {
                return completed_retirement(status, forced.load(Ordering::Acquire));
            }
            () = tokio::time::sleep_until(grace_deadline) => break,
            () = retirement_wakeup.notified() => {
                if forced.load(Ordering::Acquire) {
                    return observe_forced_retirement(
                        child,
                        force_deadline(termination_deadline, lifecycle.force_timeout_ms),
                    ).await;
                }
            }
        }
    }

    if forced.load(Ordering::Acquire) {
        return observe_forced_retirement(
            child,
            force_deadline(termination_deadline, lifecycle.force_timeout_ms),
        )
        .await;
    }

    forced.store(true, Ordering::Release);
    if let Err(error) = signal_kill(process_group_id) {
        return (
            RetirementOutcome {
                complete: false,
                forced: true,
                error: Some(error.to_string()),
            },
            Err(error),
        );
    }
    observe_forced_retirement(
        child,
        tokio::time::Instant::now() + Duration::from_millis(lifecycle.force_timeout_ms),
    )
    .await
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
    deadline: tokio::time::Instant,
) -> (RetirementOutcome, std::io::Result<ExitStatus>) {
    match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(status) => completed_retirement(status, /*forced*/ true),
        Err(_) => {
            let error = std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "sandbox namespace remained alive after the force deadline",
            );
            (
                RetirementOutcome {
                    complete: false,
                    forced: true,
                    error: Some(error.to_string()),
                },
                Err(error),
            )
        }
    }
}

fn completed_retirement(
    status: std::io::Result<ExitStatus>,
    forced: bool,
) -> (RetirementOutcome, std::io::Result<ExitStatus>) {
    let retirement = RetirementOutcome {
        complete: status.is_ok(),
        forced,
        error: status.as_ref().err().map(ToString::to_string),
    };
    (retirement, status)
}
