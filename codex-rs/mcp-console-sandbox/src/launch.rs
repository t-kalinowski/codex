//! The sole application supervisor: establish ownership, release native setup,
//! observe completion, retire descendants, then remove private storage.
use crate::bootstrap::Bootstrap;
use crate::config::Sigterm;
use crate::native::Gate;
use crate::platform;
use crate::signals::Signals;
use crate::storage::Storage;
use anyhow::Context;
use anyhow::Result;
use std::fs::File;
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

pub async fn run(request: Bootstrap, stdin: File, signals: Signals) -> Result<i32> {
    let parent = request
        .lifecycle
        .parent_pid
        .map(platform::Parent::capture)
        .transpose()?;
    let sigterm = request.lifecycle.sigterm;
    let timeout = Duration::from_millis(request.lifecycle.cleanup_timeout_ms.unwrap_or(1000));
    // Capture foreground ownership in the caller's group, before isolating an
    // owned stdio runner. Terminal stderr alone does not make it interactive.
    let terminal = platform::Terminal::capture()?;
    if parent.is_some()
        && !stdin.is_terminal()
        && !std::io::stdout().is_terminal()
        && unsafe { libc::getpgrp() != libc::getpid() }
        && unsafe { libc::setpgid(0, 0) } < 0
    {
        return Err(std::io::Error::last_os_error()).context("isolate owned runner process group");
    }
    let mut tracker = platform::Tracker::new()?;
    let storage = request
        .lifecycle
        .private_tmp
        .as_ref()
        .map(Storage::create)
        .transpose()?;
    let mut prepared = None;
    let mut child = None;
    // Keep the setup channel alive through retirement. Closing a partial frame
    // first wakes the native reader with a spurious startup error.
    let mut gate = None;
    let mut target = None;
    let mut observation_failed = false;
    let startup_cancellation = || -> Result<Option<i32>> {
        if !parent
            .as_ref()
            .map(platform::Parent::alive)
            .transpose()?
            .unwrap_or(true)
        {
            return Ok(Some(0));
        }
        for signal in signals.pending()? {
            if signal == libc::SIGTERM && sigterm == Sigterm::Retire {
                return Ok(Some(0));
            }
            if signals.forwards(signal) {
                return Ok(Some(128 + signal));
            }
        }
        Ok(None)
    };
    let result: Result<i32> = async {
        let (channel, inherited) = UnixStream::pair()?;
        platform::configure_channel(&channel)?;
        let descriptor = inherited.as_raw_fd();
        let preparation = crate::codex::prepare(
            request,
            signals.original.clone(),
            storage.as_ref(),
            descriptor,
        );
        tokio::pin!(preparation);
        let native = loop {
            if let Some(status) = startup_cancellation()? {
                return Ok(status);
            }
            tokio::select! {
                native = &mut preparation => break native?,
                event = signals.changed() => { event?; }
            }
        };
        prepared = Some(native);
        let native = prepared.as_mut().context("prepared native launch")?;
        gate = Some(Gate::new(channel, &native.setup)?);
        if let Some(status) = startup_cancellation()? {
            return Ok(status);
        }
        let mut command = native.command.take().context("native command")?;
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // Spawn on the current-thread runtime's main thread, which lives until
        // retirement ends: Linux PDEATHSIG follows the creating thread. Capture
        // the expected parent before fork, never from an already-orphaned child.
        #[cfg(target_os = "linux")]
        let supervisor = unsafe { libc::getpid() };
        // SAFETY: the child hook uses only syscalls and allocation-free errors;
        // proxy preparation may already have started other threads.
        unsafe {
            command.pre_exec(move || {
                #[cfg(target_os = "linux")]
                {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != supervisor {
                        return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                    }
                    // Native helpers use TERM for their own parent-death links.
                    // They must not inherit the supervisor's sigwait mask.
                    let mut mask = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    for signal in crate::signals::FORWARDED.into_iter().chain([libc::SIGCHLD]) {
                        libc::sigaddset(&mut mask, signal);
                    }
                    let error =
                        libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
                    if error != 0 {
                        return Err(std::io::Error::from_raw_os_error(error));
                    }
                }
                if libc::setpgid(0, 0) < 0 || libc::fcntl(descriptor, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        child = Some(command.spawn().context("launch native sandbox")?);
        // Command owns a copy of stdin; neither it nor the parent gate endpoint
        // may remain in a waiting host process after spawning.
        drop(command);
        drop(inherited);
        let root = child.as_mut().context("native child")?;
        tracker
            .track_root(root.id() as i32)
            .inspect_err(|_| observation_failed = true)
            .context("establish descendant observation")?;
        terminal.transfer(root.id() as i32)?;
        loop {
            let pending_signals = signals.pending()?;
            tracker
                .observe()
                .inspect_err(|_| observation_failed = true)?;
            // A completed root wins over pending signals and parent death.
            if let Some(status) = platform::root_status(root.id() as i32)? {
                return Ok(status);
            }
            if !parent
                .as_ref()
                .map(platform::Parent::alive)
                .transpose()?
                .unwrap_or(true)
            {
                return Ok(0);
            }
            for signal in pending_signals {
                if signal == libc::SIGTERM && sigterm == Sigterm::Retire {
                    return Ok(0);
                }
                if signals.forwards(signal) {
                    if let Some(target) = &target {
                        platform::forward(target, signal)?;
                    } else {
                        return Ok(128 + signal);
                    }
                }
            }
            if let Some(pending) = &mut gate {
                match pending.advance(root.id() as i32) {
                    Ok(false) => {}
                    Ok(true) => {
                        target = pending.target.take();
                        gate = None;
                    }
                    Err(error)
                        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            matches!(
                                error.kind(),
                                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
                            )
                        }) =>
                    {
                        // A native failure can close setup before its exit is
                        // waitable. Preserve that exit status and wait on SIGCHLD.
                        target = pending.target.take();
                        gate = None;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            wait_activity(&signals, gate.as_ref()).await?;
        }
    }
    .await;

    let mut errors = Vec::new();
    let status = match result {
        Ok(status) => status,
        Err(error) => {
            errors.push(format!("{error:#}"));
            1
        }
    };
    let deadline = Instant::now() + timeout;
    let mut retired = false;
    let mut retirement_failed = observation_failed;
    loop {
        if let Err(error) = signals.pending() {
            errors.push(format!("observe retirement signals: {error}"));
            retirement_failed = true;
            break;
        }
        #[cfg(target_os = "linux")]
        if let Some(root) = &child
            && platform::root_status(root.id() as i32).is_ok_and(|status| status.is_none())
        {
            let retirement = if let Some(target) = &target {
                platform::forward(target, libc::SIGKILL).map_err(anyhow::Error::from)
            } else if let Some(pending) = &mut gate {
                pending.retire(root.id() as i32).map(|closed| {
                    if closed {
                        gate = None;
                    }
                })
            } else {
                Ok(())
            };
            if let Err(error) = retirement {
                if !retirement_failed {
                    errors.push(format!("native retirement failed: {error}"));
                }
                retirement_failed = true;
                gate = None;
            }
        }
        match tracker.retire_pass() {
            Ok(true) => {
                retired = true;
                break;
            }
            Ok(false) => {}
            Err(error) => {
                if !retirement_failed {
                    errors.push(format!("descendant retirement failed: {error}"));
                }
                retirement_failed = true;
                // A failed initial watch may leave only the pinned Child handle.
                if let Some(child) = &mut child
                    && let Err(error) = child.kill()
                {
                    let message =
                        format!("terminate sandbox root after discovery failure: {error}");
                    if !errors.contains(&message) {
                        errors.push(message);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            errors.push("timed out retiring sandbox descendants".to_owned());
            break;
        }
        // A completed native monitor, readiness, or the deadline wakes retirement.
        // Do not ask for writable setup events once cancellation has begun.
        #[cfg(target_os = "linux")]
        let pending_gate = gate.as_ref().filter(|gate| gate.target.is_none());
        #[cfg(target_os = "macos")]
        let pending_gate = gate.as_ref();
        if let Ok(Err(error)) =
            tokio::time::timeout_at(deadline.into(), wait_activity(&signals, pending_gate)).await
        {
            errors.push(format!("wait for native retirement: {error}"));
            retirement_failed = true;
            break;
        }
    }
    if let Some(child) = &mut child
        && let Err(error) = child.try_wait()
    {
        errors.push(format!("reap sandbox root: {error}"));
    }
    drop(gate);
    if let Err(error) = terminal.restore() {
        errors.push(format!("restore foreground terminal: {error}"));
    }
    if let Some(native) = prepared
        && let Err(error) = native.shutdown().await
    {
        errors.push(format!("shutdown native proxy: {error:#}"));
    }
    if let Some(storage) = storage {
        if retired && !retirement_failed {
            if let Err(error) = storage.remove() {
                errors.push(format!("{error:#}"));
            }
        } else {
            errors.push(format!(
                "private storage retained after incomplete retirement: {}",
                storage.root.display()
            ));
        }
    }
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(status)
}

async fn wait_activity(signals: &Signals, gate: Option<&Gate>) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(gate) = gate {
        return tokio::select! {
            result = signals.changed() => result,
            result = gate.changed() => result,
        };
    }
    #[cfg(target_os = "macos")]
    let _ = gate;
    signals.changed().await
}
