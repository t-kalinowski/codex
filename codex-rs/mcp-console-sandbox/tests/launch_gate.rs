#![cfg(unix)]
#![allow(clippy::expect_used)]

use codex_mcp_console_sandbox::launch_bridge::LaunchBridgeMode;
use codex_mcp_console_sandbox::launch_bridge::prepare_target;
use codex_utils_cargo_bin::cargo_bin;
use std::ffi::OsString;
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::timeout;

#[tokio::test]
async fn closing_the_launch_gate_does_not_start_the_target() -> anyhow::Result<()> {
    let directory = TempDir::new().expect("target directory");
    let marker = directory.path().join("target-started");
    let target = [
        cargo_bin("mcp-console-sandbox-fixture")
            .expect("fixture binary")
            .into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_os_string(),
        OsString::from("started"),
    ];
    let prepared = prepare_target(
        &cargo_bin("mcp-console-sandbox").expect("runner binary"),
        &target,
        LaunchBridgeMode::Direct,
    )?;
    let (program, arguments) = prepared.command.split_first().expect("bridge command");
    let mut child = Command::new(program).args(arguments).spawn()?;
    drop(prepared.writer);
    drop(prepared.gate_reader);

    let gated_status = timeout(Duration::from_secs(2), prepared.status.wait_for_gate()).await??;
    assert!(!marker.exists());
    drop(prepared.gate);

    let (Err(error), _completion) = timeout(Duration::from_secs(2), gated_status.confirm()).await?
    else {
        panic!("closed launch gate must fail target startup");
    };
    assert!(
        error.to_string().contains("launch gate closed"),
        "{error:#}"
    );
    assert!(
        timeout(Duration::from_secs(2), child.wait())
            .await??
            .success()
    );
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn closing_the_commit_gate_after_target_preparation_does_not_execute_target()
-> anyhow::Result<()> {
    let directory = TempDir::new().expect("target directory");
    let marker = directory.path().join("target-started");
    let target = [
        cargo_bin("mcp-console-sandbox-fixture")
            .expect("fixture binary")
            .into_os_string(),
        OsString::from("write"),
        marker.as_os_str().to_os_string(),
        OsString::from("started"),
    ];
    let prepared = prepare_target(
        &cargo_bin("mcp-console-sandbox").expect("runner binary"),
        &target,
        LaunchBridgeMode::Direct,
    )?;
    let (program, arguments) = prepared.command.split_first().expect("bridge command");
    let mut child = Command::new(program).args(arguments).spawn()?;
    drop(prepared.writer);
    drop(prepared.commit_status_writer);
    drop(prepared.gate_reader);

    let gated_status = timeout(Duration::from_secs(2), prepared.status.wait_for_gate()).await??;
    let commit_gate = prepared.gate.release()?;
    let (target, _completion) = timeout(Duration::from_secs(2), gated_status.confirm()).await?;
    let _target = target?;
    assert!(!marker.exists());
    drop(commit_gate);

    assert!(
        timeout(Duration::from_secs(2), child.wait())
            .await??
            .code()
            .is_some()
    );
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn watchdog_survives_when_its_source_is_the_reserved_descriptor() -> anyhow::Result<()> {
    let status = timeout(
        Duration::from_secs(5),
        Command::new(cargo_bin("mcp-console-sandbox-fixture")?)
            .arg("watchdog-reserved-fd")
            .status(),
    )
    .await??;
    assert!(status.success(), "fixture failed with {status}");
    Ok(())
}

#[tokio::test]
async fn watchdog_disarm_does_not_retire_the_process_group() -> anyhow::Result<()> {
    let status = timeout(
        Duration::from_secs(5),
        Command::new(cargo_bin("mcp-console-sandbox-fixture")?)
            .arg("watchdog-disarm")
            .status(),
    )
    .await??;
    assert!(status.success(), "fixture failed with {status}");
    Ok(())
}
