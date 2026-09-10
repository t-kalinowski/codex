//! Explicit Landlock execution has native exec semantics and no supervisor.
use crate::bootstrap::Bootstrap;
use crate::signals::Signals;
use anyhow::Context;
use std::fs::File;
use std::io::Seek;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::Stdio;

pub async fn run(request: Bootstrap, stdin: File, signals: Signals) -> anyhow::Result<i32> {
    let fd = unsafe {
        libc::memfd_create(
            c"native target setup".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    anyhow::ensure!(
        fd >= 0,
        "create target setup: {}",
        std::io::Error::last_os_error()
    );
    let mut setup = unsafe { File::from_raw_fd(fd) };
    let mut native =
        crate::codex::prepare(request, signals.original, None, setup.as_raw_fd()).await?;
    serde_json::to_writer(&mut setup, &native.setup)?;
    setup.rewind()?;
    if unsafe {
        libc::fcntl(
            fd,
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error()).context("seal target setup");
    }
    let mut command = native.command.take().context("native command")?;
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Err(command.exec()).context("exec native Landlock sandbox")
}
