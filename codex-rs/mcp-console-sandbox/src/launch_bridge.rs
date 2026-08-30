#![cfg(unix)]

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Read;
#[cfg(target_os = "macos")]
use std::io::Seek;
#[cfg(target_os = "macos")]
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tokio::io::AsyncReadExt;

const PRIVATE_SWITCH: &str = "--mcp-console-private-launch-bridge";
const MAX_STATUS_FRAME_SIZE: usize = 16 * 1024;
const STATUS_READY: u8 = 0;
const STATUS_EXEC_ERROR: u8 = 1;
const GATE_RELEASED: u8 = 1;
#[cfg(target_os = "macos")]
const TARGET_ARGUMENTS_MAGIC: &[u8; 4] = b"MCA1";
#[cfg(target_os = "macos")]
const MAX_TARGET_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;
#[cfg(target_os = "macos")]
const TARGET_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

pub struct PreparedTargetBridge {
    pub command: Vec<OsString>,
    pub status: LaunchStatus,
    pub writer: OwnedFd,
    pub gate: TargetStartGate,
    pub gate_reader: OwnedFd,
    pub canary: SandboxCanary,
    #[cfg(target_os = "macos")]
    pub target_arguments: std::fs::File,
}

pub struct SandboxCanary {
    file: std::fs::File,
    path: PathBuf,
}

pub struct LaunchStatus {
    reader: tokio::fs::File,
}

pub struct TargetCompletion {
    reader: tokio::fs::File,
}

pub struct TargetStartGate {
    writer: OwnedFd,
}

impl LaunchStatus {
    pub async fn wait_for_gate(mut self) -> Result<TargetCompletion> {
        let payload = read_status_frame(&mut self.reader, "target launch gate").await?;
        match payload.as_slice() {
            [STATUS_READY] => {}
            [STATUS_EXEC_ERROR, message @ ..] => bail!(
                "sandbox launch bridge rejected the target: {}",
                String::from_utf8_lossy(message)
            ),
            _ => bail!("sandbox launch bridge returned an invalid launch-gate status"),
        }
        Ok(TargetCompletion {
            reader: self.reader,
        })
    }
}

impl TargetCompletion {
    pub async fn wait(mut self) -> Result<Option<String>> {
        let Some(payload) =
            read_optional_status_frame(&mut self.reader, "target execution").await?
        else {
            return Ok(None);
        };
        match payload.as_slice() {
            [STATUS_EXEC_ERROR, message @ ..] => {
                Ok(Some(String::from_utf8_lossy(message).into_owned()))
            }
            _ => bail!("sandbox launch bridge returned an invalid target status"),
        }
    }
}

impl TargetStartGate {
    pub fn release(self) -> Result<()> {
        let mut writer = std::fs::File::from(self.writer);
        writer
            .write_all(&[GATE_RELEASED])
            .context("release sandbox target launch gate")
    }
}

impl SandboxCanary {
    fn create(state_directory: &Path) -> Result<Self> {
        let path = state_directory.join("mcp-console-sandbox-launch-canary");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("create sandbox launch canary {}", path.display()))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure sandbox launch canary {}", path.display()))?;
        set_inheritable(&file)?;
        Ok(Self { file, path })
    }
}

impl Drop for SandboxCanary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn prepare_target(
    executable: &Path,
    target: &[OsString],
    state_directory: &Path,
    require_seccomp: bool,
) -> Result<PreparedTargetBridge> {
    let (status_reader, status_writer) = pipe("target launch status")?;
    set_close_on_exec(&status_reader)?;
    let (gate_reader, gate_writer) = pipe("target launch gate")?;
    set_close_on_exec(&gate_writer)?;
    let canary = SandboxCanary::create(state_directory)?;
    let canary_metadata = canary
        .file
        .metadata()
        .context("inspect sandbox launch canary")?;
    #[cfg(target_os = "macos")]
    let target_arguments = create_target_arguments(state_directory, target)?;
    #[cfg(target_os = "macos")]
    let inherited_signal_dispositions = inherited_ignored_signals()?;
    #[cfg(target_os = "macos")]
    let command = std::iter::once(executable.as_os_str().to_os_string())
        .chain([
            OsString::from(PRIVATE_SWITCH),
            OsString::from(status_writer.as_raw_fd().to_string()),
            OsString::from(gate_reader.as_raw_fd().to_string()),
            OsString::from(canary.file.as_raw_fd().to_string()),
            canary.path.as_os_str().to_os_string(),
            OsString::from(canary_metadata.dev().to_string()),
            OsString::from(canary_metadata.ino().to_string()),
            OsString::from(if require_seccomp { "1" } else { "0" }),
            OsString::from(target_arguments.as_raw_fd().to_string()),
            OsString::from(inherited_signal_dispositions.to_string()),
        ])
        .collect();
    #[cfg(not(target_os = "macos"))]
    let command = std::iter::once(executable.as_os_str().to_os_string())
        .chain([
            OsString::from(PRIVATE_SWITCH),
            OsString::from(status_writer.as_raw_fd().to_string()),
            OsString::from(gate_reader.as_raw_fd().to_string()),
            OsString::from(canary.file.as_raw_fd().to_string()),
            canary.path.as_os_str().to_os_string(),
            OsString::from(canary_metadata.dev().to_string()),
            OsString::from(canary_metadata.ino().to_string()),
            OsString::from(if require_seccomp { "1" } else { "0" }),
            OsString::from("--"),
        ])
        .chain(target.iter().cloned())
        .collect();
    Ok(PreparedTargetBridge {
        command,
        status: LaunchStatus {
            reader: tokio::fs::File::from_std(status_reader.into()),
        },
        writer: status_writer,
        gate: TargetStartGate {
            writer: gate_writer,
        },
        gate_reader,
        canary,
        #[cfg(target_os = "macos")]
        target_arguments,
    })
}

pub fn dispatch_if_requested() -> bool {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(PRIVATE_SWITCH)) {
        return false;
    }
    if let Err(error) = dispatch(arguments) {
        eprintln!("mcp-console-sandbox launch bridge error: {error}");
        std::process::exit(125);
    }
    true
}

fn dispatch(mut arguments: impl Iterator<Item = OsString>) -> Result<()> {
    let status_fd = parse_descriptor(arguments.next(), "status")?;
    let gate_fd = parse_descriptor(arguments.next(), "gate")?;
    let canary_fd = parse_descriptor(arguments.next(), "canary")?;
    let canary_path = arguments
        .next()
        .map(PathBuf::from)
        .context("launch bridge canary path is missing")?;
    let canary_device = parse_u64(arguments.next(), "canary device")?;
    let canary_inode = parse_u64(arguments.next(), "canary inode")?;
    let require_seccomp = match arguments.next().as_deref() {
        Some(value) if value == "0" => false,
        Some(value) if value == "1" => true,
        _ => bail!("launch bridge seccomp requirement is invalid"),
    };
    #[cfg(target_os = "macos")]
    let target_arguments_fd = parse_descriptor(arguments.next(), "target arguments")?;
    #[cfg(target_os = "macos")]
    let inherited_signal_dispositions = parse_u64(arguments.next(), "signal dispositions")?;
    #[cfg(target_os = "macos")]
    let target = {
        anyhow::ensure!(
            arguments.next().is_none(),
            "launch bridge received unexpected target arguments"
        );
        read_target_arguments(target_arguments_fd)?
    };
    #[cfg(not(target_os = "macos"))]
    let target = {
        if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
            bail!("launch bridge target separator is missing")
        }
        arguments.collect()
    };
    run(
        status_fd,
        gate_fd,
        canary_fd,
        &canary_path,
        canary_device,
        canary_inode,
        require_seccomp,
        #[cfg(target_os = "macos")]
        inherited_signal_dispositions,
        target,
    )
}

fn parse_descriptor(argument: Option<OsString>, name: &str) -> Result<i32> {
    let descriptor = argument
        .and_then(|argument| argument.into_string().ok())
        .with_context(|| format!("launch bridge {name} descriptor is missing"))?
        .parse()
        .with_context(|| format!("launch bridge {name} descriptor is invalid"))?;
    anyhow::ensure!(
        descriptor > libc::STDERR_FILENO,
        "launch bridge {name} descriptor must not use a standard descriptor"
    );
    Ok(descriptor)
}

fn parse_u64(argument: Option<OsString>, name: &str) -> Result<u64> {
    argument
        .and_then(|argument| argument.into_string().ok())
        .with_context(|| format!("launch bridge {name} is missing"))?
        .parse()
        .with_context(|| format!("launch bridge {name} is invalid"))
}

#[allow(clippy::too_many_arguments)]
fn run(
    status_fd: i32,
    gate_fd: i32,
    canary_fd: i32,
    canary_path: &Path,
    canary_device: u64,
    canary_inode: u64,
    require_seccomp: bool,
    #[cfg(target_os = "macos")] inherited_signal_dispositions: u64,
    target: Vec<OsString>,
) -> Result<()> {
    let (program, arguments) = target
        .split_first()
        .context("launch bridge target is missing")?;
    let mut status = unsafe { std::fs::File::from_raw_fd(status_fd) };
    set_close_on_exec(&status)?;
    let mut gate = unsafe { std::fs::File::from_raw_fd(gate_fd) };
    let canary = unsafe { std::fs::File::from_raw_fd(canary_fd) };
    set_close_on_exec(&canary)?;
    if let Err(error) = verify_sandbox(
        &canary,
        canary_path,
        canary_device,
        canary_inode,
        require_seccomp,
    ) {
        let _ = write_status(&mut status, STATUS_EXEC_ERROR, error.to_string().as_bytes());
        return Err(error);
    }
    drop(canary);
    ignore_supervision_signals()?;
    write_status(&mut status, STATUS_READY, &[])?;
    let mut release = [0_u8; 1];
    if let Err(error) = gate.read_exact(&mut release) {
        bail!("target launch gate closed before release: {error}")
    }
    if release != [GATE_RELEASED] {
        bail!("target launch gate returned an invalid release value")
    }
    drop(gate);
    #[cfg(target_os = "macos")]
    restore_target_signals(inherited_signal_dispositions)?;
    #[cfg(not(target_os = "macos"))]
    reset_target_signals()?;
    let error = Command::new(program).args(arguments).exec();
    let _ = write_status(&mut status, STATUS_EXEC_ERROR, error.to_string().as_bytes());
    Err(error).context("execute sandbox target")
}

fn verify_sandbox(
    canary: &std::fs::File,
    canary_path: &Path,
    canary_device: u64,
    canary_inode: u64,
    require_seccomp: bool,
) -> Result<()> {
    anyhow::ensure!(
        canary_path.is_absolute(),
        "sandbox launch canary path must be absolute"
    );
    let metadata = canary.metadata().context("inspect sandbox launch canary")?;
    anyhow::ensure!(
        metadata.is_file() && metadata.dev() == canary_device && metadata.ino() == canary_inode,
        "sandbox launch canary descriptor does not identify the expected file"
    );
    #[cfg(target_os = "macos")]
    verify_macos_seatbelt()?;
    #[cfg(target_os = "linux")]
    verify_linux_sandbox(require_seccomp)?;
    #[cfg(not(target_os = "linux"))]
    let _ = require_seccomp;
    match std::fs::File::open(canary_path) {
        Ok(_) => bail!("sandbox launch canary remained host-readable"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) || error.raw_os_error() == Some(libc::EPERM) =>
        {
            Ok(())
        }
        Err(error) => Err(error).context("verify sandbox launch canary denial"),
    }
}

#[cfg(target_os = "macos")]
fn create_target_arguments(state_directory: &Path, target: &[OsString]) -> Result<std::fs::File> {
    let template = state_directory.join("mcp-console-target-arguments-XXXXXX");
    let mut template = std::ffi::CString::new(template.as_os_str().as_bytes())
        .context("target argument file path contains a null byte")?
        .into_bytes_with_nul();
    let descriptor = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
    if descriptor == -1 {
        return Err(std::io::Error::last_os_error()).context("create target argument file");
    }
    let path = unsafe { std::ffi::CStr::from_ptr(template.as_ptr().cast()) };
    let unlink_result = unsafe { libc::unlink(path.as_ptr()) };
    let mut file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    if unlink_result == -1 {
        return Err(std::io::Error::last_os_error()).context("unlink target argument file");
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .context("secure target argument file")?;
    let count = u32::try_from(target.len()).context("too many target arguments")?;
    file.write_all(TARGET_ARGUMENTS_MAGIC)?;
    file.write_all(&count.to_be_bytes())?;
    let mut total = 8_usize;
    for argument in target {
        let bytes = argument.as_bytes();
        let length = u32::try_from(bytes.len()).context("target argument is too long")?;
        total = total
            .checked_add(4 + bytes.len())
            .context("target arguments exceed their size bound")?;
        anyhow::ensure!(
            total <= MAX_TARGET_ARGUMENT_BYTES,
            "target arguments exceed their size bound"
        );
        file.write_all(&length.to_be_bytes())?;
        file.write_all(bytes)?;
    }
    file.seek(SeekFrom::Start(0))?;
    set_inheritable(&file)?;
    Ok(file)
}

#[cfg(target_os = "macos")]
fn read_target_arguments(descriptor: i32) -> Result<Vec<OsString>> {
    let mut file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    set_close_on_exec(&file)?;
    let length =
        usize::try_from(file.metadata()?.len()).context("target argument file is too large")?;
    anyhow::ensure!(
        length <= MAX_TARGET_ARGUMENT_BYTES,
        "target argument file exceeds its size bound"
    );
    let mut bytes = Vec::with_capacity(length);
    file.read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() >= 8, "target argument file is truncated");
    anyhow::ensure!(
        &bytes[..4] == TARGET_ARGUMENTS_MAGIC,
        "target argument file has an invalid version"
    );
    let count = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let mut cursor = 8_usize;
    let mut target = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let end = cursor
            .checked_add(4)
            .filter(|end| *end <= bytes.len())
            .context("target argument file is truncated")?;
        let length = u32::from_be_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]) as usize;
        cursor = end;
        let end = cursor
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .context("target argument file is truncated")?;
        anyhow::ensure!(
            !bytes[cursor..end].contains(&0),
            "target argument contains a null byte"
        );
        target.push(OsString::from_vec(bytes[cursor..end].to_vec()));
        cursor = end;
    }
    anyhow::ensure!(
        cursor == bytes.len(),
        "target argument file has trailing bytes"
    );
    Ok(target)
}

#[cfg(target_os = "macos")]
fn inherited_ignored_signals() -> Result<u64> {
    let mut ignored = 0_u64;
    for (index, signal) in TARGET_SIGNALS.into_iter().enumerate() {
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
        if unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error())
                .context("inspect target signal disposition");
        }
        if unsafe { action.assume_init() }.sa_sigaction == libc::SIG_IGN {
            ignored |= 1 << index;
        }
    }
    Ok(ignored)
}

#[cfg(target_os = "macos")]
fn restore_target_signals(ignored: u64) -> std::io::Result<()> {
    if ignored >= (1 << TARGET_SIGNALS.len()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid target signal dispositions",
        ));
    }
    for (index, signal) in TARGET_SIGNALS.into_iter().enumerate() {
        let disposition = if ignored & (1 << index) == 0 {
            libc::SIG_DFL
        } else {
            libc::SIG_IGN
        };
        if unsafe { libc::signal(signal, disposition) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_linux_sandbox(require_seccomp: bool) -> Result<()> {
    anyhow::ensure!(
        unsafe { libc::getppid() } == 1,
        "launch bridge is outside the bubblewrap PID namespace"
    );
    let no_new_privileges = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS) };
    anyhow::ensure!(
        no_new_privileges == 1,
        "launch bridge is missing Linux no_new_privs confinement"
    );
    if require_seccomp {
        let seccomp = unsafe { libc::prctl(libc::PR_GET_SECCOMP) };
        anyhow::ensure!(
            seccomp == libc::SECCOMP_MODE_FILTER as libc::c_int,
            "launch bridge is missing the required Linux seccomp filter"
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_macos_seatbelt() -> Result<()> {
    use std::ffi::c_char;

    const SANDBOX_NAMED: u64 = 1;
    type SandboxInit =
        unsafe extern "C" fn(*const c_char, u64, *mut *mut c_char) -> std::ffi::c_int;
    type SandboxFreeError = unsafe extern "C" fn(*mut c_char);

    let sandbox_init = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"sandbox_init".as_ptr()) };
    anyhow::ensure!(
        !sandbox_init.is_null(),
        "resolve the macOS Seatbelt probe function"
    );
    let sandbox_init =
        unsafe { std::mem::transmute::<*mut libc::c_void, SandboxInit>(sandbox_init) };
    let sandbox_free_error =
        unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"sandbox_free_error".as_ptr()) };
    anyhow::ensure!(
        !sandbox_free_error.is_null(),
        "resolve the macOS Seatbelt error function"
    );
    let sandbox_free_error =
        unsafe { std::mem::transmute::<*mut libc::c_void, SandboxFreeError>(sandbox_free_error) };

    let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
    anyhow::ensure!(saved_stderr != -1, "duplicate launch bridge stderr");
    let saved_stderr = unsafe { OwnedFd::from_raw_fd(saved_stderr) };
    let null = OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .context("open null device for Seatbelt probe")?;
    anyhow::ensure!(
        unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) } != -1,
        "suppress Seatbelt probe diagnostics"
    );
    let mut error = std::ptr::null_mut();
    let result = unsafe { sandbox_init(c"pure-computation".as_ptr(), SANDBOX_NAMED, &mut error) };
    let raw_error = std::io::Error::last_os_error().raw_os_error();
    anyhow::ensure!(
        unsafe { libc::dup2(saved_stderr.as_raw_fd(), libc::STDERR_FILENO) } != -1,
        "restore launch bridge stderr"
    );
    if !error.is_null() {
        unsafe { sandbox_free_error(error) };
    }
    anyhow::ensure!(
        result == -1 && raw_error == Some(libc::EPERM),
        "launch bridge is outside the expected Seatbelt sandbox"
    );
    Ok(())
}

async fn read_status_frame(reader: &mut tokio::fs::File, phase: &str) -> Result<Vec<u8>> {
    read_optional_status_frame(reader, phase)
        .await?
        .with_context(|| format!("sandbox launch bridge closed before reporting {phase}"))
}

async fn read_optional_status_frame(
    reader: &mut tokio::fs::File,
    phase: &str,
) -> Result<Option<Vec<u8>>> {
    let mut length = [0_u8; 4];
    match reader.read(&mut length[..1]).await {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("one-byte read returned more than one byte"),
        Err(error) => return Err(error).with_context(|| format!("read {phase} status")),
    }
    reader
        .read_exact(&mut length[1..])
        .await
        .with_context(|| format!("truncated {phase} status length"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_STATUS_FRAME_SIZE {
        bail!("{phase} status exceeded {MAX_STATUS_FRAME_SIZE} bytes")
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .await
        .with_context(|| format!("truncated {phase} status payload"))?;
    Ok(Some(payload))
}

fn pipe(name: &str) -> Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1_i32; 2];
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("create {name} pipe"));
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn write_status(status: &mut std::fs::File, kind: u8, detail: &[u8]) -> Result<()> {
    let detail = &detail[..detail.len().min(MAX_STATUS_FRAME_SIZE - 1)];
    let length = u32::try_from(detail.len() + 1).context("launch status length overflow")?;
    status.write_all(&length.to_be_bytes())?;
    status.write_all(&[kind])?;
    status.write_all(detail)?;
    status.flush()?;
    Ok(())
}

fn set_close_on_exec(descriptor: &impl AsRawFd) -> Result<()> {
    let descriptor = descriptor.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
    {
        return Err(std::io::Error::last_os_error()).context("secure private launch descriptor");
    }
    Ok(())
}

fn set_inheritable(descriptor: &impl AsRawFd) -> Result<()> {
    let descriptor = descriptor.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1
    {
        return Err(std::io::Error::last_os_error()).context("inherit private launch descriptor");
    }
    Ok(())
}

fn ignore_supervision_signals() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, libc::SIG_IGN) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error())
                .context("configure launch bridge signal handling");
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn reset_target_signals() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
