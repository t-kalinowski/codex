use super::await_launch_commit;
use super::helper_input_loop;
use crate::standalone::wire::ParentMessage;
use crate::standalone::wire::write_wire_frame;
use anyhow::Result;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io;
use std::io::Seek;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

fn injected_termination_failure() -> io::Result<()> {
    Err(io::Error::other("injected job termination failure"))
}

fn control_file(message: Option<ParentMessage>) -> Result<File> {
    let mut file = tempfile::tempfile()?;
    if let Some(message) = message {
        write_wire_frame(&mut file, message)?;
        file.rewind()?;
    }
    Ok(file)
}

#[test]
fn explicit_force_termination_failure_returns_to_the_helper_owner() -> Result<()> {
    let forced = Arc::new(AtomicBool::new(false));
    let force_stop_timeout_ms = Arc::new(AtomicU64::new(10));
    let error = helper_input_loop(
        control_file(Some(ParentMessage::ForceTerminate {
            force_stop_timeout_ms: 25,
        }))?,
        Arc::clone(&forced),
        Arc::clone(&force_stop_timeout_ms),
        injected_termination_failure,
    )
    .expect_err("injected explicit termination failure must propagate");

    assert_eq!(
        (
            format!("{error:#}"),
            forced.load(Ordering::SeqCst),
            force_stop_timeout_ms.load(Ordering::SeqCst),
        ),
        (
            "force-terminate standalone process job: injected job termination failure".to_string(),
            true,
            25,
        )
    );
    Ok(())
}

#[test]
fn control_eof_termination_failure_returns_to_the_helper_owner() -> Result<()> {
    let forced = Arc::new(AtomicBool::new(false));
    let force_stop_timeout_ms = Arc::new(AtomicU64::new(10));
    let error = helper_input_loop(
        control_file(None)?,
        Arc::clone(&forced),
        Arc::clone(&force_stop_timeout_ms),
        injected_termination_failure,
    )
    .expect_err("injected control-EOF termination failure must propagate");

    assert_eq!(
        (
            format!("{error:#}"),
            forced.load(Ordering::SeqCst),
            force_stop_timeout_ms.load(Ordering::SeqCst),
        ),
        (
            "terminate standalone process job after control EOF: injected job termination failure"
                .to_string(),
            true,
            10,
        )
    );
    Ok(())
}

#[test]
fn launch_commit_is_required_before_the_helper_owner_can_resume_the_target() -> Result<()> {
    let mut terminated = false;
    await_launch_commit(control_file(Some(ParentMessage::CommitLaunch))?, || {
        terminated = true;
        Ok(())
    })?;
    assert!(!terminated);

    let error = await_launch_commit(control_file(None)?, || {
        terminated = true;
        Ok(())
    })
    .expect_err("control EOF before commit must reject target execution");
    assert!(terminated);
    assert_eq!(
        error.to_string(),
        "standalone helper control channel closed before launch commit"
    );
    Ok(())
}
