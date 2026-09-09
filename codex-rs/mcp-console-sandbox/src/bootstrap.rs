use crate::codex::AbsolutePathBuf;
use crate::codex::NetworkSandboxPolicy;
use crate::codex::RawFileSystemSandboxPolicy;
use crate::codex::RemoteNetworkProxyConfig;
use crate::config::Lifecycle;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;

const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    #[serde(skip)]
    pub excluded_environment: Option<String>,
    pub version: u32,
    pub command: Vec<String>,
    pub cwd: AbsolutePathBuf,
    pub environment: HashMap<String, String>,
    pub filesystem: RawFileSystemSandboxPolicy,
    pub network: NetworkSandboxPolicy,
    pub proxy: Option<RemoteNetworkProxyConfig>,
    pub macos_seatbelt_profile_extension: Option<String>,
    #[serde(default)]
    pub lifecycle: Lifecycle,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentConfiguration {
    version: u32,
    filesystem: RawFileSystemSandboxPolicy,
    network: NetworkSandboxPolicy,
    proxy: Option<RemoteNetworkProxyConfig>,
    macos_seatbelt_profile_extension: Option<String>,
    #[serde(default)]
    lifecycle: Lifecycle,
}

pub enum Input {
    Descriptor(File),
    Environment(Box<Bootstrap>),
}

pub fn take_input() -> Result<Input> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--config-env") {
        ensure!(
            args.len() >= 4 && args[2] == "--",
            "expected --config-env NAME -- command [args...]"
        );
        let name = args[1]
            .to_str()
            .context("configuration variable name must be UTF-8")?;
        ensure!(
            !name.is_empty() && !name.contains('='),
            "invalid configuration variable name"
        );
        let payload =
            std::env::var(name).context("read selected configuration environment variable")?;
        ensure!(
            payload.len() <= MAX_PAYLOAD_BYTES,
            "configuration exceeds 1048576 bytes"
        );
        let config: EnvironmentConfiguration =
            serde_json::from_str(&payload).context("invalid configuration JSON")?;
        let request = Bootstrap {
            excluded_environment: Some(name.to_owned()),
            version: config.version,
            command: args[3..]
                .iter()
                .map(|arg| {
                    arg.to_str()
                        .map(str::to_owned)
                        .context("command must be UTF-8")
                })
                .collect::<Result<_>>()?,
            cwd: AbsolutePathBuf::try_from(std::env::current_dir()?)?,
            environment: std::env::vars_os()
                .filter(|(key, _)| key != name)
                .map(|(key, value)| {
                    Ok((
                        key.into_string()
                            .map_err(|_| anyhow::anyhow!("environment key must be UTF-8"))?,
                        value
                            .into_string()
                            .map_err(|_| anyhow::anyhow!("environment value must be UTF-8"))?,
                    ))
                })
                .collect::<Result<_>>()?,
            filesystem: config.filesystem,
            network: config.network,
            proxy: config.proxy,
            macos_seatbelt_profile_extension: config.macos_seatbelt_profile_extension,
            lifecycle: config.lifecycle,
        };
        validate(&request)?;
        ensure!(
            request
                .lifecycle
                .private_tmp
                .as_ref()
                .is_none_or(|tmp| !tmp.environment.iter().any(|key| key == name)),
            "configuration transport variable cannot be exported to the target"
        );
        return Ok(Input::Environment(Box::new(request)));
    }
    take_inherited().map(Input::Descriptor)
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

pub fn read(mut bootstrap: File, signals: &crate::signals::Signals) -> Result<Bootstrap> {
    // Consume only the declared frame, without waiting for EOF.
    let mut header = [0; 4];
    read_cancellable(&mut bootstrap, &mut header, signals).context("read bootstrap length")?;
    let size = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_PAYLOAD_BYTES).contains(&size),
        "bootstrap length must be 1..=1048576 bytes"
    );
    let mut payload = vec![0; size];
    read_cancellable(&mut bootstrap, &mut payload, signals).context("read bootstrap payload")?;
    let request: Bootstrap = serde_json::from_slice(&payload).context("invalid bootstrap JSON")?;
    validate(&request)?;
    drop(bootstrap);
    Ok(request)
}

fn validate(request: &Bootstrap) -> Result<()> {
    request.lifecycle.validate()?;
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
    Ok(())
}

fn read_cancellable(
    file: &mut File,
    mut bytes: &mut [u8],
    signals: &crate::signals::Signals,
) -> Result<()> {
    while !bytes.is_empty() {
        ensure!(signals.pending()?.is_empty(), "startup cancelled by signal");
        let mut descriptor = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let count = unsafe { libc::poll(&mut descriptor, 1, 10) };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count == 0 {
            continue;
        }
        let count = file.read(bytes)?;
        ensure!(count != 0, "incomplete bootstrap frame");
        bytes = &mut bytes[count..];
    }
    Ok(())
}
