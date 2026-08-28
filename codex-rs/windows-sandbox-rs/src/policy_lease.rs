use anyhow::Context;
use anyhow::Result;
use std::ffi::OsStr;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::sync_channel;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::WAIT_ABANDONED;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(crate) const MCP_CONSOLE_SANDBOX_POLICY_MUTEX_NAME: &str =
    "Global\\McpConsoleSandboxPolicyGenerationV1";

/// Exclusive lease for the fixed MCP Console identities and policy objects.
///
/// The native mutex is owned by a dedicated thread because Windows mutex
/// ownership is thread-affine while embedding supervisors may move between
/// async runtime threads. The mutex handle is created non-inheritable.
pub struct McpConsoleSandboxPolicyLease {
    release: Option<SyncSender<()>>,
    owner: Option<JoinHandle<()>>,
}

impl Drop for McpConsoleSandboxPolicyLease {
    fn drop(&mut self) {
        self.release.take();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

pub(crate) fn acquire_mcp_console_sandbox_policy_lease() -> Result<McpConsoleSandboxPolicyLease> {
    let (ready_sender, ready_receiver) = sync_channel(1);
    let (release, released) = sync_channel(0);
    let owner = std::thread::Builder::new()
        .name("windows-sandbox-policy-lease".to_string())
        .spawn(move || {
            let name = crate::winutil::to_wide(OsStr::new(
                MCP_CONSOLE_SANDBOX_POLICY_MUTEX_NAME,
            ));
            let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
            if handle == 0 {
                let _ = ready_sender.send(Err(anyhow::anyhow!(
                    "create global Windows sandbox policy mutex: {}",
                    unsafe { GetLastError() }
                )));
                return;
            }
            match unsafe { WaitForSingleObject(handle, 0) } {
                WAIT_OBJECT_0 => {
                    match crate::setup_containment::retire_abandoned_mcp_setup_job() {
                        Ok(()) => {
                            if ready_sender.send(Ok(())).is_ok() {
                                let _ = released.recv();
                            }
                        }
                        Err(error) => {
                            let _ = ready_sender.send(Err(error.context(
                                "retire a prior Windows sandbox setup helper before acquiring policy ownership",
                            )));
                        }
                    }
                    unsafe {
                        let _ = ReleaseMutex(handle);
                        CloseHandle(handle);
                    }
                }
                WAIT_ABANDONED => {
                    let retirement =
                        crate::setup_containment::retire_abandoned_mcp_setup_job();
                    unsafe {
                        let _ = ReleaseMutex(handle);
                        CloseHandle(handle);
                    }
                    let message = match retirement {
                        Ok(()) => "the previous Windows sandbox policy mutation was abandoned and its helper was retired; run setup prepare before launch".to_string(),
                        Err(error) => format!(
                            "the previous Windows sandbox policy mutation was abandoned, and its helper could not be retired: {error:#}"
                        ),
                    };
                    let _ = ready_sender.send(Err(anyhow::anyhow!(message)));
                }
                WAIT_TIMEOUT => {
                    unsafe { CloseHandle(handle) };
                    let _ = ready_sender.send(Err(anyhow::anyhow!(
                        "another Windows sandbox policy generation is active"
                    )));
                }
                WAIT_FAILED => {
                    let error = unsafe { GetLastError() };
                    unsafe { CloseHandle(handle) };
                    let _ = ready_sender.send(Err(anyhow::anyhow!(
                        "wait for global Windows sandbox policy mutex: {error}"
                    )));
                }
                result => {
                    unsafe { CloseHandle(handle) };
                    let _ = ready_sender.send(Err(anyhow::anyhow!(
                        "unexpected global Windows sandbox policy mutex wait result: {result:#x}"
                    )));
                }
            }
        })
        .context("start Windows sandbox policy lease owner")?;
    match ready_receiver
        .recv()
        .context("Windows sandbox policy lease owner stopped before reporting readiness")?
    {
        Ok(()) => Ok(McpConsoleSandboxPolicyLease {
            release: Some(release),
            owner: Some(owner),
        }),
        Err(error) => {
            drop(release);
            let _ = owner.join();
            Err(error)
        }
    }
}
