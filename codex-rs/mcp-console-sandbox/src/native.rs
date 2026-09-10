//! One-shot execution boundary; only native init retains the private control channel.
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::io::Write;
#[cfg(target_os = "macos")]
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

#[derive(Serialize, Deserialize)]
pub struct TargetSetup {
    pub signals: crate::signals::SignalState,
    pub environment: HashMap<String, String>,
    pub proxy_environment: Vec<String>,
    pub excluded_environment: Vec<String>,
    pub command: Vec<String>,
    pub seatbelt: Option<Seatbelt>,
}

#[derive(Serialize, Deserialize)]
pub struct Seatbelt {
    pub policy: String,
    pub parameters: Vec<(String, String)>,
}

fn accept(descriptor: OwnedFd) -> Result<(TargetSetup, File)> {
    // The upstream restricted filter allows descriptor read/write, but denies
    // the sendto syscall used by UnixStream::write on Linux.
    let mut stream = File::from(descriptor);
    // Linux SO_PASSCRED supplies the namespace init's host PID to the supervisor.
    stream.write_all(&[1])?;
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(
        (1..=4 * 1024 * 1024).contains(&length),
        "invalid native setup size"
    );
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    let setup = serde_json::from_slice(&payload).context("native setup JSON")?;
    Ok((setup, stream))
}

#[cfg(target_os = "linux")]
pub fn linux_target_setup(
    command: &mut Command,
    descriptor: OwnedFd,
    mode: codex_linux_sandbox::TargetSetupMode,
) -> std::io::Result<Option<OwnedFd>> {
    let (setup, channel): (TargetSetup, Option<File>) = match mode {
        codex_linux_sandbox::TargetSetupMode::Namespace => {
            // Re-arm after bubblewrap's credential/exec boundary, before readiness.
            if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let (setup, channel) = match accept(descriptor) {
                Ok(value) => value,
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
                        )
                    }) =>
                {
                    std::process::exit(0)
                }
                Err(error) => return Err(std::io::Error::other(error)),
            };
            use std::os::fd::AsRawFd;
            if unsafe { libc::fcntl(channel.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            (setup, Some(channel))
        }
        codex_linux_sandbox::TargetSetupMode::Direct => {
            // Only the trusted pre-exec stages can access the sealed setup file.
            // There is no host supervisor or control endpoint in direct mode.
            (
                serde_json::from_reader(File::from(descriptor)).map_err(std::io::Error::other)?,
                None,
            )
        }
    };
    // Install the target environment only after enforcement and helper setup.
    // Only managed proxy values rewritten in the namespace cross this boundary.
    command.env_clear().envs(&setup.environment);
    if !setup.environment.contains_key("PWD") {
        command.env("PWD", std::env::current_dir()?);
    }
    for name in setup.proxy_environment {
        let value = std::env::var_os(&name)
            .ok_or_else(|| std::io::Error::other("missing native proxy environment"))?;
        command.env(name, value);
    }
    for name in setup.excluded_environment {
        command.env_remove(name);
    }
    unsafe {
        command.pre_exec(move || setup.signals.restore());
    }
    Ok(channel.map(Into::into))
}

#[cfg(target_os = "macos")]
pub fn macos_main() -> Result<()> {
    let fd: i32 = std::env::args()
        .nth(2)
        .context("native setup fd")?
        .parse()?;
    anyhow::ensure!(fd > 2, "native setup fd must be private");
    let (setup, channel) = accept(unsafe { OwnedFd::from_raw_fd(fd) })?;
    drop(channel);
    let profile = setup.seatbelt.context("native Seatbelt profile")?;
    apply_seatbelt(profile)?;
    let mut command = Command::new(&setup.command[0]);
    command
        .args(&setup.command[1..])
        .env_clear()
        .envs(setup.environment);
    unsafe {
        command.pre_exec(move || setup.signals.restore());
    }
    Err(command.exec()).context("exec sandbox target")
}

#[cfg(target_os = "macos")]
fn apply_seatbelt(profile: Seatbelt) -> Result<()> {
    use std::ffi::CStr;
    use std::ffi::CString;
    unsafe extern "C" {
        fn sandbox_init_with_parameters(
            profile: *const libc::c_char,
            flags: u64,
            parameters: *const *const libc::c_char,
            errorbuf: *mut *mut libc::c_char,
        ) -> libc::c_int;
        fn sandbox_free_error(errorbuf: *mut libc::c_char);
    }
    let policy = CString::new(profile.policy)?;
    let parameters: Vec<_> = profile
        .parameters
        .into_iter()
        .flat_map(|(key, value)| [key, value])
        .map(CString::new)
        .collect::<std::result::Result<_, _>>()?;
    let mut pointers: Vec<_> = parameters.iter().map(|value| value.as_ptr()).collect();
    pointers.push(std::ptr::null());
    let mut error = std::ptr::null_mut();
    if unsafe {
        sandbox_init_with_parameters(
            policy.as_ptr(),
            /*flags*/ 0,
            pointers.as_ptr(),
            &mut error,
        )
    } != 0
    {
        let message = if error.is_null() {
            std::io::Error::last_os_error().to_string()
        } else {
            let message = unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            unsafe {
                sandbox_free_error(error);
            }
            message
        };
        anyhow::bail!("apply native Seatbelt profile: {message}");
    }
    Ok(())
}

pub struct Gate {
    stream: UnixStream,
    payload: Vec<u8>,
    written: usize,
    pub target: Option<crate::platform::Target>,
}

impl Gate {
    pub fn new(stream: UnixStream, setup: &TargetSetup) -> Result<Self> {
        let payload = serde_json::to_vec(setup)?;
        anyhow::ensure!(
            payload.len() <= 4 * 1024 * 1024,
            "native setup exceeds 4 MiB"
        );
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend(payload);
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            payload: frame,
            written: 0,
            target: None,
        })
    }

    #[cfg(target_os = "linux")]
    pub async fn changed(&self) -> std::io::Result<()> {
        use std::os::fd::AsFd;
        let notification = tokio::io::unix::AsyncFd::new(self.stream.as_fd())?;
        let _ready = if self.target.is_none() {
            notification.readable().await?
        } else {
            notification.writable().await?
        };
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn retire(&mut self, root: i32) -> Result<bool> {
        if self.target.is_none() {
            self.target = match crate::platform::receive_ready(&mut self.stream, root) {
                Ok(target) => target,
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(true),
                Err(error) => return Err(error.into()),
            };
        }
        if let Some(target) = &self.target {
            crate::platform::forward(target, libc::SIGKILL)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn advance(&mut self, root: i32) -> Result<bool> {
        if self.target.is_none() {
            self.target = crate::platform::receive_ready(&mut self.stream, root)?;
            // Readiness may race cancellation or caller death. Return to the
            // supervisor's cancellation check before sending any release data.
            return Ok(false);
        }
        match self.stream.write(&self.payload[self.written..]) {
            Ok(0) => anyhow::bail!("native gate closed before request acceptance"),
            Ok(count) => self.written += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        Ok(self.written == self.payload.len())
    }
}
