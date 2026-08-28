#[cfg(unix)]
mod unix {
    use anyhow::Context;
    use anyhow::Result;
    use std::io::Read;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use tokio::process::Child;
    use tokio::process::Command;

    const WATCHDOG_ARGUMENT: &str = "--mcp-console-sandbox-watch-process-group";
    const WATCHDOG_DISARM: u8 = 2;
    const WATCHDOG_OWNER_FD: i32 = 197;
    const WATCHDOG_READY: u8 = 1;
    const WATCHDOG_READY_TIMEOUT: Duration = Duration::from_secs(2);

    pub struct ProcessGroupWatchdog {
        owner: UnixStream,
        child: Child,
    }

    impl ProcessGroupWatchdog {
        pub async fn start(process_group_id: u32) -> Result<Self> {
            let (watchdog, mut owner) = UnixStream::pair().context("create watchdog channel")?;
            owner
                .set_read_timeout(Some(WATCHDOG_READY_TIMEOUT))
                .context("bound watchdog readiness wait")?;
            let watchdog_fd = watchdog.as_raw_fd();
            let mut command = Command::new(
                std::env::current_exe().context("resolve runner executable for watchdog")?,
            );
            command
                .arg(WATCHDOG_ARGUMENT)
                .arg(process_group_id.to_string())
                .arg(WATCHDOG_OWNER_FD.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
                        if libc::signal(signal, libc::SIG_IGN) == libc::SIG_ERR {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    if libc::dup2(watchdog_fd, WATCHDOG_OWNER_FD) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let flags = libc::fcntl(WATCHDOG_OWNER_FD, libc::F_GETFD);
                    if flags == -1
                        || libc::fcntl(WATCHDOG_OWNER_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC)
                            == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().context("start process-group watchdog")?;
            drop(watchdog);
            let readiness = tokio::task::spawn_blocking(move || {
                let mut ready = [0_u8; 1];
                owner.read_exact(&mut ready)?;
                if ready != [WATCHDOG_READY] {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "watchdog returned an invalid readiness value",
                    ));
                }
                owner.set_read_timeout(None)?;
                Ok(owner)
            })
            .await
            .context("join watchdog readiness task")?
            .context("wait for watchdog readiness");
            let owner = match readiness {
                Ok(owner) => owner,
                Err(error) => {
                    let _ = child.kill().await;
                    return Err(error);
                }
            };
            Ok(Self { owner, child })
        }

        pub async fn disarm(mut self) -> Result<()> {
            self.owner
                .write_all(&[WATCHDOG_DISARM])
                .context("disarm watchdog")?;
            let status = self.child.wait().await.context("wait for watchdog")?;
            anyhow::ensure!(status.success(), "watchdog exited unsuccessfully: {status}");
            Ok(())
        }
    }

    pub fn dispatch_if_requested() -> bool {
        let mut arguments = std::env::args_os().skip(1);
        if arguments.next().as_deref() != Some(std::ffi::OsStr::new(WATCHDOG_ARGUMENT)) {
            return false;
        }
        if let Err(error) = run(arguments) {
            eprintln!("mcp-console-sandbox watchdog error: {error}");
            std::process::exit(125);
        }
        true
    }

    fn run(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
        let process_group_id = arguments
            .next()
            .and_then(|value| value.into_string().ok())
            .and_then(|value| value.parse::<u32>().ok())
            .context("watchdog process group ID is missing or invalid")?;
        let owner_fd = arguments
            .next()
            .and_then(|value| value.into_string().ok())
            .and_then(|value| value.parse::<i32>().ok())
            .context("watchdog owner descriptor is missing or invalid")?;
        anyhow::ensure!(arguments.next().is_none(), "unexpected watchdog argument");
        let flags = unsafe { libc::fcntl(owner_fd, libc::F_GETFD) };
        if flags == -1
            || unsafe { libc::fcntl(owner_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
        {
            return Err(std::io::Error::last_os_error())
                .context("protect watchdog owner descriptor");
        }
        let mut owner = unsafe { UnixStream::from_raw_fd(owner_fd) };
        owner
            .write_all(&[WATCHDOG_READY])
            .context("report watchdog readiness")?;
        let mut byte = [0_u8; 1];
        match owner.read_exact(&mut byte) {
            Ok(()) if byte == [WATCHDOG_DISARM] => Ok(()),
            Ok(()) => {
                let error = std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "watchdog received an invalid owner command",
                );
                kill_process_group(process_group_id)
                    .and(Err(error))
                    .context("read watchdog owner channel")
            }
            Err(error) => kill_process_group(process_group_id)
                .and(Err(error))
                .context("read watchdog owner channel"),
        }
    }

    fn kill_process_group(process_group_id: u32) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        let result = codex_utils_pty::process_group::kill_process_group_with_member_fallback(
            process_group_id,
        );
        #[cfg(not(target_os = "macos"))]
        let result = codex_utils_pty::process_group::kill_process_group(process_group_id);
        match result {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            result => result,
        }
    }

    use std::os::fd::FromRawFd;
}

#[cfg(unix)]
pub use unix::ProcessGroupWatchdog;
#[cfg(unix)]
pub use unix::dispatch_if_requested;

#[cfg(windows)]
pub struct ProcessGroupWatchdog;

#[cfg(windows)]
impl ProcessGroupWatchdog {
    pub async fn disarm(self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
pub fn dispatch_if_requested() -> bool {
    false
}
