use anyhow::Context;
use anyhow::Result;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use tokio::io::AsyncReadExt;

const BWRAP_INFO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BWRAP_INFO_SIZE: usize = 16 * 1024;

pub(crate) struct LinuxProcess {
    descriptor: OwnedFd,
}

impl LinuxProcess {
    pub(crate) fn open(process_id: u32) -> io::Result<Self> {
        let process_id = i32::try_from(process_id)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process ID exceeds i32"))?;
        let descriptor = unsafe {
            libc::syscall(libc::SYS_pidfd_open, process_id, /*flags*/ 0)
        };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor: unsafe { OwnedFd::from_raw_fd(descriptor as i32) },
        })
    }

    pub(crate) fn kill(&self) -> io::Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.descriptor.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                /*flags*/ 0,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    pub(crate) fn has_exited(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe {
            libc::poll(&mut descriptor, /*nfds*/ 1, /*timeout*/ 0)
        } {
            0 => Ok(false),
            1 => Ok(descriptor.revents & libc::POLLIN != 0),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub(crate) async fn wait_until(&self, deadline: tokio::time::Instant) -> io::Result<bool> {
        loop {
            if self.has_exited()? {
                return Ok(true);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            tokio::time::sleep((deadline - now).min(Duration::from_millis(10))).await;
        }
    }
}

pub(crate) struct BwrapInfoReader {
    reader: tokio::fs::File,
}

pub(crate) struct BwrapInfoWriter {
    descriptor: OwnedFd,
}

impl BwrapInfoReader {
    pub(crate) async fn child_process_id(mut self) -> Result<u32> {
        let info = tokio::time::timeout(BWRAP_INFO_TIMEOUT, async {
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let read = self
                    .reader
                    .read(&mut buffer)
                    .await
                    .context("read packaged bubblewrap process information")?;
                anyhow::ensure!(
                    read != 0,
                    "packaged bubblewrap returned truncated process information"
                );
                bytes.extend_from_slice(&buffer[..read]);
                anyhow::ensure!(
                    bytes.len() <= MAX_BWRAP_INFO_SIZE,
                    "packaged bubblewrap process information exceeded {MAX_BWRAP_INFO_SIZE} bytes"
                );
                match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(info) => return Ok(info),
                    Err(error) if error.is_eof() => {}
                    Err(error) => {
                        return Err(anyhow::Error::new(error).context(
                            "packaged bubblewrap returned malformed process information",
                        ));
                    }
                }
            }
        })
        .await
        .context("packaged bubblewrap process information timed out")??;
        let process_id = info
            .as_object()
            .and_then(|info| info.get("child-pid"))
            .and_then(serde_json::Value::as_u64)
            .context("packaged bubblewrap process information omitted child-pid")?;
        let process_id = u32::try_from(process_id)
            .context("packaged bubblewrap child-pid exceeds the native process ID range")?;
        anyhow::ensure!(process_id != 0, "packaged bubblewrap returned child-pid 0");
        Ok(process_id)
    }
}

impl AsRawFd for BwrapInfoWriter {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.descriptor.as_raw_fd()
    }
}

pub(crate) fn bwrap_info_channel() -> io::Result<(BwrapInfoReader, BwrapInfoWriter)> {
    let mut descriptors = [-1_i32; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC) }
            == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok((
        BwrapInfoReader {
            reader: tokio::fs::File::from_std(reader.into()),
        },
        BwrapInfoWriter { descriptor: writer },
    ))
}
