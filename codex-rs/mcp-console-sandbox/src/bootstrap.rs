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
const RESERVED_CONFIGURATION: &str = "MCP_CONSOLE_SANDBOX_CONFIG";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    #[serde(skip)]
    pub excluded_environment: Vec<String>,
    pub version: u32,
    pub command: Vec<String>,
    pub cwd: AbsolutePathBuf,
    pub environment: HashMap<String, String>,
    #[serde(default, deserialize_with = "supplied")]
    pub filesystem: Option<RawFileSystemSandboxPolicy>,
    #[serde(default, deserialize_with = "supplied")]
    pub network: Option<NetworkSandboxPolicy>,
    pub extends: Option<String>,
    pub workspace: Option<AbsolutePathBuf>,
    pub workspace_options: Option<crate::profiles::WorkspaceOptions>,
    pub proxy: Option<RemoteNetworkProxyConfig>,
    pub macos_seatbelt_profile_extension: Option<String>,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    pub linux_backend: Option<crate::config::LinuxBackend>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentConfiguration {
    version: u32,
    #[serde(default, deserialize_with = "supplied")]
    filesystem: Option<RawFileSystemSandboxPolicy>,
    #[serde(default, deserialize_with = "supplied")]
    network: Option<NetworkSandboxPolicy>,
    extends: Option<String>,
    workspace: Option<AbsolutePathBuf>,
    workspace_options: Option<crate::profiles::WorkspaceOptions>,
    proxy: Option<RemoteNetworkProxyConfig>,
    macos_seatbelt_profile_extension: Option<String>,
    #[serde(default)]
    lifecycle: Lifecycle,
    linux_backend: Option<crate::config::LinuxBackend>,
    #[serde(default = "inherit_environment")]
    inherit_environment: bool,
    #[serde(default)]
    environment: HashMap<String, String>,
}

// Omitted policy fields can inherit a built-in; explicit null is not a native policy.
fn supplied<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn inherit_environment() -> bool {
    true
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
        let payload = std::env::var(name).map_err(|error| match error {
            std::env::VarError::NotPresent => {
                anyhow::anyhow!("selected configuration variable is not set")
            }
            std::env::VarError::NotUnicode(_) => {
                anyhow::anyhow!("selected configuration value must be UTF-8")
            }
        })?;
        ensure!(
            payload.len() <= MAX_PAYLOAD_BYTES,
            "configuration exceeds 1048576 bytes"
        );
        let config: EnvironmentConfiguration = parse_json(payload.as_bytes())?;
        let mut environment = if config.inherit_environment {
            std::env::vars_os()
                .filter(|(key, _)| key != name && key != RESERVED_CONFIGURATION)
                .map(|(key, value)| {
                    Ok((
                        key.into_string()
                            .map_err(|_| anyhow::anyhow!("environment key must be UTF-8"))?,
                        value
                            .into_string()
                            .map_err(|_| anyhow::anyhow!("environment value must be UTF-8"))?,
                    ))
                })
                .collect::<Result<HashMap<_, _>>>()?
        } else {
            HashMap::new()
        };
        environment.extend(config.environment);
        let mut request = Bootstrap {
            excluded_environment: vec![name.to_owned()],
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
            environment,
            filesystem: config.filesystem,
            network: config.network,
            extends: config.extends,
            workspace: config.workspace,
            workspace_options: config.workspace_options,
            proxy: config.proxy,
            macos_seatbelt_profile_extension: config.macos_seatbelt_profile_extension,
            lifecycle: config.lifecycle,
            linux_backend: config.linux_backend,
        };
        validate(&mut request)?;
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
    let mut request: Bootstrap = parse_json(&payload)?;
    validate(&mut request)?;
    drop(bootstrap);
    Ok(request)
}

fn parse_json<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T> {
    // Serde's data errors may quote an entire invalid value, including a target
    // environment map. Report location and category without echoing input.
    serde_json::from_slice(payload).map_err(|error| {
        anyhow::anyhow!(
            "invalid configuration JSON ({:?} at line {}, column {})",
            error.classify(),
            error.line(),
            error.column()
        )
    })
}

fn validate(request: &mut Bootstrap) -> Result<()> {
    request.lifecycle.validate()?;
    ensure!(
        cfg!(target_os = "linux") || request.linux_backend.is_none(),
        "linux_backend is supported only on Linux"
    );
    if request.linux_backend == Some(crate::config::LinuxBackend::Landlock) {
        ensure!(
            request.proxy.is_none(),
            "landlock does not support managed proxy routing"
        );
        ensure!(
            request.lifecycle.parent_pid.is_none()
                && request.lifecycle.private_tmp.is_none()
                && request.lifecycle.cleanup_timeout_ms.is_none()
                && request.lifecycle.sigterm == crate::config::Sigterm::Forward,
            "landlock does not provide supervised lifetime; omit lifecycle options"
        );
    }
    request
        .excluded_environment
        .push(RESERVED_CONFIGURATION.to_owned());
    for name in &request.excluded_environment {
        request.environment.remove(name);
        ensure!(
            request
                .lifecycle
                .private_tmp
                .as_ref()
                .is_none_or(|tmp| !tmp.environment.contains(name)),
            "configuration transport variable cannot be exported to the target"
        );
    }
    for (name, value) in &request.environment {
        ensure!(
            !name.is_empty() && !name.contains(['=', '\0']),
            "invalid target environment name"
        );
        ensure!(
            !value.contains('\0'),
            "target environment value contains NUL"
        );
    }
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
    #[cfg(target_os = "linux")]
    let notification = signals.notification()?;
    while !bytes.is_empty() {
        ensure!(signals.pending()?.is_empty(), "startup cancelled by signal");
        let mut descriptors = [
            libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                #[cfg(target_os = "linux")]
                fd: notification.as_raw_fd(),
                #[cfg(target_os = "macos")]
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = if cfg!(target_os = "linux") { -1 } else { 10 };
        let count = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout) };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count == 0 || descriptors[0].revents == 0 {
            continue;
        }
        let count = file.read(bytes)?;
        ensure!(count != 0, "incomplete bootstrap frame");
        bytes = &mut bytes[count..];
    }
    Ok(())
}
