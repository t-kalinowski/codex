#![cfg(target_os = "macos")]

use crate::cleanup::CleanupDirectory;
use crate::cleanup::DirectoryIdentity;
use crate::process::ProcessIdentity;
use crate::process::process_info;
use crate::process::signal_process;
use crate::process_tracker::DescendantTracker;
use crate::process_tracker::TrackerCommand;
use crate::process_tracker::TrackerOutcome;
use crate::protocol::LifecyclePolicy;
use crate::protocol::StopDeadlines;
use crate::stdio::ForegroundTerminal;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::Duration;

const PRIVATE_SWITCH: &str = "--mcp-console-private-lifetime-manager";
const INITIALIZATION_MAGIC: &[u8; 4] = b"MCL1";
const READY: u8 = 1;
const COMMIT: u8 = 2;
const FINISH: u8 = 3;
const TERMINATE: u8 = 4;
const STOP: u8 = 5;
const COMMITTED: u8 = 7;
const COMPLETE: u8 = 8;
const CLEANUP_FAILED: u8 = 9;
const MAXIMUM_PATH_BYTES: usize = 16 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const FINISH_ALLOWANCE: Duration = Duration::from_secs(1);

pub struct LifetimeManager {
    child: Option<Child>,
    child_identity: Option<ProcessIdentity>,
    monitor: Option<ManagerMonitor>,
    stream: Option<UnixStream>,
    control: LifetimeControl,
    finish_timeout: Duration,
    force_timeout: Duration,
}

#[derive(Clone)]
pub struct LifetimeControl {
    stream: Arc<Mutex<UnixStream>>,
    stopped: Arc<AtomicBool>,
    manager: ProcessIdentity,
}

struct ManagerMonitor {
    identity: ProcessIdentity,
    result: Receiver<Result<ManagerExit, String>>,
    thread: Option<JoinHandle<()>>,
    recovery_timeout: Duration,
}

enum ManagerExit {
    Normal { forced: bool },
    Recovered,
}

pub struct LifetimeOutcome {
    pub forced: bool,
    pub cleanup_error: Option<String>,
}

pub(crate) fn stop_phase_timeout(lifecycle: &LifecyclePolicy) -> Duration {
    Duration::from_millis(lifecycle.force_timeout_ms).saturating_add(FINISH_ALLOWANCE)
}

struct Initialization {
    root_pid: libc::pid_t,
    lifecycle: LifecyclePolicy,
    cleanup_directory: PathBuf,
    cleanup_identity: DirectoryIdentity,
}

enum OwnerDisposition {
    Awaiting,
    Finish,
    Stop,
    Lost(Option<String>),
}

impl LifetimeControl {
    pub fn terminate(&self, deadlines: &StopDeadlines) -> Result<(), String> {
        let mut command = Vec::with_capacity(17);
        command.push(TERMINATE);
        command.extend_from_slice(&deadlines.graceful_ms.to_be_bytes());
        command.extend_from_slice(&deadlines.force_ms.to_be_bytes());
        self.write_or_recover(&command, "request sandbox lifetime termination")
    }

    pub fn stop(&self) -> Result<(), String> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.write_or_recover(&[STOP], "stop sandbox lifetime ownership")
    }

    fn finish(&self) -> Result<(), String> {
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        self.write_or_recover(&[FINISH], "finish sandbox lifetime ownership")
    }

    pub(crate) fn force_manager(&self) -> Result<(), String> {
        signal_process(self.manager, libc::SIGKILL).map(|_| ())
    }

    fn write_or_recover(&self, bytes: &[u8], operation: &str) -> Result<(), String> {
        match self.write(bytes, FINISH_ALLOWANCE) {
            Ok(()) => Ok(()),
            Err(error) => {
                let error = format!("failed to {operation}: {error}");
                match self.force_manager() {
                    Ok(()) => Err(error),
                    Err(manager_error) => Err(with_prior_error(Some(error), manager_error)),
                }
            }
        }
    }

    fn write(&self, bytes: &[u8], timeout: Duration) -> std::io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(bytes)
    }
}

impl LifetimeManager {
    pub fn spawn(
        lifecycle: &LifecyclePolicy,
        foreground_terminal: Option<&ForegroundTerminal>,
    ) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("failed to locate the sandbox lifetime manager: {error}"))?;
        let (stream, inherited_stream) = UnixStream::pair()
            .map_err(|error| format!("failed to create sandbox lifetime control: {error}"))?;
        let inherited_descriptor = inherited_stream.as_raw_fd();
        let inherited_terminal = foreground_terminal
            .map(ForegroundTerminal::duplicate_for_lifetime_manager)
            .transpose()
            .map_err(|error| format!("failed to retain the sandbox lifetime terminal: {error}"))?;
        let terminal_descriptor = inherited_terminal.as_ref().map(AsRawFd::as_raw_fd);
        let mut command = Command::new(executable);
        command
            .arg(PRIVATE_SWITCH)
            .arg(inherited_descriptor.to_string())
            .arg(
                terminal_descriptor
                    .map(|descriptor| descriptor.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                ignore_owner_signals_native()?;
                configure_descriptors(inherited_descriptor, terminal_descriptor)
            });
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to launch the sandbox lifetime manager: {error}"))?;
        let child_pid = child.id() as libc::pid_t;
        let child_identity = match process_info(child_pid) {
            Ok(Some(info)) => info.identity,
            Ok(None) => {
                return Err(stop_and_reap(
                    &mut child,
                    "sandbox lifetime manager exited before observation".to_string(),
                ));
            }
            Err(error) => return Err(stop_and_reap(&mut child, error)),
        };
        drop(inherited_stream);
        drop(inherited_terminal);
        let reader = stream
            .try_clone()
            .map_err(|error| format!("failed to clone sandbox lifetime control: {error}"))?;
        let control = LifetimeControl {
            stream: Arc::new(Mutex::new(stream)),
            stopped: Arc::new(AtomicBool::new(false)),
            manager: child_identity,
        };
        let force_timeout = Duration::from_millis(lifecycle.force_timeout_ms);
        let finish_timeout = Duration::from_millis(lifecycle.root_exit_grace_ms)
            .saturating_add(Duration::from_millis(lifecycle.terminate_grace_ms))
            .saturating_add(force_timeout)
            .saturating_add(FINISH_ALLOWANCE);
        Ok(Self {
            child: Some(child),
            child_identity: Some(child_identity),
            monitor: None,
            stream: Some(reader),
            control,
            finish_timeout,
            force_timeout,
        })
    }

    pub fn control(&self) -> LifetimeControl {
        self.control.clone()
    }

    pub fn observe(
        &mut self,
        root_pid: u32,
        lifecycle: &LifecyclePolicy,
        cleanup_directory: &CleanupDirectory,
    ) -> Result<(), String> {
        let root_pid = libc::pid_t::try_from(root_pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| "sandbox lifetime manager received an invalid root PID".to_string())?;
        let path = cleanup_directory.path().as_os_str().as_bytes();
        let path_length = u32::try_from(path.len())
            .ok()
            .filter(|length| *length as usize <= MAXIMUM_PATH_BYTES)
            .ok_or_else(|| "target cleanup directory path is too long".to_string())?;
        let identity = cleanup_directory.identity();
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| "sandbox lifetime control is unavailable".to_string())?;
        stream
            .set_read_timeout(Some(STARTUP_TIMEOUT))
            .map_err(|error| format!("failed to configure sandbox lifetime control: {error}"))?;
        let mut initialization = Vec::with_capacity(64 + path.len());
        initialization.extend_from_slice(INITIALIZATION_MAGIC);
        initialization.extend_from_slice(&root_pid.to_be_bytes());
        initialization.extend_from_slice(&lifecycle.root_exit_grace_ms.to_be_bytes());
        initialization.extend_from_slice(&lifecycle.terminate_grace_ms.to_be_bytes());
        initialization.extend_from_slice(&lifecycle.force_timeout_ms.to_be_bytes());
        initialization.extend_from_slice(&identity.device.to_be_bytes());
        initialization.extend_from_slice(&identity.inode.to_be_bytes());
        initialization.extend_from_slice(&identity.owner.to_be_bytes());
        initialization.extend_from_slice(&path_length.to_be_bytes());
        initialization.extend_from_slice(path);
        self.control
            .write(&initialization, STARTUP_TIMEOUT)
            .map_err(|error| format!("failed to initialize sandbox lifetime manager: {error}"))?;
        let mut ready = [0];
        stream
            .read_exact(&mut ready)
            .map_err(|error| format!("sandbox lifetime manager did not become ready: {error}"))?;
        if ready != [READY] {
            return Err("sandbox lifetime manager sent invalid readiness".to_string());
        }
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), String> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| "sandbox lifetime control is unavailable".to_string())?;
        self.control
            .write(&[COMMIT], STARTUP_TIMEOUT)
            .map_err(|error| format!("failed to commit sandbox lifetime ownership: {error}"))?;
        let mut committed = [0];
        stream
            .read_exact(&mut committed)
            .map_err(|error| format!("sandbox lifetime ownership was not confirmed: {error}"))?;
        if committed != [COMMITTED] {
            return Err("sandbox lifetime manager sent invalid ownership confirmation".to_string());
        }
        Ok(())
    }

    pub fn monitor(
        &mut self,
        root_pid: u32,
        cleanup_directory: CleanupDirectory,
    ) -> Result<(), String> {
        let root_pid = root_pid as libc::pid_t;
        let root = process_info(root_pid)?
            .filter(|info| !info.is_zombie)
            .map(|info| info.identity)
            .ok_or_else(|| "sandbox root exited before fallback ownership".to_string())?;
        let child = self
            .child
            .take()
            .ok_or_else(|| "sandbox lifetime manager child is unavailable".to_string())?;
        let identity = self
            .child_identity
            .take()
            .ok_or_else(|| "sandbox lifetime manager identity is unavailable".to_string())?;
        self.monitor = Some(ManagerMonitor::start(
            child,
            identity,
            root,
            cleanup_directory,
            self.force_timeout,
        ));
        Ok(())
    }

    pub fn finish(mut self) -> Result<LifetimeOutcome, String> {
        let timeout = self.finish_timeout;
        self.finish_inner(/*stop*/ false, timeout)
    }

    pub fn finish_with_timeout(mut self, timeout: Duration) -> Result<LifetimeOutcome, String> {
        self.finish_inner(
            /*stop*/ false,
            timeout.saturating_add(FINISH_ALLOWANCE),
        )
    }

    pub fn stop(mut self) -> Result<LifetimeOutcome, String> {
        let timeout = self.force_timeout.saturating_add(FINISH_ALLOWANCE);
        self.finish_inner(/*stop*/ true, timeout)
    }

    fn finish_inner(
        &mut self,
        stop: bool,
        finish_timeout: Duration,
    ) -> Result<LifetimeOutcome, String> {
        let finish_deadline = std::time::Instant::now() + finish_timeout;
        let disposition = if stop {
            self.control.stop()
        } else {
            self.control.finish()
        };
        let mut forced = false;
        let mut cleanup_error = None;
        let mut error = disposition.err();
        if let Some(mut stream) = self.stream.take() {
            let remaining = finish_deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                error = Some("timed out waiting for sandbox lifetime cleanup".to_string());
            } else if let Err(timeout_error) = stream.set_read_timeout(Some(remaining)) {
                error = Some(format!(
                    "failed to configure sandbox lifetime cleanup timeout: {timeout_error}"
                ));
            } else if error.is_none() {
                let mut completion = [0_u8; 2];
                if let Err(read_error) = stream.read_exact(&mut completion) {
                    error = Some(format!(
                        "sandbox lifetime manager did not confirm cleanup: {read_error}"
                    ));
                } else if !matches!(completion[0], COMPLETE | CLEANUP_FAILED) || completion[1] > 1 {
                    error = Some(
                        "sandbox lifetime manager sent invalid cleanup confirmation".to_string(),
                    );
                } else {
                    forced = completion[1] == 1;
                    if completion[0] == CLEANUP_FAILED {
                        let mut length = [0_u8; 4];
                        if let Err(read_error) = stream.read_exact(&mut length) {
                            error = Some(format!(
                                "sandbox lifetime manager sent truncated cleanup failure: {read_error}"
                            ));
                        } else {
                            let length = u32::from_be_bytes(length) as usize;
                            if length > MAXIMUM_PATH_BYTES {
                                error = Some(
                                    "sandbox lifetime cleanup failure exceeded its bound"
                                        .to_string(),
                                );
                            } else {
                                let mut message = vec![0_u8; length];
                                match stream.read_exact(&mut message) {
                                    Ok(()) => {
                                        cleanup_error =
                                            Some(String::from_utf8_lossy(&message).into_owned());
                                    }
                                    Err(read_error) => {
                                        error = Some(format!(
                                            "sandbox lifetime manager sent truncated cleanup failure: {read_error}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let remaining = finish_deadline.saturating_duration_since(std::time::Instant::now());
        let manager_exit = if let Some(monitor) = self.monitor.take() {
            monitor.finish(remaining)
        } else if let Some(mut child) = self.child.take() {
            wait_manager_child(&mut child, remaining).map(|()| ManagerExit::Normal { forced })
        } else {
            Ok(ManagerExit::Normal { forced })
        };
        match manager_exit {
            Ok(ManagerExit::Normal {
                forced: manager_forced,
            }) if error.is_none() => Ok(LifetimeOutcome {
                forced: forced || manager_forced,
                cleanup_error,
            }),
            Ok(ManagerExit::Recovered) => Ok(LifetimeOutcome {
                forced: true,
                cleanup_error: None,
            }),
            Ok(ManagerExit::Normal { .. }) => Err(error
                .unwrap_or_else(|| "sandbox lifetime control failed without an error".to_string())),
            Err(manager_error) => Err(with_prior_error(error, manager_error)),
        }
    }
}

impl Drop for LifetimeManager {
    fn drop(&mut self) {
        let timeout = self.force_timeout.saturating_add(FINISH_ALLOWANCE);
        let _ = self.finish_inner(/*stop*/ true, timeout);
    }
}

impl ManagerMonitor {
    fn start(
        child: Child,
        identity: ProcessIdentity,
        root: ProcessIdentity,
        cleanup_directory: CleanupDirectory,
        cleanup_timeout: Duration,
    ) -> Self {
        let (result_sender, result) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = monitor_manager(child, root, cleanup_directory, cleanup_timeout);
            let _ = result_sender.send(result);
        });
        Self {
            identity,
            result,
            thread: Some(thread),
            recovery_timeout: cleanup_timeout.saturating_add(FINISH_ALLOWANCE),
        }
    }

    fn finish(mut self, timeout: Duration) -> Result<ManagerExit, String> {
        let mut prior_error = None;
        let result = match self.result.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                prior_error = Some("timed out waiting for sandbox lifetime cleanup".to_string());
                if let Err(signal_error) = signal_process(self.identity, libc::SIGKILL) {
                    prior_error = Some(with_prior_error(prior_error, signal_error));
                }
                match self.result.recv_timeout(self.recovery_timeout) {
                    Ok(result) => result,
                    Err(RecvTimeoutError::Timeout) => {
                        Err("timed out recovering failed sandbox lifetime ownership".to_string())
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        Err("sandbox lifetime monitor ended without a result".to_string())
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err("sandbox lifetime monitor ended without a result".to_string())
            }
        };
        let exit = match result {
            Ok(exit) => Some(exit),
            Err(result_error) => {
                prior_error = Some(with_prior_error(prior_error, result_error));
                None
            }
        };
        let mut error = if exit.is_some() { None } else { prior_error };
        match self.thread.take() {
            Some(thread) => {
                if !thread.is_finished() {
                    drop(thread);
                } else if thread.join().is_err() {
                    error = Some(with_prior_error(
                        error,
                        "sandbox lifetime monitor failed".to_string(),
                    ));
                }
            }
            None => {
                error = Some(with_prior_error(
                    error,
                    "sandbox lifetime monitor thread is unavailable".to_string(),
                ));
            }
        }
        match (error, exit) {
            (Some(error), _) => Err(error),
            (None, Some(exit)) => Ok(exit),
            (None, None) => Err("sandbox lifetime monitor produced no result".to_string()),
        }
    }
}

pub fn dispatch_if_requested() -> bool {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(PRIVATE_SWITCH)) {
        return false;
    }
    let result = (|| {
        let control_descriptor = parse_descriptor(arguments.next(), "control")?
            .ok_or_else(|| "sandbox lifetime control descriptor is missing".to_string())?;
        let terminal_descriptor = parse_descriptor(arguments.next(), "terminal")?;
        if arguments.next().is_some() {
            return Err("sandbox lifetime manager received extra arguments".to_string());
        }
        run(control_descriptor, terminal_descriptor)
    })();
    if let Err(error) = result {
        eprintln!("mcp-console-sandbox lifetime manager error: {error}");
        std::process::exit(2);
    }
    true
}

fn run(
    control_descriptor: libc::c_int,
    terminal_descriptor: Option<libc::c_int>,
) -> Result<(), String> {
    ignore_owner_signals()?;
    let owner_pid = unsafe { libc::getppid() };
    if owner_pid <= 0 {
        return Err("sandbox lifetime manager has no valid owner".to_string());
    }
    let mut stream = unsafe { UnixStream::from_raw_fd(control_descriptor) };
    let foreground_terminal = terminal_descriptor
        .map(|descriptor| unsafe { ForegroundTerminal::from_inherited_descriptor(descriptor) })
        .transpose()
        .map_err(|error| format!("failed to adopt the sandbox lifetime terminal: {error}"))?;
    let initialization = read_initialization(&mut stream)?;
    let info = process_info(initialization.root_pid)?
        .ok_or_else(|| "sandbox root exited before lifetime manager startup".to_string())?;
    if info.parent_pid != owner_pid {
        return Err("sandbox root is not a child of the lifetime-manager owner".to_string());
    }
    let tracker = DescendantTracker::start(info.identity)?;
    let cleanup_directory = CleanupDirectory::adopt(
        initialization.cleanup_directory,
        initialization.cleanup_identity,
    )
    .map_err(|error| error.to_string())?;
    let root = info.identity;
    if let Err(error) = stream.write_all(&[READY]) {
        return finish_startup_failure(error.to_string(), tracker, cleanup_directory);
    }
    let mut commit = [0];
    if let Err(error) = stream.read_exact(&mut commit) {
        return finish_startup_failure(error.to_string(), tracker, cleanup_directory);
    }
    if commit != [COMMIT] {
        return finish_startup_failure(
            "sandbox lifetime ownership commit is invalid".to_string(),
            tracker,
            cleanup_directory,
        );
    }
    let (command_sender, command_receiver) = mpsc::channel();
    let tracker_control = stream
        .try_clone()
        .map_err(|error| format!("failed to monitor sandbox lifetime control: {error}"))?;
    let lifecycle = initialization.lifecycle;
    let tracker_thread = std::thread::spawn(move || {
        supervise_tracker(
            tracker,
            &lifecycle,
            &command_receiver,
            root,
            tracker_control,
        )
    });
    if let Err(error) = stream.write_all(&[COMMITTED]) {
        let _ = command_sender.send(TrackerCommand::Retire);
        let result = join_tracker(tracker_thread);
        return match result {
            Ok(_) => cleanup_directory.remove().map_err(|cleanup_error| {
                with_prior_error(Some(error.to_string()), cleanup_error.to_string())
            }),
            Err(retirement_error) => {
                cleanup_directory.preserve();
                Err(with_prior_error(Some(error.to_string()), retirement_error))
            }
        };
    }
    stream
        .set_read_timeout(Some(Duration::from_millis(10)))
        .map_err(|error| format!("failed to monitor sandbox lifetime control: {error}"))?;
    let disposition = wait_for_tracker_disposition(&mut stream, &command_sender, &tracker_thread);
    let owner_connected = !matches!(&disposition, OwnerDisposition::Lost(_));
    let tracker_result = join_tracker(tracker_thread);
    let mut error = match disposition {
        OwnerDisposition::Finish | OwnerDisposition::Stop => None,
        OwnerDisposition::Lost(error) => error,
        OwnerDisposition::Awaiting => {
            Some("sandbox lifetime owner disposition was not resolved".to_string())
        }
    };
    let outcome = match tracker_result {
        Ok(outcome) => outcome,
        Err(tracker_error) => {
            let error = with_prior_error(error, tracker_error);
            cleanup_directory.preserve();
            return Err(error);
        }
    };
    let terminal_error = foreground_terminal
        .and_then(|terminal| terminal.restore().err())
        .map(|error| format!("failed to restore the foreground terminal: {error}"));
    error = match terminal_error {
        Some(terminal_error) => Some(with_prior_error(error, terminal_error)),
        None => error,
    };
    let cleanup_result = cleanup_directory.remove();
    if owner_connected {
        if let Err(cleanup_error) = cleanup_result {
            let message = cleanup_error.to_string();
            let message = message.as_bytes();
            let message = &message[..message.len().min(MAXIMUM_PATH_BYTES)];
            stream
                .write_all(&[CLEANUP_FAILED, u8::from(outcome.forced)])
                .and_then(|()| stream.write_all(&(message.len() as u32).to_be_bytes()))
                .and_then(|()| stream.write_all(message))
                .map_err(|write_error| {
                    format!("failed to report sandbox lifetime cleanup failure: {write_error}")
                })?;
            return error.map_or(Ok(()), Err);
        }
        if error.is_none() {
            stream
                .write_all(&[COMPLETE, u8::from(outcome.forced)])
                .map_err(|write_error| {
                    format!("failed to confirm sandbox lifetime cleanup: {write_error}")
                })?;
        }
    } else if let Err(cleanup_error) = cleanup_result {
        error = Some(with_prior_error(error, cleanup_error.to_string()));
    }
    error.map_or(Ok(()), Err)
}

fn finish_startup_failure(
    error: String,
    tracker: DescendantTracker,
    cleanup_directory: CleanupDirectory,
) -> Result<(), String> {
    match tracker.stop(STARTUP_TIMEOUT) {
        Ok(_) => {
            drop(cleanup_directory);
            Err(error)
        }
        Err(cleanup_error) => {
            cleanup_directory.preserve();
            Err(with_prior_error(Some(error), cleanup_error))
        }
    }
}

fn supervise_tracker(
    tracker: DescendantTracker,
    lifecycle: &LifecyclePolicy,
    commands: &Receiver<TrackerCommand>,
    root: ProcessIdentity,
    control: UnixStream,
) -> Result<TrackerOutcome, String> {
    match tracker.supervise(lifecycle, commands) {
        Ok(outcome) => Ok(outcome),
        Err(mut error) => {
            if let Err(signal_error) = signal_process(root, libc::SIGKILL) {
                error = with_prior_error(Some(error), signal_error);
            }
            if let Err(control_error) = control.shutdown(Shutdown::Both) {
                error = with_prior_error(Some(error), control_error.to_string());
            }
            Err(error)
        }
    }
}

fn monitor_manager(
    mut child: Child,
    root: ProcessIdentity,
    cleanup_directory: CleanupDirectory,
    cleanup_timeout: Duration,
) -> Result<ManagerExit, String> {
    let status = child
        .wait()
        .map_err(|error| format!("failed to reap sandbox lifetime manager: {error}"));
    match status {
        Ok(status) if status.success() => {
            cleanup_directory.preserve();
            Ok(ManagerExit::Normal { forced: false })
        }
        Ok(status) => recover_manager_failure(
            format!("sandbox lifetime manager exited with status {status}"),
            root,
            cleanup_directory,
            cleanup_timeout,
        ),
        Err(error) => recover_manager_failure(error, root, cleanup_directory, cleanup_timeout),
    }
}

fn recover_manager_failure(
    error: String,
    root: ProcessIdentity,
    cleanup_directory: CleanupDirectory,
    cleanup_timeout: Duration,
) -> Result<ManagerExit, String> {
    let root_live = process_info(root.pid)
        .map_err(|inspect_error| with_prior_error(Some(error.clone()), inspect_error))?
        .is_some_and(|info| info.identity == root && !info.is_zombie);
    if !root_live {
        cleanup_directory.preserve();
        return Err(with_prior_error(
            Some(error),
            "sandbox root exited before fallback supervision".to_string(),
        ));
    }
    match DescendantTracker::start(root).and_then(|tracker| tracker.stop(cleanup_timeout)) {
        Ok(_) => cleanup_directory
            .remove()
            .map(|()| ManagerExit::Recovered)
            .map_err(|cleanup_error| with_prior_error(Some(error), cleanup_error.to_string())),
        Err(cleanup_error) => {
            cleanup_directory.preserve();
            Err(with_prior_error(Some(error), cleanup_error))
        }
    }
}

fn wait_for_tracker_disposition(
    stream: &mut UnixStream,
    commands: &mpsc::Sender<TrackerCommand>,
    tracker: &JoinHandle<Result<TrackerOutcome, String>>,
) -> OwnerDisposition {
    let mut disposition = OwnerDisposition::Awaiting;
    loop {
        if tracker.is_finished() && !matches!(disposition, OwnerDisposition::Awaiting) {
            return disposition;
        }
        if matches!(disposition, OwnerDisposition::Lost(_)) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let mut command = [0];
        match stream.read(&mut command) {
            Ok(0) => {
                let _ = commands.send(TrackerCommand::Retire);
                disposition = OwnerDisposition::Lost(None);
            }
            Ok(_) if command == [FINISH] => disposition = OwnerDisposition::Finish,
            Ok(_) if command == [STOP] => {
                let _ = commands.send(TrackerCommand::Retire);
                disposition = OwnerDisposition::Stop;
            }
            Ok(_) if command == [TERMINATE] => {
                let graceful = match read_command_duration(stream) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = commands.send(TrackerCommand::Retire);
                        disposition = OwnerDisposition::Lost(Some(error));
                        continue;
                    }
                };
                let force = match read_command_duration(stream) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = commands.send(TrackerCommand::Retire);
                        disposition = OwnerDisposition::Lost(Some(error));
                        continue;
                    }
                };
                if force.is_zero() {
                    let _ = commands.send(TrackerCommand::Retire);
                    disposition = OwnerDisposition::Lost(Some(
                        "sandbox lifetime termination force deadline is invalid".to_string(),
                    ));
                } else {
                    let _ = commands.send(TrackerCommand::Terminate { graceful, force });
                }
            }
            Ok(_) => {
                let _ = commands.send(TrackerCommand::Retire);
                disposition = OwnerDisposition::Lost(Some(
                    "sandbox lifetime manager received invalid disposition".to_string(),
                ));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => {
                let _ = commands.send(TrackerCommand::Retire);
                disposition = OwnerDisposition::Lost(Some(error.to_string()));
            }
        }
    }
}

fn read_command_duration(stream: &mut UnixStream) -> Result<Duration, String> {
    let mut bytes = [0_u8; 8];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| format!("sandbox lifetime termination was truncated: {error}"))?;
    Ok(Duration::from_millis(u64::from_be_bytes(bytes)))
}

fn parse_descriptor(value: Option<OsString>, name: &str) -> Result<Option<libc::c_int>, String> {
    let Some(value) = value else {
        return Err(format!("sandbox lifetime {name} descriptor is missing"));
    };
    if value == "-" {
        return Ok(None);
    }
    value
        .into_string()
        .ok()
        .and_then(|descriptor| descriptor.parse::<libc::c_int>().ok())
        .filter(|descriptor| *descriptor > libc::STDERR_FILENO)
        .map(Some)
        .ok_or_else(|| format!("sandbox lifetime {name} descriptor is invalid"))
}

fn ignore_owner_signals() -> Result<(), String> {
    ignore_owner_signals_native()
        .map_err(|error| format!("failed to isolate sandbox lifetime manager signals: {error}"))
}

fn ignore_owner_signals_native() -> std::io::Result<()> {
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        if unsafe { libc::signal(signal, libc::SIG_IGN) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn read_initialization(stream: &mut UnixStream) -> Result<Initialization, String> {
    let mut magic = [0; INITIALIZATION_MAGIC.len()];
    stream
        .read_exact(&mut magic)
        .map_err(|error| format!("failed to read lifetime initialization: {error}"))?;
    if &magic != INITIALIZATION_MAGIC {
        return Err("sandbox lifetime initialization has invalid version".to_string());
    }
    let root_pid = read_number::<{ std::mem::size_of::<libc::pid_t>() }>(stream)?;
    let root_exit_grace_ms = u64::from_be_bytes(read_number::<8>(stream)?);
    let terminate_grace_ms = u64::from_be_bytes(read_number::<8>(stream)?);
    let force_timeout_ms = u64::from_be_bytes(read_number::<8>(stream)?);
    let device = u64::from_be_bytes(read_number::<8>(stream)?);
    let inode = u64::from_be_bytes(read_number::<8>(stream)?);
    let owner = u32::from_be_bytes(read_number::<4>(stream)?);
    let path_length = u32::from_be_bytes(read_number::<4>(stream)?) as usize;
    let root_pid = libc::pid_t::from_be_bytes(root_pid);
    if root_pid <= 0 || force_timeout_ms == 0 || path_length > MAXIMUM_PATH_BYTES {
        return Err("sandbox lifetime initialization is invalid".to_string());
    }
    let mut path = vec![0; path_length];
    stream
        .read_exact(&mut path)
        .map_err(|error| format!("failed to read target cleanup directory: {error}"))?;
    Ok(Initialization {
        root_pid,
        lifecycle: LifecyclePolicy {
            kind: crate::protocol::LaunchKind::Command,
            root_exit_grace_ms,
            terminate_grace_ms,
            force_timeout_ms,
        },
        cleanup_directory: PathBuf::from(OsString::from_vec(path)),
        cleanup_identity: DirectoryIdentity {
            device,
            inode,
            owner,
        },
    })
}

fn read_number<const N: usize>(stream: &mut UnixStream) -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| format!("failed to read lifetime initialization: {error}"))?;
    Ok(bytes)
}

fn join_tracker(
    tracker: JoinHandle<Result<TrackerOutcome, String>>,
) -> Result<TrackerOutcome, String> {
    tracker
        .join()
        .map_err(|_| "sandbox lifetime process tracker failed".to_string())?
}

fn wait_manager_child(child: &mut Child, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "sandbox lifetime manager exited with status {status}"
                ));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                return Err(stop_and_reap(
                    child,
                    "timed out waiting for sandbox lifetime manager".to_string(),
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn set_inheritable(descriptor: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn configure_descriptors(
    control: libc::c_int,
    terminal: Option<libc::c_int>,
) -> std::io::Result<()> {
    let descriptor_limit = unsafe { libc::getdtablesize() };
    if descriptor_limit <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    for descriptor in (libc::STDERR_FILENO + 1)..descriptor_limit {
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(error);
        }
        let desired = if descriptor == control || terminal == Some(descriptor) {
            flags & !libc::FD_CLOEXEC
        } else {
            flags | libc::FD_CLOEXEC
        };
        if desired != flags && unsafe { libc::fcntl(descriptor, libc::F_SETFD, desired) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    set_inheritable(control)?;
    if let Some(terminal) = terminal {
        set_inheritable(terminal)?;
    }
    Ok(())
}

fn stop_and_reap(child: &mut Child, mut error: String) -> String {
    if let Err(kill_error) = child.kill()
        && kill_error.raw_os_error() != Some(libc::ESRCH)
    {
        error = with_prior_error(Some(error), kill_error.to_string());
    }
    if let Err(wait_error) = child.wait() {
        error = with_prior_error(Some(error), wait_error.to_string());
    }
    error
}

fn with_prior_error(prior: Option<String>, error: String) -> String {
    prior.map_or(error.clone(), |prior| {
        if error.is_empty() {
            prior
        } else {
            format!("{prior}; additionally, {error}")
        }
    })
}
