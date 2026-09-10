use serde::Deserialize;
use serde::Serialize;
use std::io;

pub const FORWARDED: [i32; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

#[derive(Clone, Serialize, Deserialize)]
pub struct SignalState {
    blocked: Vec<i32>,
    ignored: Vec<i32>,
}

pub struct Signals {
    pub original: SignalState,
    wait_set: libc::sigset_t,
}

fn catchable() -> impl Iterator<Item = i32> {
    #[cfg(target_os = "macos")]
    let last = libc::SIGUSR2;
    #[cfg(target_os = "linux")]
    let last = libc::SIGRTMAX();
    let mut valid = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigfillset(&mut valid);
    }
    (1..=last).filter(move |signal| {
        !matches!(*signal, libc::SIGKILL | libc::SIGSTOP)
            && unsafe { libc::sigismember(&valid, *signal) } == 1
    })
}

impl Signals {
    pub fn install() -> io::Result<Self> {
        let mut inherited = unsafe { std::mem::zeroed() };
        let mut wait_set = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut wait_set);
            for signal in FORWARDED.into_iter().chain([libc::SIGCHLD]) {
                libc::sigaddset(&mut wait_set, signal);
            }
        }
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &wait_set, &mut inherited) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        let mut original = SignalState {
            blocked: Vec::new(),
            ignored: Vec::new(),
        };
        for signal in catchable() {
            let mut action = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if action.sa_sigaction == libc::SIG_IGN {
                original.ignored.push(signal);
            }
            if unsafe { libc::sigismember(&inherited, signal) } == 1 {
                original.blocked.push(signal);
            }
        }
        // The supervisor needs waitable children and observable cancellation.
        // Only the native target boundary restores the caller's dispositions.
        for signal in FORWARDED.into_iter().chain([libc::SIGCHLD]) {
            if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { original, wait_set })
    }

    #[cfg(target_os = "linux")]
    pub fn notification(&self) -> io::Result<std::os::fd::OwnedFd> {
        use std::os::fd::FromRawFd;
        let fd =
            unsafe { libc::signalfd(-1, &self.wait_set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }

    pub async fn changed(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let notification = tokio::io::unix::AsyncFd::new(self.notification()?)?;
            let _ready = notification.readable().await?;
        }
        #[cfg(target_os = "macos")]
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        Ok(())
    }

    pub fn pending(&self) -> io::Result<Vec<i32>> {
        let mut result = Vec::new();
        loop {
            let mut pending = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigpending(&mut pending) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if !FORWARDED
                .into_iter()
                .chain([libc::SIGCHLD])
                .any(|signal| unsafe { libc::sigismember(&pending, signal) } == 1)
            {
                break;
            }
            let mut signal = 0;
            let error = unsafe { libc::sigwait(&self.wait_set, &mut signal) };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            if signal != libc::SIGCHLD {
                result.push(signal);
            }
        }
        Ok(result)
    }

    pub fn forwards(&self, signal: i32) -> bool {
        !self.original.blocked.contains(&signal) && !self.original.ignored.contains(&signal)
    }
}

impl SignalState {
    /// Called in Command::pre_exec after native enforcement and std's resets.
    pub unsafe fn restore(&self) -> io::Result<()> {
        let mut mask = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut mask);
        }
        for signal in catchable() {
            let disposition = if self.ignored.contains(&signal) {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            };
            if unsafe { libc::signal(signal, disposition) } == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
        }
        for &signal in &self.blocked {
            unsafe {
                libc::sigaddset(&mut mask, signal);
            }
        }
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(())
    }
}
