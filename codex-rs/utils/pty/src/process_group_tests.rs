use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::time::timeout;

use super::ProcessExitStatus;
use super::kill_process_group_members_except;
use super::kill_process_group_until_quiescent;
use super::kill_process_group_until_quiescent_with;
use super::process_is_live_group_member;
use super::signal_process_group_with_member_fallback;
use super::signal_process_id;
use super::terminate_process_group_with_member_fallback;
use super::try_process_exit_without_reaping;
use super::wait_for_process_exit_without_reaping;

#[tokio::test]
async fn observes_exit_status_without_reaping_the_direct_child() -> Result<()> {
    for (script, expected) in [
        ("exit 23", ProcessExitStatus::Exited(23)),
        ("kill -KILL $$", ProcessExitStatus::Signaled(libc::SIGKILL)),
    ] {
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;
        let process_id = child.id().context("child process has no process ID")?;

        assert_eq!(wait_for_process_exit_without_reaping(process_id)?, expected);
        assert_eq!(
            try_process_exit_without_reaping(process_id)?,
            Some(expected)
        );

        let status = child.wait().await?;
        match expected {
            ProcessExitStatus::Exited(code) => assert_eq!(status.code(), Some(code)),
            ProcessExitStatus::Signaled(signal) => assert_eq!(status.signal(), Some(signal)),
        }
    }

    Ok(())
}

#[tokio::test]
async fn non_reaping_status_probe_reports_a_live_child_as_pending() -> Result<()> {
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()?;
    let process_id = child.id().context("child process has no process ID")?;

    assert_eq!(try_process_exit_without_reaping(process_id)?, None);

    child.kill().await?;
    Ok(())
}

#[tokio::test]
async fn quiescing_descendants_preserves_an_exited_leaders_status() -> Result<()> {
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "/bin/sleep 30 & descendant=$!; printf '%s\\n' \"$descendant\"; exit 0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()?;
    let process_group_id = child.id().context("child process has no process ID")?;
    let stdout = child.stdout.take().context("child has no stdout")?;
    let descendant_id = timeout(
        Duration::from_secs(5),
        BufReader::new(stdout).lines().next_line(),
    )
    .await??
    .context("missing descendant ID")?
    .parse::<libc::pid_t>()?;
    assert_eq!(
        wait_for_process_exit_without_reaping(process_group_id)?,
        ProcessExitStatus::Exited(0)
    );
    assert!(process_is_live_group_member(
        descendant_id,
        process_group_id as libc::pid_t,
    )?);

    kill_process_group_until_quiescent(process_group_id)?;
    kill_process_group_until_quiescent(process_group_id)?;

    assert!(!process_is_live_group_member(
        descendant_id,
        process_group_id as libc::pid_t,
    )?);
    assert!(child.wait().await?.success());
    Ok(())
}

#[tokio::test]
async fn excluded_group_leader_survives_exact_member_cleanup() -> Result<()> {
    let mut wrapper = Command::new("/bin/sh")
        .args([
            "-c",
            "/bin/sleep 30 & first=$!; /bin/sleep 30 & second=$!; printf '%s %s\\n' \"$first\" \"$second\"; kill -STOP $$; wait",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()?;
    let process_group_id = wrapper.id().context("wrapper process has no process ID")?;
    let stdout = wrapper.stdout.take().context("wrapper has no stdout")?;
    let line = timeout(
        Duration::from_secs(5),
        BufReader::new(stdout).lines().next_line(),
    )
    .await??
    .context("missing descendant IDs")?;
    let (first, second) = line.split_once(' ').context("invalid descendant IDs")?;
    let descendants = [
        first.parse::<libc::pid_t>()?,
        second.parse::<libc::pid_t>()?,
    ];

    let error = kill_process_group_members_except(process_group_id, std::process::id())
        .expect_err("nonmember exclusion should be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    kill_process_group_members_except(process_group_id, process_group_id)?;
    kill_process_group_members_except(process_group_id, process_group_id)?;

    assert!(process_is_live_group_member(
        process_group_id as libc::pid_t,
        process_group_id as libc::pid_t,
    )?);
    for process_id in descendants {
        assert!(!process_is_live_group_member(
            process_id,
            process_group_id as libc::pid_t,
        )?);
    }

    assert!(signal_process_id(
        process_group_id as libc::pid_t,
        libc::SIGKILL,
    )?);
    wrapper.wait().await?;
    Ok(())
}

#[tokio::test]
async fn denied_group_signal_rescans_after_signalling_the_leader_first() -> Result<()> {
    let mut wrapper = Command::new("/bin/sh")
        .args([
            "-c",
            "/bin/sleep 30 & descendant=$!; printf '%s\\n' \"$descendant\"; wait",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()?;
    let process_group_id = wrapper.id().context("wrapper process has no process ID")?;
    let stdout = wrapper.stdout.take().context("wrapper has no stdout")?;
    let descendant_id = timeout(
        Duration::from_secs(5),
        BufReader::new(stdout).lines().next_line(),
    )
    .await??
    .context("missing descendant ID")?
    .parse::<libc::pid_t>()?;
    let mut late_child = None;
    let mut signalled = Vec::new();

    kill_process_group_until_quiescent_with(
        process_group_id,
        Duration::from_secs(2),
        |_, _| Err(io::Error::from_raw_os_error(libc::EPERM)),
        |process_id, signal| {
            signalled.push(process_id);
            if process_id == process_group_id as libc::pid_t && late_child.is_none() {
                late_child = Some(
                    Command::new("/bin/sleep")
                        .arg("30")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .kill_on_drop(true)
                        .process_group(process_group_id as libc::pid_t)
                        .spawn()?,
                );
            }
            signal_process_id(process_id, signal)
        },
    )?;

    let mut late_child = late_child.context("leader was not signalled")?;
    let late_child_id = late_child.id().context("late child has no process ID")?;
    assert_eq!(signalled.first(), Some(&(process_group_id as libc::pid_t)));
    assert!(signalled.contains(&(late_child_id as libc::pid_t)));
    assert!(!process_is_live_group_member(
        descendant_id,
        process_group_id as libc::pid_t,
    )?);
    assert!(!process_is_live_group_member(
        late_child_id as libc::pid_t,
        process_group_id as libc::pid_t,
    )?);

    wrapper.wait().await?;
    late_child.wait().await?;
    Ok(())
}

#[tokio::test]
async fn denied_group_signal_terminates_owned_descendants_and_preserves_escalation() -> Result<()> {
    for leader_exited in [false, true] {
        let mut wrapper = Command::new("/bin/sh")
            .args([
                "-c",
                "trap '' TERM; /bin/sleep 30 & resistant=$!; trap - TERM; /bin/sleep 30 & sibling=$!; printf '%s %s\\n' \"$resistant\" \"$sibling\"; wait",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;
        let process_group_id =
            wrapper.id().context("wrapper process has no process ID")? as libc::pid_t;
        let stdout = wrapper.stdout.take().context("wrapper has no stdout")?;
        let line = timeout(
            Duration::from_secs(5),
            BufReader::new(stdout).lines().next_line(),
        )
        .await??
        .context("missing descendant IDs")?;
        let (resistant_pid, sibling_pid) =
            line.split_once(' ').context("invalid descendant IDs")?;
        let resistant_pid = resistant_pid.parse::<libc::pid_t>()?;
        let sibling_pid = sibling_pid.parse::<libc::pid_t>()?;

        if leader_exited {
            wrapper.kill().await?;
        }

        let mut denied_leader = false;
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            assert!(signal_process_group_with_member_fallback(
                process_group_id as u32,
                signal,
                |_, _| Err(io::Error::from_raw_os_error(libc::EPERM)),
                |process_id, signal| {
                    if process_id == process_group_id {
                        denied_leader = true;
                        Err(io::Error::from_raw_os_error(libc::EPERM))
                    } else {
                        signal_process_id(process_id, signal)
                    }
                },
            )?);
            if signal == libc::SIGTERM {
                assert_eq!(denied_leader, !leader_exited);
                assert!(signal_process_id(resistant_pid, /*signal*/ 0)?);
            }
        }

        for process_id in [resistant_pid, sibling_pid] {
            timeout(Duration::from_secs(5), async move {
                while signal_process_id(process_id, /*signal*/ 0)? {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok::<(), io::Error>(())
            })
            .await??;
        }

        if !leader_exited {
            timeout(Duration::from_secs(5), wrapper.wait()).await??;
        }
    }

    Ok(())
}

#[test]
fn denied_group_signal_rejects_unsafe_process_group_ids() {
    for process_group_id in [0, u32::MAX] {
        let error = terminate_process_group_with_member_fallback(process_group_id)
            .expect_err("unsafe process group ID should be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
