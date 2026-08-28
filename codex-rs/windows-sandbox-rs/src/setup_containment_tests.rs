use super::McpSetupContainmentJob;
use super::current_setup_containment_parent;
use super::enroll_current_process_in_mcp_setup_job;
use anyhow::Result;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::sync::Mutex;
use std::sync::mpsc::sync_channel;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::System::Threading::CreateMutexW;

const CHILD_TEST: &str = "setup_containment::tests::contained_setup_helper_child";
const CHILD_MARKER_ENV: &str = "MCP_CONSOLE_SETUP_CONTAINMENT_CHILD_MARKER";
const PARENT_PID_ENV: &str = "MCP_CONSOLE_SETUP_CONTAINMENT_PARENT_PID";
const PARENT_CREATION_ENV: &str = "MCP_CONSOLE_SETUP_CONTAINMENT_PARENT_CREATION";

static CONTAINMENT_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn setup_helper_tree_is_retired_on_owner_loss_and_abandoned_lease_recovery() -> Result<()> {
    let _test_guard = CONTAINMENT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir()?;

    let owner_loss_marker = root.path().join("owner-loss-enrolled");
    let owner_job = McpSetupContainmentJob::create()?;
    let mut owner_loss_child = spawn_contained_child(&owner_loss_marker)?;
    wait_for_marker(&owner_loss_marker)?;
    drop(owner_job);
    wait_for_child_exit(&mut owner_loss_child)?;

    let abandoned_marker = root.path().join("abandoned-enrolled");
    let abandoned_job = McpSetupContainmentJob::create()?;
    let mut abandoned_child = spawn_contained_child(&abandoned_marker)?;
    wait_for_marker(&abandoned_marker)?;
    let abandoned_mutex = abandon_policy_mutex()?;
    let error = match crate::policy_lease::acquire_mcp_console_sandbox_policy_lease() {
        Ok(_) => anyhow::bail!(
            "abandoned policy ownership was accepted while its setup Job remained owned"
        ),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("setup containment Job is still owned"),
        "unexpected abandoned-policy error: {error:#}"
    );
    wait_for_child_exit(&mut abandoned_child)?;
    drop(abandoned_job);
    drop(abandoned_mutex);

    let recovered_lease = crate::policy_lease::acquire_mcp_console_sandbox_policy_lease()?;
    drop(recovered_lease);
    drop(McpSetupContainmentJob::create()?);
    Ok(())
}

fn abandon_policy_mutex() -> Result<OwnedHandle> {
    let (sender, receiver) = sync_channel(0);
    let owner = std::thread::spawn(move || -> Result<()> {
        let name =
            crate::winutil::to_wide(crate::policy_lease::MCP_CONSOLE_SANDBOX_POLICY_MUTEX_NAME);
        let handle = unsafe {
            CreateMutexW(
                std::ptr::null_mut(),
                /*b_initial_owner*/ 1,
                name.as_ptr(),
            )
        };
        if handle == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        sender.send(handle)?;
        Ok(())
    });
    let handle = receiver.recv()?;
    owner
        .join()
        .map_err(|_| anyhow::anyhow!("abandoned policy-mutex owner panicked"))??;
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
}

#[test]
#[ignore = "private child entrypoint for the setup containment regression"]
fn contained_setup_helper_child() -> Result<()> {
    let Some(marker) = std::env::var_os(CHILD_MARKER_ENV) else {
        return Ok(());
    };
    let parent_process_id = std::env::var(PARENT_PID_ENV)?.parse()?;
    let parent_creation_time = std::env::var(PARENT_CREATION_ENV)?.parse()?;
    enroll_current_process_in_mcp_setup_job(parent_process_id, parent_creation_time)?;
    std::fs::write(marker, b"enrolled")?;
    std::thread::sleep(Duration::from_secs(30));
    Ok(())
}

fn spawn_contained_child(marker: &Path) -> Result<Child> {
    let parent = current_setup_containment_parent()?;
    Ok(Command::new(std::env::current_exe()?)
        .arg("--ignored")
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--nocapture")
        .env(CHILD_MARKER_ENV, marker)
        .env(PARENT_PID_ENV, parent.process_id.to_string())
        .env(PARENT_CREATION_ENV, parent.creation_time.to_string())
        .spawn()?)
}

fn wait_for_marker(marker: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.is_file() {
        if Instant::now() >= deadline {
            anyhow::bail!("contained setup helper did not enroll within 5 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn wait_for_child_exit(child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("contained setup helper did not exit within 5 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
