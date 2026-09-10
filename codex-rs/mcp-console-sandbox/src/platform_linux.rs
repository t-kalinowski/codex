use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

fn pidfd(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn signal(fd: &OwnedFd, signal: i32) -> io::Result<()> {
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

pub struct Parent {
    pid: i32,
}
impl Parent {
    pub fn capture(pid: i32) -> io::Result<Self> {
        if unsafe { libc::getppid() } != pid {
            return Err(io::Error::other("parent_pid is not the current parent"));
        }
        // The caller is our direct parent. Its death changes getppid and wakes
        // the existing SIGCHLD wait set; no PID lookup or pidfd is needed.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGCHLD, 0, 0, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let value = Self { pid };
        if !value.alive()? {
            return Err(io::Error::other("parent exited during startup"));
        }
        Ok(value)
    }
    pub fn alive(&self) -> io::Result<bool> {
        Ok(unsafe { libc::getppid() } == self.pid)
    }
}

pub struct Tracker {
    root: Option<i32>,
}
impl Tracker {
    pub fn new() -> io::Result<Self> {
        Ok(Self { root: None })
    }
    pub fn track_root(&mut self, pid: i32) -> io::Result<()> {
        self.root = Some(pid);
        Ok(())
    }
    pub fn observe(&mut self) -> io::Result<()> {
        Ok(())
    }
    pub fn retire_pass(&mut self) -> io::Result<bool> {
        // The native monitor reaps namespace init after the kernel has retired
        // that namespace. Keep our direct child waitable until this barrier.
        self.root.map_or(Ok(true), |pid| {
            super::root_status(pid).map(|status| status.is_some())
        })
    }
}

pub struct Target {
    channel: UnixStream,
    // Optional identity-safe emergency termination also reaches a stopped init.
    // Ordinary forwarding and retirement use the private native channel.
    pidfd: Option<OwnedFd>,
}
pub fn configure_channel(stream: &UnixStream) -> io::Result<()> {
    let enabled: libc::c_int = 1;
    if unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&enabled as *const libc::c_int).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn receive_ready(stream: &mut UnixStream, _: i32) -> io::Result<Option<Target>> {
    let mut byte = 0u8;
    let mut control = [0usize; 16];
    let mut vector = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_DONTWAIT) };
    if count < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error);
    }
    if count == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    if count != 1 || byte != 1 || message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("invalid native readiness"));
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null()
        || unsafe {
            (*header).cmsg_level != libc::SOL_SOCKET
                || (*header).cmsg_type != libc::SCM_CREDENTIALS
                || (*header).cmsg_len
                    != libc::CMSG_LEN(std::mem::size_of::<libc::ucred>() as u32) as usize
        }
    {
        return Err(io::Error::other("native readiness lacks credentials"));
    }
    let credentials = unsafe {
        libc::CMSG_DATA(header)
            .cast::<libc::ucred>()
            .read_unaligned()
    };
    if credentials.uid != unsafe { libc::geteuid() } || credentials.pid <= 1 {
        return Err(io::Error::other("invalid native readiness identity"));
    }
    let pidfd = match pidfd(credentials.pid) {
        Ok(fd) => Some(fd),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS | libc::EPERM | libc::EACCES)
            ) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    Ok(Some(Target {
        channel: stream.try_clone()?,
        pidfd,
    }))
}
pub fn forward(target: &Target, number: i32) -> io::Result<()> {
    if number == libc::SIGKILL {
        if let Some(fd) = &target.pidfd {
            match signal(fd, number) {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENOSYS | libc::EPERM | libc::EACCES)
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        return target.channel.shutdown(std::net::Shutdown::Write);
    }
    let byte = number as u8;
    if unsafe { libc::write(target.channel.as_raw_fd(), (&byte as *const u8).cast(), 1) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::BrokenPipe {
            return Err(error);
        }
    }
    Ok(())
}

// Bubblewrap owns the target's separate session. Original terminal descriptors
// remain attached; host foreground ownership stays with the calling job.
pub struct Terminal;
impl Terminal {
    pub fn capture() -> io::Result<Self> {
        Ok(Self)
    }
    pub fn transfer(&self, _: i32) -> io::Result<()> {
        Ok(())
    }
    pub fn restore(&self) -> io::Result<()> {
        Ok(())
    }
}
