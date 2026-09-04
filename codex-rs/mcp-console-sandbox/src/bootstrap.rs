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
}

pub fn read() -> Result<(Bootstrap, File)> {
    // SAFETY: this executable takes ownership of fd 0 exactly once. File has no
    // read-ahead buffer; read_exact requests only the bytes still in each slice.
    // The same open file description is then transferred to the child as stdin.
    let mut stdin = unsafe { File::from_raw_fd(libc::STDIN_FILENO) };
    let mut header = [0; 4];
    stdin
        .read_exact(&mut header)
        .context("read bootstrap length")?;
    let size = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_PAYLOAD_BYTES).contains(&size),
        "bootstrap length must be 1..=1048576 bytes"
    );
    let mut payload = vec![0; size];
    stdin
        .read_exact(&mut payload)
        .context("read bootstrap payload")?;
    let request: Bootstrap = serde_json::from_slice(&payload).context("invalid bootstrap JSON")?;
    ensure!(
        request.version == 1,
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
    Ok((request, stdin))
}
