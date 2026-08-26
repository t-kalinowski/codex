use std::io;
use std::time::Duration;
use std::time::Instant;

const PROCESS_GROUP_STOP_POLL_INTERVAL: Duration = Duration::from_millis(1);
const PROCESS_GROUP_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Exit status observed without consuming the direct child's wait status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessExitStatus {
    /// The process exited normally with the enclosed exit code.
    Exited(i32),
    /// The process was terminated by the enclosed signal.
    Signaled(i32),
}

/// Wait for a direct child to exit without consuming its wait status.
///
/// Keeping the child waitable prevents its PID from being reused until the
/// caller finishes process-group cleanup and reaps it.
pub fn wait_for_process_exit_without_reaping(process_id: u32) -> io::Result<ProcessExitStatus> {
    observe_process_exit_without_reaping(process_id, ExitObservation::Wait)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "blocking process exit observation returned no status",
        )
    })
}

/// Return a direct child's exit status when available without consuming it.
pub fn try_process_exit_without_reaping(process_id: u32) -> io::Result<Option<ProcessExitStatus>> {
    observe_process_exit_without_reaping(process_id, ExitObservation::Poll)
}

#[derive(Clone, Copy)]
enum ExitObservation {
    Wait,
    Poll,
}

fn observe_process_exit_without_reaping(
    process_id: u32,
    observation: ExitObservation,
) -> io::Result<Option<ProcessExitStatus>> {
    let process_id = valid_process_id(process_id, "process")?;
    let wait_id = libc::id_t::try_from(process_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid process ID"))?;
    let options = libc::WEXITED
        | libc::WNOWAIT
        | match observation {
            ExitObservation::Wait => 0,
            ExitObservation::Poll => libc::WNOHANG,
        };

    loop {
        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result =
            unsafe { libc::waitid(libc::P_PID, wait_id, information.as_mut_ptr(), options) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        let information = unsafe { information.assume_init() };
        let observed_process_id = unsafe { information.si_pid() };
        if observed_process_id == 0 {
            return Ok(None);
        }
        if observed_process_id != process_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "waitid returned process {observed_process_id} while waiting for {process_id}"
                ),
            ));
        }

        let status = unsafe { information.si_status() };
        match information.si_code {
            libc::CLD_EXITED => return Ok(Some(ProcessExitStatus::Exited(status))),
            libc::CLD_KILLED | libc::CLD_DUMPED => {
                return Ok(Some(ProcessExitStatus::Signaled(status)));
            }
            libc::CLD_STOPPED | libc::CLD_CONTINUED => {
                consume_non_exit_notification(wait_id, process_id)?;
            }
            code => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("waitid returned unexpected child status code {code}"),
                ));
            }
        }
    }
}

fn consume_non_exit_notification(wait_id: libc::id_t, process_id: libc::pid_t) -> io::Result<()> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            wait_id,
            information.as_mut_ptr(),
            libc::WSTOPPED | libc::WCONTINUED | libc::WNOHANG,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    let information = unsafe { information.assume_init() };
    let observed_process_id = unsafe { information.si_pid() };
    if observed_process_id == 0 {
        return Ok(());
    }
    if observed_process_id != process_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "waitid returned process {observed_process_id} while consuming a notification for {process_id}"
            ),
        ));
    }
    if !matches!(information.si_code, libc::CLD_STOPPED | libc::CLD_CONTINUED) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "waitid consumed unexpected child status code {}",
                information.si_code
            ),
        ));
    }
    Ok(())
}

fn valid_process_id(process_id: u32, kind: &str) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(process_id)
        .ok()
        .filter(|process_id| *process_id > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {kind} ID")))
}

fn process_group_members(process_group_id: libc::pid_t) -> io::Result<Vec<libc::pid_t>> {
    let mut process_ids: Vec<libc::pid_t> = vec![0; 16];
    loop {
        let buffer_size = libc::c_int::try_from(std::mem::size_of_val(process_ids.as_slice()))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "process group is too large")
            })?;
        clear_errno();
        let count = unsafe {
            libc::proc_listpgrppids(
                process_group_id,
                process_ids.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        if count == 0
            && let Some(error) = current_errno()
        {
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        if count < 0 {
            return Err(current_errno().unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "process-group enumeration returned a negative count",
                )
            }));
        }
        let count = count as usize;
        if count < process_ids.len() {
            process_ids.truncate(count);
            return Ok(process_ids);
        }
        let capacity = process_ids.len().checked_mul(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "process group is too large")
        })?;
        process_ids.resize(capacity, 0);
    }
}

pub(super) fn process_is_live_group_member(
    process_id: libc::pid_t,
    process_group_id: libc::pid_t,
) -> io::Result<bool> {
    let mut information = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let information_size = libc::c_int::try_from(std::mem::size_of_val(&information))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "process info is too large"))?;
    clear_errno();
    let result = unsafe {
        libc::proc_pidinfo(
            process_id,
            libc::PROC_PIDTBSDINFO,
            1,
            information.as_mut_ptr().cast(),
            information_size,
        )
    };
    if result == 0 {
        return match current_errno() {
            Some(error) if error.raw_os_error() != Some(libc::ESRCH) => Err(error),
            Some(_) => Ok(false),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process status returned no data without an error",
            )),
        };
    }
    if result != information_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process status had an unexpected size",
        ));
    }
    let information = unsafe { information.assume_init() };
    Ok(information.pbi_pgid == process_group_id as u32 && information.pbi_status != libc::SZOMB)
}

fn process_group_of(process_id: libc::pid_t) -> io::Result<Option<libc::pid_t>> {
    let process_group_id = unsafe { libc::getpgid(process_id) };
    if process_group_id < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(process_group_id))
}

fn clear_errno() {
    unsafe { *libc::__error() = 0 };
}

fn current_errno() -> Option<io::Error> {
    let error = unsafe { *libc::__error() };
    (error != 0).then(|| io::Error::from_raw_os_error(error))
}

#[derive(Clone, Copy)]
enum ProcessGroupSelection {
    All,
    Except(libc::pid_t),
}

pub(super) fn kill_process_group_until_quiescent_with(
    process_group_id: u32,
    timeout: Duration,
    signal_group: impl FnOnce(libc::pid_t, libc::c_int) -> io::Result<bool>,
    signal_member: impl FnMut(libc::pid_t, libc::c_int) -> io::Result<bool>,
) -> io::Result<()> {
    let process_group_id = valid_process_id(process_group_id, "process group")?;
    match signal_group(process_group_id, libc::SIGKILL) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    kill_selected_process_group_members_until_quiescent(
        process_group_id,
        ProcessGroupSelection::All,
        timeout,
        signal_member,
    )
}

fn kill_selected_process_group_members_until_quiescent(
    process_group_id: libc::pid_t,
    selection: ProcessGroupSelection,
    timeout: Duration,
    mut signal_member: impl FnMut(libc::pid_t, libc::c_int) -> io::Result<bool>,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut process_ids = process_group_members(process_group_id)?;
        if matches!(selection, ProcessGroupSelection::All)
            && !process_ids.contains(&process_group_id)
        {
            process_ids.push(process_group_id);
        }
        process_ids.sort_unstable_by_key(|process_id| *process_id != process_group_id);

        let mut observed_live_member = false;
        let mut first_error = None;
        for process_id in process_ids {
            if process_id <= 0
                || matches!(selection, ProcessGroupSelection::Except(excluded) if process_id == excluded)
            {
                continue;
            }
            match process_is_live_group_member(process_id, process_group_id) {
                Ok(false) => continue,
                Ok(true) => observed_live_member = true,
                Err(error) if first_error.is_none() => {
                    first_error = Some(error);
                    continue;
                }
                Err(_) => continue,
            }
            match process_group_of(process_id) {
                Ok(Some(current_group_id)) if current_group_id == process_group_id => {
                    if let Err(error) = signal_member(process_id, libc::SIGKILL)
                        && first_error.is_none()
                    {
                        first_error = Some(error);
                    }
                }
                Ok(Some(_)) | Ok(None) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }
        if !observed_live_member {
            return Ok(());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "process group {process_group_id} remained live after {} ms",
                    timeout.as_millis()
                ),
            ));
        }
        std::thread::sleep(
            PROCESS_GROUP_STOP_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
        );
    }
}

/// Kill a process group and rescan until no live exact-group member remains.
///
/// The caller must keep the group leader waitable for the duration of this
/// call so the process-group identity cannot be reused during cleanup.
pub fn kill_process_group_until_quiescent(process_group_id: u32) -> io::Result<()> {
    kill_process_group_until_quiescent_with(
        process_group_id,
        PROCESS_GROUP_STOP_TIMEOUT,
        super::signal_process_group_id,
        super::signal_process_id,
    )
}

/// Kill all live exact-group members except one process.
///
/// The excluded process must currently belong to the supplied group. Cleanup
/// rescans until only the excluded process and zombies remain.
pub fn kill_process_group_members_except(
    process_group_id: u32,
    excluded_process_id: u32,
) -> io::Result<()> {
    let process_group_id = valid_process_id(process_group_id, "process group")?;
    let excluded_process_id = valid_process_id(excluded_process_id, "excluded process")?;
    if process_group_of(excluded_process_id)? != Some(process_group_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "excluded process is not a member of the process group",
        ));
    }
    kill_selected_process_group_members_until_quiescent(
        process_group_id,
        ProcessGroupSelection::Except(excluded_process_id),
        PROCESS_GROUP_STOP_TIMEOUT,
        super::signal_process_id,
    )
}
