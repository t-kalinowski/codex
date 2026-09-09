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
    fd: OwnedFd,
}
impl Parent {
    pub fn capture(pid: i32) -> io::Result<Self> {
        if unsafe { libc::getppid() } != pid {
            return Err(io::Error::other("parent_pid is not the current parent"));
        }
        let value = Self {
            pid,
            fd: pidfd(pid)?,
        };
        if !value.alive()? {
            return Err(io::Error::other("parent exited during startup"));
        }
        Ok(value)
    }
    pub fn alive(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let count = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(count == 0 && unsafe { libc::getppid() } == self.pid)
    }
}

pub struct Tracker;
impl Tracker {
    pub fn new() -> io::Result<Self> {
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self)
    }
    pub fn track_root(&mut self, _: i32) -> io::Result<()> {
        Ok(())
    }
    pub fn observe(&mut self) -> io::Result<()> {
        Ok(())
    }
    pub fn retire_pass(&mut self) -> io::Result<bool> {
        let mut children = Vec::new();
        for task in std::fs::read_dir("/proc/self/task")? {
            match std::fs::read_to_string(task?.path().join("children")) {
                Ok(text) => {
                    for child in text.split_whitespace() {
                        children.push(child.parse::<i32>().map_err(io::Error::other)?);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        children.sort_unstable();
        children.dedup();
        if children.is_empty() {
            return Ok(true);
        }
        let mut failure = None;
        for pid in children {
            let result = (|| {
                let fd = pidfd(pid)?;
                signal(&fd, libc::SIGKILL)?;
                let mut status = 0;
                if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })();
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(false), Err)
    }
}

pub type Target = OwnedFd;
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
    pidfd(credentials.pid).map(Some)
}
pub fn forward(target: &Target, number: i32) -> io::Result<()> {
    signal(target, number)
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
