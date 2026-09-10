use std::collections::HashMap;
use std::io;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    pid: i32,
    seconds: u64,
    microseconds: u64,
}
struct Process {
    identity: Identity,
    parent: i32,
    group: i32,
    zombie: bool,
}

fn process(pid: i32) -> io::Result<Option<Process>> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    unsafe {
        *libc::__error() = 0;
    }
    let size = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            std::mem::size_of_val(&info) as i32,
        )
    };
    if size == std::mem::size_of_val(&info) as i32 {
        return Ok(Some(Process {
            identity: Identity {
                pid,
                seconds: info.pbi_start_tvsec,
                microseconds: info.pbi_start_tvusec,
            },
            parent: info.pbi_ppid as i32,
            group: info.pbi_pgid as i32,
            zombie: info.pbi_status == libc::SZOMB,
        }));
    }
    let error = io::Error::last_os_error();
    if size == 0 && error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(None);
    }
    Err(io::Error::other(format!(
        "process discovery for {pid}: proc_pidinfo returned {size}: {error}"
    )))
}

fn list_pids(
    pid: i32,
    list: unsafe extern "C" fn(i32, *mut libc::c_void, i32) -> i32,
) -> io::Result<Vec<i32>> {
    let mut capacity = 16;
    loop {
        let mut pids = vec![0; capacity];
        unsafe {
            *libc::__error() = 0;
        }
        let count = unsafe {
            list(
                pid,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(pids.as_slice()) as i32,
            )
        };
        if count == 0 {
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(0 | libc::ESRCH)) {
                return Ok(Vec::new());
            }
            return Err(io::Error::other(format!(
                "process discovery for {pid}: {error}"
            )));
        }
        if count < 0 {
            return Err(io::Error::other("process enumeration failed"));
        }
        if (count as usize) < capacity {
            pids.truncate(count as usize);
            return Ok(pids);
        }
        capacity *= 2;
    }
}

pub struct Parent(Identity);
impl Parent {
    pub fn capture(pid: i32) -> io::Result<Self> {
        if unsafe { libc::getppid() } != pid {
            return Err(io::Error::other("parent_pid is not the current parent"));
        }
        let parent = process(pid)?
            .filter(|p| !p.zombie)
            .ok_or_else(|| io::Error::other("parent exited during startup"))?;
        let value = Self(parent.identity);
        if !value.alive()? {
            return Err(io::Error::other("parent exited during startup"));
        }
        Ok(value)
    }
    pub fn alive(&self) -> io::Result<bool> {
        Ok(unsafe { libc::getppid() } == self.0.pid
            && process(self.0.pid)?.is_some_and(|p| p.identity == self.0 && !p.zombie))
    }
}

pub struct Tracker {
    queue: OwnedFd,
    active: HashMap<i32, Identity>,
    root: Option<Identity>,
}
impl Tracker {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let queue = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            queue,
            active: HashMap::new(),
            root: None,
        })
    }

    pub fn track_root(&mut self, pid: i32) -> io::Result<()> {
        self.root = Some(
            process(pid)?
                .ok_or_else(|| io::Error::other("native root disappeared before observation"))?
                .identity,
        );
        self.add(pid, /*parent*/ None)
    }

    fn add(&mut self, pid: i32, parent: Option<Identity>) -> io::Result<()> {
        let Some(info) = process(pid)? else {
            return Ok(());
        };
        if let Some(parent) = parent
            && (info.parent != parent.pid
                || !process(parent.pid)?.is_some_and(|p| p.identity == parent))
        {
            return Ok(());
        }
        if self.active.get(&pid) == Some(&info.identity) {
            return Ok(());
        }
        self.active.insert(pid, info.identity);
        if info.zombie {
            return Ok(());
        }
        let event = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_FORK | libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(io::Error::other(format!(
                "watch sandbox process {pid}: {error}"
            )));
        }
        self.children(info.identity)
    }

    fn children(&mut self, parent: Identity) -> io::Result<()> {
        if !process(parent.pid)?.is_some_and(|p| p.identity == parent && !p.zombie) {
            return Ok(());
        }
        for pid in list_pids(parent.pid, libc::proc_listchildpids)? {
            self.add(pid, Some(parent))?;
        }
        Ok(())
    }

    pub fn observe(&mut self) -> io::Result<()> {
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut events: [libc::kevent; 64] = unsafe { std::mem::zeroed() };
        loop {
            let count = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    &timeout,
                )
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                return Ok(());
            }
            for event in events.iter().take(count as usize) {
                if event.flags & libc::EV_ERROR != 0 {
                    return Err(io::Error::other(format!(
                        "process observation event error {}",
                        { event.data }
                    )));
                }
                if event.fflags & libc::NOTE_FORK != 0
                    && let Some(parent) = self.active.get(&(event.ident as i32)).copied()
                {
                    self.children(parent)?;
                }
            }
        }
    }

    #[expect(
        clippy::needless_collect,
        reason = "Discovery registers children; retirement removes identities from the iterated map."
    )]
    pub fn retire_pass(&mut self) -> io::Result<bool> {
        let mut error = self.observe().err();
        for parent in self.active.values().copied().collect::<Vec<_>>() {
            if let Err(next) = self.children(parent) {
                error.get_or_insert(next);
            }
        }
        if let Some(root) = self.root {
            // The supervisor keeps this direct child unreaped, pinning the
            // original group number even after exit or a group change. Group
            // retirement covers forks that orphaned before event discovery.
            let group = (|| {
                if !process(root.pid)?.is_some_and(|p| p.identity == root) {
                    return Err(io::Error::other(
                        "native root identity lost before retirement",
                    ));
                }
                // Signal checked live members below. Darwin can return EPERM
                // for killpg when the group's only remaining members are zombies.
                for pid in list_pids(root.pid, libc::proc_listpgrppids)? {
                    if let Some(info) = process(pid)?
                        && info.group == root.pid
                        && !info.zombie
                    {
                        self.active.insert(pid, info.identity);
                    }
                }
                Ok(())
            })();
            if let Err(next) = group {
                error.get_or_insert(next);
            }
        }
        for identity in self.active.values().copied().collect::<Vec<_>>() {
            match process(identity.pid) {
                Ok(Some(info)) if info.identity == identity && !info.zombie => {
                    // Darwin has no pidfd signal primitive. Recheck the start time
                    // immediately before kill; the residual PID reuse race is documented.
                    if unsafe { libc::kill(identity.pid, libc::SIGKILL) } < 0 {
                        let next = io::Error::last_os_error();
                        if next.raw_os_error() != Some(libc::ESRCH) {
                            error.get_or_insert(next);
                        }
                    }
                }
                Ok(_) => {
                    self.active.remove(&identity.pid);
                }
                Err(next) => {
                    error.get_or_insert(next);
                }
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        Ok(self.active.is_empty())
    }
}

pub fn configure_channel(_: &UnixStream) -> io::Result<()> {
    Ok(())
}
pub struct Target(i32);
pub fn receive_ready(stream: &mut UnixStream, root: i32) -> io::Result<Option<Target>> {
    match stream.read(&mut [0]) {
        Ok(1) => Ok(Some(Target(root))),
        Ok(_) => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}
pub fn forward(pid: &Target, signal: i32) -> io::Result<()> {
    if unsafe { libc::kill(-pid.0, signal) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

pub struct Terminal {
    descriptor: Option<OwnedFd>,
    group: i32,
}
impl Terminal {
    pub fn capture() -> io::Result<Self> {
        let group = unsafe { libc::getpgrp() };
        let own_pid = unsafe { libc::getpid() };
        let candidate = (0..=2).find(|fd| unsafe { libc::tcgetpgrp(*fd) } == group);
        let descriptor = if let Some(fd) = candidate
            && !list_pids(group, libc::proc_listpgrppids)?
                .into_iter()
                .any(|pid| pid != own_pid)
        {
            let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if copy < 0 {
                return Err(io::Error::last_os_error());
            }
            Some(unsafe { OwnedFd::from_raw_fd(copy) })
        } else {
            None
        };
        Ok(Self { descriptor, group })
    }
    pub fn transfer(&self, root: i32) -> io::Result<()> {
        self.set(root)
    }
    pub fn restore(&self) -> io::Result<()> {
        self.set(self.group)
    }
    fn set(&self, group: i32) -> io::Result<()> {
        let Some(fd) = &self.descriptor else {
            return Ok(());
        };
        let mut set = unsafe { std::mem::zeroed() };
        let mut old = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTTOU);
        }
        let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old) };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let result = unsafe { libc::tcsetpgrp(fd.as_raw_fd(), group) };
        let error = io::Error::last_os_error();
        let restored =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
        if restored != 0 {
            return Err(io::Error::from_raw_os_error(restored));
        }
        if result < 0 && error.raw_os_error() != Some(libc::ENOTTY) {
            return Err(error);
        }
        Ok(())
    }
}
