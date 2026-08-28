#[cfg(unix)]
mod unix {
    use anyhow::Context;
    use anyhow::Result;
    use std::io::Read;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::process::Child;
    use tokio::process::Command;
    use tokio::sync::watch;

    const WATCHDOG_ARGUMENT: &str = "--mcp-console-sandbox-watch-process-group";
    const WATCHDOG_DISARM: u8 = 2;
    #[cfg(target_os = "linux")]
    const WATCHDOG_REGISTER_NAMESPACE_PROCESS: u8 = 3;
    #[cfg(target_os = "linux")]
    const WATCHDOG_REGISTERED: u8 = 4;
    const WATCHDOG_OWNER_FD: i32 = 197;
    const WATCHDOG_READY: u8 = 1;
    const WATCHDOG_READY_TIMEOUT: Duration = Duration::from_secs(2);
    const WATCHDOG_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

    pub struct ProcessGroupWatchdog {
        owner: UnixStream,
        exit: watch::Receiver<Option<std::result::Result<(), String>>>,
        child: Arc<Mutex<Option<Child>>>,
    }

    pub(crate) struct WatchdogExitReceiver {
        receiver: watch::Receiver<Option<std::result::Result<(), String>>>,
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
            let (exit_sender, exit) = watch::channel(None);
            let child = Arc::new(Mutex::new(Some(child)));
            let observed_child = Arc::clone(&child);
            tokio::spawn(async move {
                loop {
                    let observation = {
                        let mut child = observed_child
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let Some(running_child) = child.as_mut() else {
                            return;
                        };
                        let observation = running_child.try_wait();
                        if !matches!(observation, Ok(None)) {
                            child.take();
                        }
                        observation
                    };
                    match observation {
                        Ok(Some(status)) => {
                            let result = status
                                .success()
                                .then_some(())
                                .ok_or_else(|| format!("watchdog exited unsuccessfully: {status}"));
                            exit_sender.send_replace(Some(result));
                            return;
                        }
                        Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                        Err(error) => {
                            exit_sender
                                .send_replace(Some(Err(format!("wait for watchdog: {error}"))));
                            return;
                        }
                    }
                }
            });
            Ok(Self { owner, exit, child })
        }

        #[cfg(target_os = "linux")]
        pub fn register_namespace_process(&self, process_id: u32) -> Result<()> {
            let mut owner = &self.owner;
            owner
                .write_all(&[WATCHDOG_REGISTER_NAMESPACE_PROCESS])
                .and_then(|()| owner.write_all(&process_id.to_be_bytes()))
                .context("register namespace process with watchdog")?;
            let mut acknowledgement = [0_u8; 1];
            owner
                .read_exact(&mut acknowledgement)
                .context("wait for watchdog namespace registration")?;
            anyhow::ensure!(
                acknowledgement == [WATCHDOG_REGISTERED],
                "watchdog returned an invalid namespace registration acknowledgement"
            );
            Ok(())
        }

        pub(crate) fn exit_receiver(&self) -> WatchdogExitReceiver {
            WatchdogExitReceiver {
                receiver: self.exit.clone(),
            }
        }

        pub async fn disarm(self) -> Result<()> {
            if self.exit.borrow().is_none() {
                let mut owner = &self.owner;
                owner
                    .write_all(&[WATCHDOG_DISARM])
                    .context("disarm watchdog")?;
            }
            WatchdogExitReceiver {
                receiver: self.exit,
            }
            .wait_bounded(&self.child)
            .await
        }

        pub async fn retire(self) -> Result<()> {
            drop(self.owner);
            WatchdogExitReceiver {
                receiver: self.exit,
            }
            .wait_bounded(&self.child)
            .await
        }
    }

    impl WatchdogExitReceiver {
        pub(crate) async fn wait(&mut self) -> Result<()> {
            loop {
                if let Some(result) = self.receiver.borrow().clone() {
                    return result.map_err(anyhow::Error::msg);
                }
                self.receiver
                    .changed()
                    .await
                    .context("watchdog exit channel closed")?;
            }
        }

        async fn wait_bounded(mut self, child: &Mutex<Option<Child>>) -> Result<()> {
            match tokio::time::timeout(WATCHDOG_EXIT_TIMEOUT, self.wait()).await {
                Ok(result) => result,
                Err(_) => {
                    let kill_error = {
                        let mut child = child
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        child.as_mut().map(Child::start_kill).transpose().err()
                    };
                    let forced_exit =
                        tokio::time::timeout(WATCHDOG_EXIT_TIMEOUT, self.wait()).await;
                    let mut detail =
                        "watchdog exit timed out and forced retirement was requested".to_string();
                    if let Some(error) = kill_error {
                        detail.push_str(&format!("; watchdog kill failed: {error}"));
                    }
                    match forced_exit {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            detail.push_str(&format!("; forced watchdog outcome: {error:#}"));
                        }
                        Err(_) => detail.push_str("; watchdog remained active after SIGKILL"),
                    }
                    anyhow::bail!(detail)
                }
            }
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
        #[cfg(target_os = "linux")]
        let mut namespace_process = None;
        loop {
            let mut command = [0_u8; 1];
            if owner.read_exact(&mut command).is_err() {
                return retire_owned_processes(
                    process_group_id,
                    #[cfg(target_os = "linux")]
                    namespace_process.as_ref(),
                );
            }
            match command[0] {
                WATCHDOG_DISARM => {
                    #[cfg(target_os = "linux")]
                    if let Some(process) = namespace_process.as_ref() {
                        match process.has_exited() {
                            Ok(true) => {}
                            Ok(false) => {
                                retire_owned_processes(
                                    process_group_id,
                                    namespace_process.as_ref(),
                                )?;
                                anyhow::bail!(
                                    "namespace process remained active while disarming watchdog"
                                );
                            }
                            Err(error) => {
                                retire_owned_processes(
                                    process_group_id,
                                    namespace_process.as_ref(),
                                )?;
                                return Err(error).context("inspect namespace process retirement");
                            }
                        }
                    }
                    return Ok(());
                }
                #[cfg(target_os = "linux")]
                WATCHDOG_REGISTER_NAMESPACE_PROCESS => {
                    let registration = (|| -> Result<()> {
                        anyhow::ensure!(
                            namespace_process.is_none(),
                            "watchdog namespace process is already registered"
                        );
                        let mut process_id = [0_u8; 4];
                        owner
                            .read_exact(&mut process_id)
                            .context("read watchdog namespace process ID")?;
                        namespace_process = Some(
                            crate::linux_process::LinuxProcess::open(u32::from_be_bytes(
                                process_id,
                            ))
                            .context("open watchdog namespace process")?,
                        );
                        owner
                            .write_all(&[WATCHDOG_REGISTERED])
                            .context("acknowledge watchdog namespace registration")
                    })();
                    if let Err(error) = registration {
                        retire_owned_processes(process_group_id, namespace_process.as_ref())?;
                        return Err(error);
                    }
                }
                _ => {
                    retire_owned_processes(
                        process_group_id,
                        #[cfg(target_os = "linux")]
                        namespace_process.as_ref(),
                    )?;
                    anyhow::bail!("watchdog received an invalid owner command");
                }
            }
        }
    }

    fn retire_owned_processes(
        process_group_id: u32,
        #[cfg(target_os = "linux")] namespace_process: Option<&crate::linux_process::LinuxProcess>,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        let namespace_result = match namespace_process {
            Some(namespace_process) => namespace_process
                .kill()
                .context("retire sandbox namespace process"),
            None => Ok(()),
        };
        let process_group_result =
            kill_process_group(process_group_id).context("retire sandbox process group");
        #[cfg(target_os = "linux")]
        match (namespace_result, process_group_result) {
            (Err(namespace_error), Err(process_group_error)) => anyhow::bail!(
                "{namespace_error:#}; sandbox process-group retirement also failed: {process_group_error:#}"
            ),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
        #[cfg(not(target_os = "linux"))]
        process_group_result
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
