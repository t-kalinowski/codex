#![cfg(unix)]

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use tokio::io::AsyncReadExt;

const PRIVATE_SWITCH: &str = "--mcp-console-private-launch-bridge";
const PRIVATE_TARGET_SWITCH: &str = "--mcp-console-private-target-launch";
const MAX_STATUS_FRAME_SIZE: usize = 16 * 1024;
const STATUS_STARTED: u8 = 0;
const STATUS_START_ERROR: u8 = 1;
const STATUS_TARGET_EXITED: u8 = 2;
const STATUS_TARGET_SIGNALED: u8 = 3;
const STATUS_INFRASTRUCTURE_ERROR: u8 = 4;
const STATUS_WAITING_FOR_GATE: u8 = 5;
const STATUS_COMMIT_OK: u8 = 6;
const STATUS_COMMIT_ERROR: u8 = 7;
const TARGET_HELPER_READY: u8 = 8;
const TARGET_HELPER_ERROR: u8 = 9;
const GATE_RELEASED: u8 = 1;
const TARGET_COMMITTED: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchBridgeMode {
    Direct,
    NamespacePid1,
}

impl LaunchBridgeMode {
    fn argument(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::NamespacePid1 => "namespace-pid-1",
        }
    }
}

pub struct PreparedTargetBridge {
    pub command: Vec<OsString>,
    pub status: LaunchStatus,
    pub writer: OwnedFd,
    pub commit_status: LaunchCommitStatus,
    pub commit_status_writer: OwnedFd,
    pub gate: TargetStartGate,
    pub gate_reader: OwnedFd,
}

pub struct LaunchStatus {
    reader: tokio::fs::File,
}

pub struct GatedLaunchStatus {
    reader: tokio::fs::File,
}

pub struct LaunchCommitStatus {
    reader: tokio::fs::File,
}

pub struct TargetStartGate {
    writer: OwnedFd,
}

pub struct ReleasedTargetStartGate {
    writer: OwnedFd,
}

pub struct ConfirmedTarget {
    pub process_id: u32,
}

pub struct TargetCompletion {
    reader: tokio::fs::File,
}

pub enum ReportedTargetOutcome {
    Exited(i32),
    Signaled(i32),
    InfrastructureError(String),
}

impl LaunchStatus {
    pub async fn wait_for_gate(mut self) -> Result<GatedLaunchStatus> {
        let payload = read_status_frame(&mut self.reader, "target launch gate").await?;
        anyhow::ensure!(
            payload == [STATUS_WAITING_FOR_GATE],
            "sandbox launch bridge returned an invalid launch-gate status"
        );
        Ok(GatedLaunchStatus {
            reader: self.reader,
        })
    }
}

impl GatedLaunchStatus {
    pub async fn confirm(mut self) -> (Result<ConfirmedTarget>, TargetCompletion) {
        let payload = read_status_frame(&mut self.reader, "target startup").await;
        let confirmation = match payload {
            Ok(payload) => match payload.as_slice() {
                [STATUS_STARTED, first, second, third, fourth] => Ok(ConfirmedTarget {
                    process_id: u32::from_be_bytes([*first, *second, *third, *fourth]),
                }),
                [STATUS_START_ERROR, message @ ..] => Err(anyhow::anyhow!(
                    "target executable could not start inside the sandbox: {}",
                    String::from_utf8_lossy(message)
                )),
                _ => Err(anyhow::anyhow!(
                    "sandbox launch bridge returned an invalid target status"
                )),
            },
            Err(error) => Err(error),
        };
        (
            confirmation,
            TargetCompletion {
                reader: self.reader,
            },
        )
    }
}

impl LaunchCommitStatus {
    pub async fn confirm(mut self) -> Result<()> {
        let payload = read_status_frame(&mut self.reader, "target launch commit").await?;
        match payload.as_slice() {
            [STATUS_COMMIT_OK] => Ok(()),
            [STATUS_COMMIT_ERROR, message @ ..] => bail!(
                "sandbox launch bridge could not commit target execution: {}",
                String::from_utf8_lossy(message)
            ),
            _ => bail!("sandbox launch bridge returned an invalid commit status"),
        }
    }
}

impl TargetStartGate {
    pub fn release(self) -> Result<ReleasedTargetStartGate> {
        let mut writer = std::fs::File::from(self.writer);
        writer
            .write_all(&[GATE_RELEASED])
            .context("release sandbox target preparation gate")?;
        Ok(ReleasedTargetStartGate {
            writer: writer.into(),
        })
    }
}

impl ReleasedTargetStartGate {
    pub fn commit(self) -> Result<()> {
        let mut writer = std::fs::File::from(self.writer);
        writer
            .write_all(&[TARGET_COMMITTED])
            .context("commit sandbox target launch")
    }
}

impl TargetCompletion {
    pub async fn wait(mut self) -> Result<ReportedTargetOutcome> {
        let payload = read_status_frame(&mut self.reader, "target completion").await?;
        match payload.as_slice() {
            [STATUS_TARGET_EXITED, first, second, third, fourth] => {
                let code = i32::from_be_bytes([*first, *second, *third, *fourth]);
                if code < 0 {
                    bail!("sandbox launch bridge returned a negative target exit code")
                }
                Ok(ReportedTargetOutcome::Exited(code))
            }
            [STATUS_TARGET_SIGNALED, first, second, third, fourth] => {
                let signal = i32::from_be_bytes([*first, *second, *third, *fourth]);
                if signal <= 0 {
                    bail!("sandbox launch bridge returned an invalid target signal")
                }
                Ok(ReportedTargetOutcome::Signaled(signal))
            }
            [STATUS_INFRASTRUCTURE_ERROR, message @ ..] => {
                Ok(ReportedTargetOutcome::InfrastructureError(
                    String::from_utf8_lossy(message).into_owned(),
                ))
            }
            _ => bail!("sandbox launch bridge returned an invalid target completion status"),
        }
    }
}

async fn read_status_frame(reader: &mut tokio::fs::File, phase: &str) -> Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length)
        .await
        .with_context(|| format!("sandbox launch bridge closed before reporting {phase}"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_STATUS_FRAME_SIZE {
        bail!("sandbox launch status exceeded {MAX_STATUS_FRAME_SIZE} bytes")
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .await
        .with_context(|| format!("sandbox launch bridge returned a truncated {phase} status"))?;
    Ok(payload)
}

pub fn prepare_target(
    executable: &Path,
    target: &[OsString],
    mode: LaunchBridgeMode,
) -> Result<PreparedTargetBridge> {
    let mut status_descriptors = [-1_i32; 2];
    if unsafe { libc::pipe(status_descriptors.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error()).context("create target launch status pipe");
    }
    let reader = unsafe { OwnedFd::from_raw_fd(status_descriptors[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(status_descriptors[1]) };
    set_close_on_exec(&reader)?;
    let mut gate_descriptors = [-1_i32; 2];
    if unsafe { libc::pipe(gate_descriptors.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error()).context("create target launch gate pipe");
    }
    let gate_reader = unsafe { OwnedFd::from_raw_fd(gate_descriptors[0]) };
    let gate_writer = unsafe { OwnedFd::from_raw_fd(gate_descriptors[1]) };
    set_close_on_exec(&gate_writer)?;
    let mut commit_status_descriptors = [-1_i32; 2];
    if unsafe { libc::pipe(commit_status_descriptors.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error()).context("create target commit status pipe");
    }
    let commit_status_reader = unsafe { OwnedFd::from_raw_fd(commit_status_descriptors[0]) };
    let commit_status_writer = unsafe { OwnedFd::from_raw_fd(commit_status_descriptors[1]) };
    set_close_on_exec(&commit_status_reader)?;
    let command = std::iter::once(executable.as_os_str().to_os_string())
        .chain([
            OsString::from(PRIVATE_SWITCH),
            OsString::from(status_descriptors[1].to_string()),
            OsString::from(gate_descriptors[0].to_string()),
            OsString::from(commit_status_descriptors[1].to_string()),
            OsString::from(mode.argument()),
            OsString::from("--"),
        ])
        .chain(target.iter().cloned())
        .collect();
    Ok(PreparedTargetBridge {
        command,
        status: LaunchStatus {
            reader: tokio::fs::File::from_std(reader.into()),
        },
        writer,
        commit_status: LaunchCommitStatus {
            reader: tokio::fs::File::from_std(commit_status_reader.into()),
        },
        commit_status_writer,
        gate: TargetStartGate {
            writer: gate_writer,
        },
        gate_reader,
    })
}

pub fn dispatch_if_requested() -> bool {
    let mut arguments = std::env::args_os().skip(1);
    let Some(switch) = arguments.next() else {
        return false;
    };
    let result = if switch == std::ffi::OsStr::new(PRIVATE_SWITCH) {
        dispatch_bridge(arguments)
    } else if switch == std::ffi::OsStr::new(PRIVATE_TARGET_SWITCH) {
        dispatch_target(arguments)
    } else {
        return false;
    };
    if let Err(error) = result {
        eprintln!("mcp-console-sandbox launch bridge error: {error}");
        std::process::exit(125);
    }
    true
}

fn dispatch_bridge(mut arguments: impl Iterator<Item = OsString>) -> Result<()> {
    let status_fd = arguments
        .next()
        .and_then(|argument| argument.into_string().ok())
        .context("launch bridge status descriptor is missing")?
        .parse::<i32>()
        .context("launch bridge status descriptor is invalid")?;
    let gate_fd = arguments
        .next()
        .and_then(|argument| argument.into_string().ok())
        .context("launch bridge gate descriptor is missing")?
        .parse::<i32>()
        .context("launch bridge gate descriptor is invalid")?;
    let commit_status_fd = arguments
        .next()
        .and_then(|argument| argument.into_string().ok())
        .context("launch bridge commit status descriptor is missing")?
        .parse::<i32>()
        .context("launch bridge commit status descriptor is invalid")?;
    let mode = match arguments.next().as_deref() {
        Some(mode) if mode == std::ffi::OsStr::new("direct") => LaunchBridgeMode::Direct,
        Some(mode) if mode == std::ffi::OsStr::new("namespace-pid-1") => {
            LaunchBridgeMode::NamespacePid1
        }
        _ => bail!("launch bridge mode is missing or invalid"),
    };
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        bail!("launch bridge target separator is missing")
    }
    run(
        status_fd,
        gate_fd,
        commit_status_fd,
        mode,
        arguments.collect(),
    )
}

fn dispatch_target(mut arguments: impl Iterator<Item = OsString>) -> Result<()> {
    let status_fd = arguments
        .next()
        .and_then(|argument| argument.into_string().ok())
        .context("private target status descriptor is missing")?
        .parse::<i32>()
        .context("private target status descriptor is invalid")?;
    let gate_fd = arguments
        .next()
        .and_then(|argument| argument.into_string().ok())
        .context("private target commit descriptor is missing")?
        .parse::<i32>()
        .context("private target commit descriptor is invalid")?;
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        bail!("private target separator is missing")
    }
    run_target(status_fd, gate_fd, arguments.collect())
}

fn run(
    status_fd: i32,
    gate_fd: i32,
    commit_status_fd: i32,
    mode: LaunchBridgeMode,
    target: Vec<OsString>,
) -> Result<()> {
    let (program, arguments) = target
        .split_first()
        .context("launch bridge target is missing")?;
    let mut status = unsafe { std::fs::File::from_raw_fd(status_fd) };
    set_close_on_exec(&status)?;
    let mut gate = unsafe { std::fs::File::from_raw_fd(gate_fd) };
    let mut commit_status = unsafe { std::fs::File::from_raw_fd(commit_status_fd) };
    set_close_on_exec(&commit_status)?;
    if mode == LaunchBridgeMode::NamespacePid1 {
        #[cfg(target_os = "linux")]
        if unsafe { libc::getpid() } != 1 {
            bail!("namespace launch bridge is not PID 1")
        }
        #[cfg(not(target_os = "linux"))]
        bail!("namespace PID 1 launch bridge mode is Linux-only")
    }
    ignore_supervision_signals()?;
    write_status(&mut status, STATUS_WAITING_FOR_GATE, &[])?;
    let mut release = [0_u8; 1];
    if let Err(error) = gate.read_exact(&mut release) {
        write_status(
            &mut status,
            STATUS_START_ERROR,
            format!("target launch gate closed before release: {error}").as_bytes(),
        )?;
        return Ok(());
    }
    if release != [GATE_RELEASED] {
        write_status(
            &mut status,
            STATUS_START_ERROR,
            b"target launch gate returned an invalid release value",
        )?;
        return Ok(());
    }
    let mut helper_status_descriptors = [-1_i32; 2];
    if unsafe { libc::pipe(helper_status_descriptors.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error()).context("create private target status pipe");
    }
    let helper_status_reader = unsafe { OwnedFd::from_raw_fd(helper_status_descriptors[0]) };
    let helper_status_writer = unsafe { OwnedFd::from_raw_fd(helper_status_descriptors[1]) };
    set_close_on_exec(&helper_status_reader)?;
    let executable = std::env::current_exe().context("resolve private target launcher")?;
    let mut command = Command::new(executable);
    command
        .arg(PRIVATE_TARGET_SWITCH)
        .arg(helper_status_descriptors[1].to_string())
        .arg(gate_fd.to_string())
        .arg("--")
        .arg(program)
        .args(arguments);
    unsafe {
        command.pre_exec(reset_target_signals);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            write_status(
                &mut status,
                STATUS_START_ERROR,
                error.to_string().as_bytes(),
            )?;
            return Ok(());
        }
    };
    drop(helper_status_writer);
    drop(gate);
    let mut helper_status = std::fs::File::from(helper_status_reader);
    match read_sync_status_frame(&mut helper_status, "private target preparation")? {
        Some(payload) if payload == [TARGET_HELPER_READY] => {}
        Some(payload) if payload.first() == Some(&TARGET_HELPER_ERROR) => {
            write_status(&mut status, STATUS_START_ERROR, &payload[1..])?;
            let _ = child.wait();
            return Ok(());
        }
        Some(_) => bail!("private target launcher returned an invalid preparation status"),
        None => bail!("private target launcher closed before reporting preparation"),
    }
    let process_id = child.id();
    if let Err(error) = write_status(&mut status, STATUS_STARTED, &process_id.to_be_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    match read_sync_status_frame(&mut helper_status, "private target execution")? {
        None => {
            let redirect_result = (|| -> Result<()> {
                let null = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/null")
                    .context("open null device for resident launch bridge")?;
                for descriptor in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    if unsafe { libc::dup2(null.as_raw_fd(), descriptor) } == -1 {
                        return Err(std::io::Error::last_os_error()).with_context(|| {
                            format!("redirect resident launch bridge descriptor {descriptor}")
                        });
                    }
                }
                Ok(())
            })();
            if let Err(error) = redirect_result {
                let message =
                    format!("could not release resident launch bridge standard streams: {error:#}");
                write_status(&mut commit_status, STATUS_COMMIT_ERROR, message.as_bytes())?;
                write_status(&mut status, STATUS_INFRASTRUCTURE_ERROR, message.as_bytes())?;
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            write_status(&mut commit_status, STATUS_COMMIT_OK, &[])?;
        }
        Some(payload) if payload.first() == Some(&TARGET_HELPER_ERROR) => {
            write_status(&mut commit_status, STATUS_COMMIT_ERROR, &payload[1..])?;
            write_status(&mut status, STATUS_INFRASTRUCTURE_ERROR, &payload[1..])?;
            let _ = child.wait();
            return Ok(());
        }
        Some(_) => bail!("private target launcher returned an invalid execution status"),
    }
    drop(commit_status);
    let outcome = match child.wait().context("wait for sandbox target root") {
        Ok(outcome) => outcome,
        Err(error) => {
            write_status(
                &mut status,
                STATUS_INFRASTRUCTURE_ERROR,
                format!("{error:#}").as_bytes(),
            )?;
            return Err(error);
        }
    };
    if let Some(code) = outcome.code() {
        write_status(&mut status, STATUS_TARGET_EXITED, &code.to_be_bytes())?;
        drop(status);
        if mode == LaunchBridgeMode::NamespacePid1 {
            reap_namespace_descendants()?;
        }
        std::process::exit(code);
    }
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&outcome) {
        write_status(&mut status, STATUS_TARGET_SIGNALED, &signal.to_be_bytes())?;
        drop(status);
        if mode == LaunchBridgeMode::NamespacePid1 {
            reap_namespace_descendants()?;
        }
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
            libc::_exit(128 + signal);
        }
    }
    let error = anyhow::anyhow!("sandbox target returned an unrecognized wait status");
    write_status(
        &mut status,
        STATUS_INFRASTRUCTURE_ERROR,
        error.to_string().as_bytes(),
    )?;
    Err(error)
}

fn run_target(status_fd: i32, gate_fd: i32, target: Vec<OsString>) -> Result<()> {
    let (program, arguments) = target
        .split_first()
        .context("private target command is missing")?;
    let mut status = unsafe { std::fs::File::from_raw_fd(status_fd) };
    set_close_on_exec(&status)?;
    let mut gate = unsafe { std::fs::File::from_raw_fd(gate_fd) };
    set_close_on_exec(&gate)?;
    write_status(&mut status, TARGET_HELPER_READY, &[])?;
    let mut commit = [0_u8; 1];
    if let Err(error) = gate.read_exact(&mut commit) {
        let message = format!("target launch commit closed before release: {error}");
        let _ = write_status(&mut status, TARGET_HELPER_ERROR, message.as_bytes());
        bail!(message)
    }
    if commit != [TARGET_COMMITTED] {
        let message = "target launch commit returned an invalid value";
        let _ = write_status(&mut status, TARGET_HELPER_ERROR, message.as_bytes());
        bail!(message)
    }
    drop(gate);
    let error = Command::new(program).args(arguments).exec();
    let message = error.to_string();
    let _ = write_status(&mut status, TARGET_HELPER_ERROR, message.as_bytes());
    Err(error).context("execute committed sandbox target")
}

fn read_sync_status_frame(reader: &mut std::fs::File, phase: &str) -> Result<Option<Vec<u8>>> {
    let mut length = [0_u8; 4];
    match reader.read(&mut length[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("one-byte read returned more than one byte"),
        Err(error) => return Err(error).with_context(|| format!("read {phase} status")),
    }
    reader
        .read_exact(&mut length[1..])
        .with_context(|| format!("truncated {phase} status length"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_STATUS_FRAME_SIZE {
        bail!("{phase} status exceeded {MAX_STATUS_FRAME_SIZE} bytes")
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .with_context(|| format!("truncated {phase} status payload"))?;
    Ok(Some(payload))
}

fn reap_namespace_descendants() -> Result<()> {
    loop {
        let mut status = 0;
        let process_id = unsafe { libc::waitpid(-1, &mut status, 0) };
        if process_id > 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Ok(()),
            _ => return Err(error).context("reap adopted sandbox descendant"),
        }
    }
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

fn set_close_on_exec(descriptor: &impl std::os::fd::AsRawFd) -> Result<()> {
    let descriptor = descriptor.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
    {
        return Err(std::io::Error::last_os_error()).context("secure private launch descriptor");
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

fn reset_target_signals() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
