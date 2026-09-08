use crate::codex::AbsolutePathBuf;
use crate::codex::NetworkSandboxPolicy;
use crate::codex::RawFileSystemSandboxPolicy;
use crate::codex::RemoteNetworkProxyConfig;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;

const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub version: u32,
    pub command: Vec<String>,
    pub cwd: AbsolutePathBuf,
    pub environment: HashMap<String, String>,
    pub filesystem: RawFileSystemSandboxPolicy,
    pub network: NetworkSandboxPolicy,
    pub proxy: Option<RemoteNetworkProxyConfig>,
    pub macos_seatbelt_profile_extension: Option<String>,
}

pub fn take_inherited() -> Result<File> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2 && args[0] == "--bootstrap-fd",
        "expected --bootstrap-fd <N>"
    );
    let number = args[1].to_str().context("bootstrap fd must be decimal")?;
    ensure!(
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()),
        "bootstrap fd must be decimal"
    );
    let fd: libc::c_int = number.parse().context("bootstrap fd is out of range")?;
    ensure!(
        fd > libc::STDERR_FILENO,
        "bootstrap fd must be greater than 2"
    );
    // Validate before opening any files or creating the runtime: a closed
    // caller fd must not become valid through reuse by our own setup.
    // SAFETY: F_GETFL only queries this process's descriptor; it does not adopt it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("invalid bootstrap fd");
    }
    ensure!(
        flags & libc::O_ACCMODE != libc::O_WRONLY,
        "bootstrap fd must be readable"
    );
    #[cfg(target_os = "linux")]
    ensure!(flags & libc::O_PATH == 0, "bootstrap fd must be readable");
    // SAFETY: fd is an open inherited descriptor above stdio, validated before
    // any other descriptor allocation. This is its sole adoption; File owns it
    // on both success and error paths. No other code closes or adopts this fd.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn read(mut bootstrap: File) -> Result<Bootstrap> {
    // File/read_exact consumes only the declared frame, without waiting for EOF.
    let mut header = [0; 4];
    bootstrap
        .read_exact(&mut header)
        .context("read bootstrap length")?;
    let size = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_PAYLOAD_BYTES).contains(&size),
        "bootstrap length must be 1..=1048576 bytes"
    );
    let mut payload = vec![0; size];
    bootstrap
        .read_exact(&mut payload)
        .context("read bootstrap payload")?;
    let request: Bootstrap = serde_json::from_slice(&payload).context("invalid bootstrap JSON")?;
    ensure!(
        request.version == 2,
        "unsupported bootstrap version {}",
        request.version
    );
    ensure!(
        request
            .command
            .first()
            .is_some_and(|program| !program.is_empty()),
        "command must contain a program"
    );
    // The one-shot gate is closed before proxy, runtime, or native setup.
    drop(bootstrap);
    Ok(request)
}
