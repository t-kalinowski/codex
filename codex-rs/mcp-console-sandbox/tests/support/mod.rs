#![cfg(unix)]

use codex_utils_cargo_bin::cargo_bin;
use serde_json::Value;
use serde_json::json;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use tempfile::TempDir;

mod runner_executable;
pub use runner_executable::RunnerExecutable;
pub use runner_executable::apply_sanitized_environment;
use runner_executable::apply_sanitized_native_environment;

pub const PROTOCOL_VERSION: u64 = 1;
pub const TARGET_STDIN_FD: i32 = 190;
pub const TARGET_STDOUT_FD: i32 = 191;
pub const TARGET_STDERR_FD: i32 = 192;
const CHILD_CONTROL_FD: i32 = 198;

pub struct Runner {
    _process_id: u32,
    child: Child,
    pub control: UnixStream,
    state_dir: TempDir,
    _cleanup_dir: TempDir,
    _runner_executable: RunnerExecutable,
}

pub struct TargetIo {
    pub stdin: UnixStream,
    pub stdout: UnixStream,
    pub stderr: UnixStream,
}

impl Runner {
    pub fn spawn(
        target: &[OsString],
        environment: &[(&str, &str)],
        with_io: bool,
    ) -> (Self, Option<TargetIo>) {
        Self::spawn_with_state(
            target,
            environment,
            with_io,
            TempDir::new().expect("state directory"),
        )
    }

    pub fn spawn_with_state(
        target: &[OsString],
        environment: &[(&str, &str)],
        with_io: bool,
        state_dir: TempDir,
    ) -> (Self, Option<TargetIo>) {
        let environment = environment
            .iter()
            .map(|(key, value)| (OsString::from(*key), OsString::from(*value)))
            .collect::<Vec<_>>();
        Self::spawn_with_state_and_runner(
            target,
            &environment,
            with_io,
            state_dir,
            RunnerExecutable::packaged(),
            /*ignored_signal*/ None,
        )
    }

    pub fn spawn_with_native_environment(
        target: &[OsString],
        environment: &[(OsString, OsString)],
        with_io: bool,
    ) -> (Self, Option<TargetIo>) {
        Self::spawn_with_state_and_runner(
            target,
            environment,
            with_io,
            TempDir::new().expect("state directory"),
            RunnerExecutable::packaged(),
            /*ignored_signal*/ None,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn spawn_without_companion(
        target: &[OsString],
        environment: &[(&str, &str)],
        with_io: bool,
    ) -> (Self, Option<TargetIo>) {
        let environment = environment
            .iter()
            .map(|(key, value)| (OsString::from(*key), OsString::from(*value)))
            .collect::<Vec<_>>();
        Self::spawn_with_state_and_runner(
            target,
            &environment,
            with_io,
            TempDir::new().expect("state directory"),
            RunnerExecutable::stage_linux(/*include_bwrap*/ false),
            None,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn spawn_with_incompatible_companion(
        target: &[OsString],
        environment: &[(&str, &str)],
        with_io: bool,
    ) -> (Self, Option<TargetIo>) {
        let environment = environment
            .iter()
            .map(|(key, value)| (OsString::from(*key), OsString::from(*value)))
            .collect::<Vec<_>>();
        let companion = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        Self::spawn_with_state_and_runner(
            target,
            &environment,
            with_io,
            TempDir::new().expect("state directory"),
            RunnerExecutable::stage_linux_with_companion(Some(&companion)),
            None,
        )
    }

    #[cfg(target_os = "macos")]
    pub fn spawn_with_ignored_signal(
        target: &[OsString],
        signal: libc::c_int,
    ) -> (Self, Option<TargetIo>) {
        Self::spawn_with_state_and_runner(
            target,
            &[],
            /*with_io*/ true,
            TempDir::new().expect("state directory"),
            RunnerExecutable::packaged(),
            Some(signal),
        )
    }

    fn spawn_with_state_and_runner(
        target: &[OsString],
        environment: &[(OsString, OsString)],
        with_io: bool,
        state_dir: TempDir,
        runner_executable: RunnerExecutable,
        ignored_signal: Option<libc::c_int>,
    ) -> (Self, Option<TargetIo>) {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let cleanup_dir = TempDir::new().expect("cleanup directory");
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--cleanup-dir")
            .arg(cleanup_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string());
        apply_sanitized_native_environment(&mut command, environment);
        let (child_stream_fds, parent_io, child_streams) = if with_io {
            let (parent_stdin, child_stdin) = UnixStream::pair().expect("stdin pair");
            let (child_stdout, parent_stdout) = UnixStream::pair().expect("stdout pair");
            let (child_stderr, parent_stderr) = UnixStream::pair().expect("stderr pair");
            (
                Some((
                    child_stdin.as_raw_fd(),
                    child_stdout.as_raw_fd(),
                    child_stderr.as_raw_fd(),
                )),
                Some(TargetIo {
                    stdin: parent_stdin,
                    stdout: parent_stdout,
                    stderr: parent_stderr,
                }),
                Some((child_stdin, child_stdout, child_stderr)),
            )
        } else {
            (None, None, None)
        };
        if with_io {
            for descriptor in [TARGET_STDIN_FD, TARGET_STDOUT_FD, TARGET_STDERR_FD] {
                command.arg("--stream-fd").arg(descriptor.to_string());
            }
        }
        if !target.is_empty() {
            command.arg("--").args(target);
        }
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some((stdin, stdout, stderr)) = child_stream_fds {
                    for (source, destination) in [
                        (stdin, TARGET_STDIN_FD),
                        (stdout, TARGET_STDOUT_FD),
                        (stderr, TARGET_STDERR_FD),
                    ] {
                        if libc::dup2(source, destination) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                }
                if let Some(signal) = ignored_signal
                    && libc::signal(signal, libc::SIG_IGN) == libc::SIG_ERR
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop(child_control);
        drop(child_streams);
        (
            Self {
                _process_id: child.id(),
                child,
                control,
                state_dir,
                _cleanup_dir: cleanup_dir,
                _runner_executable: runner_executable,
            },
            parent_io,
        )
    }

    pub fn spawn_with_passed_pty(target: &[OsString]) -> (Self, std::fs::File) {
        let (master, slave) = open_pty();
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let slave_fd = slave.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let cleanup_dir = TempDir::new().expect("cleanup directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--cleanup-dir")
            .arg(cleanup_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--stream-fd")
            .arg(TARGET_STDIN_FD.to_string())
            .arg("--stream-fd")
            .arg(TARGET_STDOUT_FD.to_string())
            .arg("--stream-fd")
            .arg(TARGET_STDERR_FD.to_string())
            .arg("--")
            .args(target);
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                for destination in [TARGET_STDIN_FD, TARGET_STDOUT_FD, TARGET_STDERR_FD] {
                    if libc::dup2(slave_fd, destination) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop((child_control, slave));
        (
            Self {
                _process_id: child.id(),
                child,
                control,
                state_dir,
                _cleanup_dir: cleanup_dir,
                _runner_executable: runner_executable,
            },
            master,
        )
    }

    pub fn spawn_with_inherited_pty(target: &[OsString]) -> (Self, std::fs::File) {
        let (master, slave) = open_pty();
        configure_interactive_pty(slave.as_raw_fd());
        let stdin = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let stdout = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let stderr = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let cleanup_dir = TempDir::new().expect("cleanup directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--cleanup-dir")
            .arg(cleanup_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--")
            .args(target)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                #[allow(clippy::cast_lossless)]
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop((child_control, slave));
        (
            Self {
                _process_id: child.id(),
                child,
                control,
                state_dir,
                _cleanup_dir: cleanup_dir,
                _runner_executable: runner_executable,
            },
            master,
        )
    }

    #[cfg(target_os = "macos")]
    pub fn spawn_with_surviving_pty_launcher(
        target: &[OsString],
    ) -> (Self, std::fs::File, libc::pid_t, std::fs::File) {
        let (master, slave) = open_pty();
        configure_interactive_pty(slave.as_raw_fd());
        let stdin = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let stdout = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let stderr = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let (pid_reader, pid_writer) = raw_pipe();
        let (launcher_release_reader, launcher_release) = raw_pipe();
        let pid_writer_fd = pid_writer.as_raw_fd();
        let launcher_release_reader_fd = launcher_release_reader.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let cleanup_dir = TempDir::new().expect("cleanup directory");
        let runner_executable = RunnerExecutable::packaged();
        let launcher = cargo_bin("mcp-console-sandbox-fixture").expect("fixture binary");
        let mut command = Command::new(launcher);
        command
            .arg("pty-runner-launcher")
            .arg(runner_executable.path())
            .arg(pid_writer_fd.to_string())
            .arg(launcher_release_reader_fd.to_string())
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--")
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--cleanup-dir")
            .arg(cleanup_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--")
            .args(target)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                #[allow(clippy::cast_lossless)]
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner launcher");
        let launcher_process_group = child.id() as libc::pid_t;
        drop((child_control, slave, pid_writer, launcher_release_reader));
        let mut pid_reader = std::fs::File::from(pid_reader);
        let mut process_id = [0_u8; 4];
        pid_reader
            .read_exact(&mut process_id)
            .expect("read private runner process identifier");
        let process_id = u32::from_be_bytes(process_id);
        (
            Self {
                _process_id: process_id,
                child,
                control,
                state_dir,
                _cleanup_dir: cleanup_dir,
                _runner_executable: runner_executable,
            },
            master,
            launcher_process_group,
            std::fs::File::from(launcher_release),
        )
    }

    pub fn spawn_with_inherited_output(target: &[OsString]) -> (Self, UnixStream, UnixStream) {
        let (child_stdout, parent_stdout) = UnixStream::pair().expect("stdout pair");
        let (child_stderr, parent_stderr) = UnixStream::pair().expect("stderr pair");
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let cleanup_dir = TempDir::new().expect("cleanup directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--cleanup-dir")
            .arg(cleanup_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--")
            .args(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                child_stdout,
            )))
            .stderr(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                child_stderr,
            )));
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop(child_control);
        (
            Self {
                _process_id: child.id(),
                child,
                control,
                state_dir,
                _cleanup_dir: cleanup_dir,
                _runner_executable: runner_executable,
            },
            parent_stdout,
            parent_stderr,
        )
    }

    pub fn spawn_with_inherited_stdin_and_stdout(
        target: &[OsString],
    ) -> (Self, UnixStream, UnixStream) {
        let (parent_stdin, child_stdin) = UnixStream::pair().expect("stdin pair");
        let (child_stdout, parent_stdout) = UnixStream::pair().expect("stdout pair");
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let cleanup_dir = TempDir::new().expect("cleanup directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--cleanup-dir")
            .arg(cleanup_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--")
            .args(target)
            .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                child_stdin,
            )))
            .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                child_stdout,
            )))
            .stderr(std::process::Stdio::null());
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop(child_control);
        (
            Self {
                _process_id: child.id(),
                child,
                control,
                state_dir,
                _cleanup_dir: cleanup_dir,
                _runner_executable: runner_executable,
            },
            parent_stdin,
            parent_stdout,
        )
    }

    pub fn spawn_with_control_on_stdout(target: &[OsString]) -> Self {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let state_dir = TempDir::new().expect("state directory");
        let cleanup_dir = TempDir::new().expect("cleanup directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--cleanup-dir")
            .arg(cleanup_dir.path())
            .arg("--control-fd")
            .arg(libc::STDOUT_FILENO.to_string())
            .arg("--")
            .args(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                child_control,
            )))
            .stderr(std::process::Stdio::null());
        apply_sanitized_environment(&mut command, &[]);
        let child = command.spawn().expect("spawn runner");
        Self {
            _process_id: child.id(),
            child,
            control,
            state_dir,
            _cleanup_dir: cleanup_dir,
            _runner_executable: runner_executable,
        }
    }

    pub fn request(&mut self, request: Value) -> Value {
        write_json_frame(&mut self.control, request);
        read_json_frame(&mut self.control)
    }

    pub fn state_dir(&self) -> &Path {
        self.state_dir.path()
    }

    pub fn runner_path(&self) -> &Path {
        self._runner_executable.path()
    }

    #[cfg(target_os = "macos")]
    pub fn cleanup_dir(&self) -> &Path {
        self._cleanup_dir.path()
    }

    #[cfg(target_os = "macos")]
    pub fn process_id(&self) -> u32 {
        self._process_id
    }

    #[cfg(target_os = "macos")]
    pub fn signal(&self, signal: libc::c_int) {
        assert_eq!(
            unsafe { libc::kill(self._process_id as libc::pid_t, signal) },
            0,
            "signal runner: {}",
            std::io::Error::last_os_error()
        );
    }

    pub fn close_control(&mut self) {
        self.control
            .shutdown(std::net::Shutdown::Both)
            .expect("close control channel");
    }

    pub fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        self.child.wait().expect("wait for runner")
    }
}

pub fn open_pty() -> (std::fs::File, std::fs::File) {
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    (unsafe { std::fs::File::from_raw_fd(master) }, unsafe {
        std::fs::File::from_raw_fd(slave)
    })
}

#[cfg(target_os = "macos")]
fn raw_pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    let mut descriptors = [-1_i32; 2];
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(descriptors[0]),
            std::os::fd::OwnedFd::from_raw_fd(descriptors[1]),
        )
    }
}

fn configure_interactive_pty(descriptor: i32) {
    let mut terminal = std::mem::MaybeUninit::<libc::termios>::zeroed();
    assert_eq!(
        unsafe { libc::tcgetattr(descriptor, terminal.as_mut_ptr()) },
        0
    );
    let mut terminal = unsafe { terminal.assume_init() };
    terminal.c_lflag &= !(libc::ECHO | libc::ICANON);
    terminal.c_cc[libc::VMIN] = 1;
    terminal.c_cc[libc::VTIME] = 0;
    assert_eq!(
        unsafe { libc::tcsetattr(descriptor, libc::TCSANOW, &terminal) },
        0
    );
}

impl Drop for Runner {
    fn drop(&mut self) {
        drop(self.control.shutdown(std::net::Shutdown::Both));
        if self
            .child
            .try_wait()
            .expect("query runner status")
            .is_none()
        {
            self.child.kill().expect("kill runner");
        }
        self.child.wait().expect("wait for runner");
    }
}

pub fn fixture_target(arguments: &[impl AsRef<std::ffi::OsStr>]) -> Vec<OsString> {
    std::iter::once(
        cargo_bin("mcp-console-sandbox-fixture")
            .expect("fixture binary")
            .into_os_string(),
    )
    .chain(
        arguments
            .iter()
            .map(|argument| argument.as_ref().to_os_string()),
    )
    .collect()
}

pub fn launch_request(
    id: u64,
    working_directory: &Path,
    filesystem_base: &str,
    filesystem_rules: Value,
    network: Value,
    streams: Value,
    lifecycle: Value,
) -> Value {
    json!({
        "type": "launch",
        "id": id,
        "protocol_version": PROTOCOL_VERSION,
        "launch": {
            "working_directory": working_directory,
            "policy_base_directory": working_directory,
            "filesystem": {
                "base": filesystem_base,
                "rules": filesystem_rules,
            },
            "network": network,
            "streams": streams,
            "terminal": "preserve",
            "lifecycle": lifecycle,
            "platform_extensions": {},
        },
    })
}

pub fn default_lifecycle() -> Value {
    let terminate_grace_ms = if cfg!(target_os = "linux") { 0 } else { 20 };
    json!({
        "kind": "command",
        "root_exit_grace_ms": 20,
        "terminate_grace_ms": terminate_grace_ms,
        "force_timeout_ms": 2000,
    })
}

pub fn passed_streams() -> Value {
    json!({
        "stdin": { "mode": "passed_handle", "handle": TARGET_STDIN_FD },
        "stdout": { "mode": "passed_handle", "handle": TARGET_STDOUT_FD },
        "stderr": { "mode": "passed_handle", "handle": TARGET_STDERR_FD },
    })
}

pub fn null_streams() -> Value {
    json!({
        "stdin": { "mode": "null" },
        "stdout": { "mode": "null" },
        "stderr": { "mode": "null" },
    })
}

pub fn wait_request(id: u64) -> Value {
    json!({
        "type": "wait",
        "id": id,
        "protocol_version": PROTOCOL_VERSION,
        "retirement_timeout_ms": 5000,
    })
}

pub fn write_json_frame(stream: &mut impl Write, value: Value) {
    let payload = serde_json::to_vec(&value).expect("serialize request");
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .expect("write frame length");
    stream.write_all(&payload).expect("write frame payload");
}

pub fn read_json_frame(stream: &mut impl Read) -> Value {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).expect("read frame length");
    let mut payload = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).expect("read frame payload");
    serde_json::from_slice(&payload).expect("parse response")
}
