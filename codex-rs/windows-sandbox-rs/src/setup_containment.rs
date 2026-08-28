use anyhow::Context;
use anyhow::Result;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::SetLastError;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectBasicAccountingInformation;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::OpenJobObjectW;
use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_ASSIGN_PROCESS;
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_TERMINATE;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::System::Threading::GetProcessTimes;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const MCP_SETUP_JOB_NAME: &str = r"Global\McpConsoleSandboxSetupGenerationV1";
const SETUP_RETIREMENT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
#[path = "setup_containment_tests.rs"]
mod tests;

/// Parent process identity captured before a standalone setup helper starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SetupContainmentParent {
    pub(crate) process_id: u32,
    pub(crate) creation_time: u64,
}

/// Parent-owned Job that contains one standalone setup helper generation.
pub(crate) struct McpSetupContainmentJob(OwnedHandle);

impl McpSetupContainmentJob {
    pub(crate) fn create() -> Result<Self> {
        let name = crate::winutil::to_wide(MCP_SETUP_JOB_NAME);
        unsafe { SetLastError(ERROR_SUCCESS) };
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
        if handle == 0 {
            return Err(std::io::Error::last_os_error())
                .context("create standalone Windows setup containment Job");
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            anyhow::bail!("a previous standalone Windows setup containment Job is still present");
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("configure standalone Windows setup containment Job");
        }
        Ok(Self(handle))
    }

    pub(crate) fn retire_remaining(&self) -> Result<()> {
        terminate_and_wait(self.0.as_raw_handle() as HANDLE)
    }
}

pub(crate) fn current_setup_containment_parent() -> Result<SetupContainmentParent> {
    Ok(SetupContainmentParent {
        process_id: unsafe { GetCurrentProcessId() },
        creation_time: process_creation_time(unsafe { GetCurrentProcess() })?,
    })
}

/// Enrolls the current standalone setup helper in its parent's Job before any
/// machine-global policy mutation begins.
///
/// The parent process identity is checked on both sides of assignment. If the
/// parent exits before enrollment, the named Job disappears and setup fails. If
/// it exits after enrollment, `KILL_ON_JOB_CLOSE` retires the helper tree.
#[doc(hidden)]
pub fn enroll_current_process_in_mcp_setup_job(
    parent_process_id: u32,
    parent_creation_time: u64,
) -> Result<()> {
    let parent = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            /*b_inherit_handle*/ 0,
            parent_process_id,
        )
    };
    if parent == 0 {
        return Err(std::io::Error::last_os_error())
            .context("open standalone setup parent process");
    }
    let parent = unsafe { OwnedHandle::from_raw_handle(parent as _) };
    if process_creation_time(parent.as_raw_handle() as HANDLE)? != parent_creation_time {
        anyhow::bail!("standalone setup parent process identity changed before enrollment");
    }
    require_parent_alive(parent.as_raw_handle() as HANDLE)?;

    let name = crate::winutil::to_wide(MCP_SETUP_JOB_NAME);
    let job = unsafe {
        OpenJobObjectW(
            JOB_OBJECT_ASSIGN_PROCESS,
            /*b_inherit_handle*/ 0,
            name.as_ptr(),
        )
    };
    if job == 0 {
        return Err(std::io::Error::last_os_error())
            .context("open standalone Windows setup containment Job");
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job as _) };
    if unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, GetCurrentProcess()) } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("enroll standalone setup helper in containment Job");
    }
    drop(job);
    require_parent_alive(parent.as_raw_handle() as HANDLE)
}

/// Retires a setup Job left behind by an abandoned policy lease. A missing Job
/// means a not-yet-enrolled helper can no longer open it and must fail before
/// mutating policy.
pub(crate) fn retire_abandoned_mcp_setup_job() -> Result<()> {
    let name = crate::winutil::to_wide(MCP_SETUP_JOB_NAME);
    let handle = unsafe {
        OpenJobObjectW(
            JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE,
            /*b_inherit_handle*/ 0,
            name.as_ptr(),
        )
    };
    if handle == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        anyhow::bail!("open prior standalone Windows setup containment Job: {error}");
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
    terminate_and_wait(handle.as_raw_handle() as HANDLE)
        .context("retire prior standalone Windows setup helper generation")?;
    drop(handle);

    unsafe { SetLastError(ERROR_SUCCESS) };
    let probe = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
    if probe == 0 {
        return Err(std::io::Error::last_os_error())
            .context("verify prior standalone Windows setup containment ownership ended");
    }
    let probe = unsafe { OwnedHandle::from_raw_handle(probe as _) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        anyhow::bail!(
            "the prior standalone Windows setup containment Job is still owned after its helper tree retired"
        );
    }
    drop(probe);
    Ok(())
}

fn require_parent_alive(parent: HANDLE) -> Result<()> {
    match unsafe {
        WaitForSingleObject(parent, /*dw_milliseconds*/ 0)
    } {
        WAIT_TIMEOUT => Ok(()),
        WAIT_OBJECT_0 => anyhow::bail!("standalone setup parent exited before helper enrollment"),
        result => anyhow::bail!(
            "wait for standalone setup parent process returned unexpected result {result:#x}"
        ),
    }
}

fn process_creation_time(process: HANDLE) -> Result<u64> {
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error()).context("read process creation time");
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn terminate_and_wait(job: HANDLE) -> Result<()> {
    if unsafe {
        TerminateJobObject(job, /*u_exit_code*/ 1)
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("terminate standalone Windows setup containment Job");
    }
    let deadline = Instant::now() + SETUP_RETIREMENT_TIMEOUT;
    loop {
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                std::ptr::addr_of_mut!(accounting).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("query standalone Windows setup containment Job");
        }
        if accounting.ActiveProcesses == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("standalone Windows setup helper tree did not retire within 5 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
