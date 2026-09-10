//! Native namespace-init control. Only the host runner owns the other endpoint.
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

pub(crate) fn wait(channel: OwnedFd, command: libc::pid_t) -> ! {
    let result = run(channel, command);
    match result {
        Ok(status) => super::linux_run_main::exit_with_wait_status(status),
        Err(error) => {
            eprintln!("native namespace control: {error}");
            std::process::exit(1);
        }
    }
}

fn run(channel: OwnedFd, command: libc::pid_t) -> io::Result<i32> {
    let mut mask = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGCHLD);
    }
    let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    let fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let signals = unsafe { OwnedFd::from_raw_fd(fd) };
    loop {
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if reaped == command {
            return Ok(status);
        }
        if reaped > 0 {
            continue;
        }
        if reaped < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let mut descriptors = [
            libc::pollfd {
                fd: channel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signals.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if descriptors[0].revents != 0 {
            let mut signal = 0u8;
            match unsafe { libc::read(channel.as_raw_fd(), (&mut signal as *mut u8).cast(), 1) } {
                // Exiting PID 1 makes the kernel retire the entire namespace.
                // Its native parent remains alive to wait for that completion.
                0 => return Ok(0),
                1 if [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM]
                    .contains(&i32::from(signal)) =>
                {
                    if unsafe { libc::kill(command, i32::from(signal)) } < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                _ => return Err(io::Error::other("invalid native control message")),
            }
        }
        if descriptors[1].revents != 0 {
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::read(
                    signals.as_raw_fd(),
                    (&mut info as *mut libc::signalfd_siginfo).cast(),
                    std::mem::size_of_val(&info),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
    }
}
