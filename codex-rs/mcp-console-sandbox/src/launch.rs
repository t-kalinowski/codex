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
    let mut tracker = platform::Tracker::new()?;
    let terminal = platform::Terminal::capture()?;
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
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
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
        unsafe {
            command.pre_exec(move || {
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
        let mut target = None;
        loop {
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
            for signal in signals.pending()? {
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
            if let Some(pending) = &mut gate
                && pending.advance(root.id() as i32)?
            {
                target = pending.target.take();
                gate = None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if let Some(child) = &mut child {
        if let Err(error) = child.try_wait() {
            errors.push(format!("reap sandbox root: {error}"));
        }
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
