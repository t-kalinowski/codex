#[cfg(target_os = "macos")]
#[path = "platform_macos.rs"]
mod implementation;
#[cfg(target_os = "linux")]
#[path = "platform_linux.rs"]
mod implementation;
pub use implementation::*;

/// Observe the owned child without releasing its PID or process-group identity.
/// Only the supervisor reaps it, after descendant retirement.
pub fn root_status(pid: i32) -> std::io::Result<Option<i32>> {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        // These waitid si_code values are shared by Darwin and Linux. Darwin's
        // libc crate does not expose the CLD_* constants.
        match info.si_code {
            1 => return Ok(Some(unsafe { info.si_status() })),
            2 | 3 => return Ok(Some(128 + unsafe { info.si_status() })),
            5 | 6 => {
                // Darwin can surface stop/continue notifications even for WEXITED.
                // Consume only that notification; never reap the root here.
                if unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WSTOPPED | libc::WCONTINUED | libc::WNOHANG,
                    )
                } < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            _ => return Err(std::io::Error::other("unexpected native root wait status")),
        }
    }
}
