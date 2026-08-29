#![cfg(target_os = "macos")]

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: libc::pid_t,
    started_seconds: u64,
    started_microseconds: u64,
}

pub(crate) struct ProcessInfo {
    pub(crate) identity: ProcessIdentity,
    pub(crate) parent_pid: libc::pid_t,
    pub(crate) is_zombie: bool,
}

pub(crate) fn list_child_pids(parent: libc::pid_t) -> Result<Vec<libc::pid_t>, String> {
    let mut capacity = 16;
    loop {
        let mut children = vec![0; capacity];
        unsafe { *libc::__error() = 0 };
        let count = unsafe {
            libc::proc_listchildpids(
                parent,
                children.as_mut_ptr().cast(),
                std::mem::size_of_val(children.as_slice()) as libc::c_int,
            )
        };
        if count == 0 {
            let error_code = unsafe { *libc::__error() };
            if error_code == 0 || error_code == libc::ESRCH {
                return Ok(Vec::new());
            }
            if error_code == libc::EINTR {
                continue;
            }
            return Err(format!(
                "failed to list children of sandbox process {parent}: {}",
                std::io::Error::from_raw_os_error(error_code)
            ));
        }
        if count < 0 {
            return Err(format!(
                "failed to list children of sandbox process {parent}: proc_listchildpids returned {count}"
            ));
        }
        let count = count as usize;
        if count < capacity {
            children.truncate(count);
            return Ok(children);
        }
        capacity = capacity.saturating_mul(2).max(count + 16);
    }
}

pub(crate) fn process_info(pid: libc::pid_t) -> Result<Option<ProcessInfo>, String> {
    if pid <= 0 {
        return Err(format!("invalid sandbox process PID {pid}"));
    }
    let expected_size = std::mem::size_of::<libc::proc_bsdinfo>();
    loop {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        unsafe { *libc::__error() = 0 };
        let size = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                1,
                info.as_mut_ptr().cast(),
                expected_size as libc::c_int,
            )
        };
        if size as usize == expected_size {
            let info = unsafe { info.assume_init() };
            return Ok(Some(ProcessInfo {
                identity: ProcessIdentity {
                    pid,
                    started_seconds: info.pbi_start_tvsec,
                    started_microseconds: info.pbi_start_tvusec,
                },
                parent_pid: info.pbi_ppid as libc::pid_t,
                is_zombie: info.pbi_status == libc::SZOMB,
            }));
        }
        let error_code = unsafe { *libc::__error() };
        if size == 0 && error_code == libc::ESRCH {
            return Ok(None);
        }
        if size == 0 && error_code == libc::EINTR {
            continue;
        }
        if size == 0 && error_code != 0 {
            return Err(format!(
                "failed to inspect sandbox process {pid}: {}",
                std::io::Error::from_raw_os_error(error_code)
            ));
        }
        return Err(format!(
            "failed to inspect sandbox process {pid}: proc_pidinfo returned {size} bytes, expected {expected_size}"
        ));
    }
}

pub(crate) fn process_identity(pid: libc::pid_t) -> Result<Option<ProcessIdentity>, String> {
    Ok(process_info(pid)?.map(|info| info.identity))
}

pub(crate) fn signal_process(
    identity: ProcessIdentity,
    signal: libc::c_int,
) -> Result<bool, String> {
    let Some(info) = process_info(identity.pid)? else {
        return Ok(false);
    };
    if info.identity != identity || info.is_zombie {
        return Ok(false);
    }
    let result = unsafe { libc::kill(identity.pid, signal) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(format!(
            "failed to signal sandbox descendant {}: {error}",
            identity.pid
        ))
    }
}
