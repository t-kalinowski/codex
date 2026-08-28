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
use anyhow::bail;
use codex_network_proxy::NetworkProxyHandle;
use codex_windows_sandbox::WindowsSandboxStandaloneOutcome;
use codex_windows_sandbox::WindowsSandboxStandaloneProcess;
use codex_windows_sandbox::WindowsSandboxStandaloneRootOutcome;
use std::time::Duration;
use tokio::sync::watch;

pub struct Supervisor {
    process: WindowsSandboxStandaloneProcess,
    force_timeout: Duration,
    final_outcome: watch::Receiver<Option<FinalOutcome>>,
}

impl Supervisor {
    pub fn start(
        process: WindowsSandboxStandaloneProcess,
        lifecycle: LifecyclePolicy,
        proxy_handle: Option<NetworkProxyHandle>,
    ) -> Self {
        let force_timeout = Duration::from_millis(lifecycle.force_timeout_ms);
        let process_observer = process.clone();
        let (final_sender, final_outcome) = watch::channel(None);
        tokio::spawn(async move {
            let waiting_process = process_observer.clone();
            let native = tokio::task::spawn_blocking(move || waiting_process.wait()).await;
            let mut outcome = match native {
                Ok(native) => final_outcome_from_native(native),
                Err(error) => observer_failure(error.to_string()),
            };
            if let Some(handle) = proxy_handle
                && let Err(error) = handle.shutdown().await
            {
                outcome.infrastructure.cleanup_error = Some(error.to_string());
            }
            process_observer.release_policy_lease();
            final_sender.send_replace(Some(outcome));
        });
        Self {
            process,
            force_timeout,
            final_outcome,
        }
    }

    pub fn status(&self) -> RunnerStatus {
        if let Some(outcome) = self.final_outcome.borrow().clone() {
            return RunnerStatus {
                phase: RunnerPhase::Retired,
                target: outcome.target,
                retirement: Some(outcome.retirement),
            };
        }
        if let Some(target) = self.process.try_root_outcome() {
            return RunnerStatus {
                phase: RunnerPhase::RootExited,
                target: Some(target_outcome(target)),
                retirement: None,
            };
        }
        RunnerStatus {
            phase: RunnerPhase::Running,
            target: None,
            retirement: None,
        }
    }

    pub fn interrupt(&self) -> Result<()> {
        bail!("native Windows interrupt projection is unsupported")
    }

    pub fn terminate(&self, deadlines: StopDeadlines) -> Result<()> {
        if self.final_outcome.borrow().is_some() {
            bail!("target generation is already retired")
        }
        if deadlines.graceful_ms != 0 {
            bail!("graceful termination is unsupported by the Windows elevated backend")
        }
        self.process
            .force_terminate(Duration::from_millis(deadlines.force_ms))
            .context("target force termination failed")
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
        if self.final_outcome.borrow().is_none() {
            self.process
                .force_terminate(self.force_timeout)
                .context("control-loss target retirement failed")?;
        }
        self.wait(self.force_timeout + Duration::from_secs(1)).await
    }
}

fn final_outcome_from_native(native: WindowsSandboxStandaloneOutcome) -> FinalOutcome {
    FinalOutcome {
        target: Some(target_outcome(native.target)),
        retirement: RetirementOutcome {
            complete: native.retirement.complete,
            forced: native.retirement.forced,
            error: native.retirement.error,
        },
        infrastructure: InfrastructureOutcome {
            error: native.infrastructure_error,
            cleanup_error: None,
        },
    }
}

fn observer_failure(error: String) -> FinalOutcome {
    FinalOutcome {
        target: None,
        retirement: RetirementOutcome {
            complete: false,
            forced: false,
            error: Some("Windows target retirement could not be observed".to_string()),
        },
        infrastructure: InfrastructureOutcome {
            error: Some(format!("Windows target observer failed: {error}")),
            cleanup_error: None,
        },
    }
}

fn target_outcome(native: WindowsSandboxStandaloneRootOutcome) -> TargetOutcome {
    match native {
        WindowsSandboxStandaloneRootOutcome::Exited { code } => TargetOutcome {
            kind: TargetOutcomeKind::Exited,
            code: Some(i64::from(code)),
            signal: None,
            error: None,
        },
        WindowsSandboxStandaloneRootOutcome::Unknown { error } => TargetOutcome {
            kind: TargetOutcomeKind::Unknown,
            code: None,
            signal: None,
            error: Some(error),
        },
    }
}
