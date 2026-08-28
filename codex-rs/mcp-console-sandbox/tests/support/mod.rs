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
    child: Child,
    pub control: UnixStream,
    state_dir: TempDir,
    _runner_executable: RunnerExecutable,
}

pub struct TargetIo {
    pub stdin: UnixStream,
    pub stdout: UnixStream,
    pub stderr: UnixStream,
}

#[derive(Clone, Copy)]
enum RunnerProcessGroup {
    Inherit,
    New,
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
            RunnerProcessGroup::Inherit,
        )
    }

    pub fn spawn_in_new_process_group(
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
            RunnerExecutable::packaged(),
            RunnerProcessGroup::New,
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
            RunnerProcessGroup::Inherit,
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
            RunnerProcessGroup::Inherit,
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
            RunnerProcessGroup::Inherit,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn spawn_with_companion(
        target: &[OsString],
        companion: &Path,
        with_io: bool,
    ) -> (Self, Option<TargetIo>) {
        Self::spawn_with_state_and_runner(
            target,
            &[],
            with_io,
            TempDir::new().expect("state directory"),
            RunnerExecutable::stage_linux_with_companion(Some(companion)),
            RunnerProcessGroup::Inherit,
        )
    }

    fn spawn_with_state_and_runner(
        target: &[OsString],
        environment: &[(OsString, OsString)],
        with_io: bool,
        state_dir: TempDir,
        runner_executable: RunnerExecutable,
        process_group: RunnerProcessGroup,
    ) -> (Self, Option<TargetIo>) {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string());
        apply_sanitized_native_environment(&mut command, environment);
        match process_group {
            RunnerProcessGroup::Inherit => {}
            RunnerProcessGroup::New => {
                command.process_group(0);
            }
        }
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
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop(child_control);
        drop(child_streams);
        (
            Self {
                child,
                control,
                state_dir,
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
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
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
                child,
                control,
                state_dir,
                _runner_executable: runner_executable,
            },
            master,
        )
    }

    pub fn spawn_with_inherited_pty(target: &[OsString]) -> (Self, std::fs::File) {
        let (master, slave) = open_pty();
        let stdin = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let stdout = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let stderr = unsafe { std::fs::File::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
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
                child,
                control,
                state_dir,
                _runner_executable: runner_executable,
            },
            master,
        )
    }

    pub fn spawn_with_inherited_output(target: &[OsString]) -> (Self, UnixStream, UnixStream) {
        let (child_stdout, parent_stdout) = UnixStream::pair().expect("stdout pair");
        let (child_stderr, parent_stderr) = UnixStream::pair().expect("stderr pair");
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
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
                child,
                control,
                state_dir,
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
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
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
                child,
                control,
                state_dir,
                _runner_executable: runner_executable,
            },
            parent_stdin,
            parent_stdout,
        )
    }

    pub fn spawn_with_closed_standard_stream(target: &[OsString], descriptor: i32) -> Self {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--")
            .args(target);
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::close(descriptor) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop(child_control);
        Self {
            child,
            control,
            state_dir,
            _runner_executable: runner_executable,
        }
    }

    pub fn spawn_with_duplicated_control_as_passed_stream(target: &[OsString]) -> Self {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--stream-fd")
            .arg(TARGET_STDOUT_FD.to_string())
            .arg("--")
            .args(target);
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                for destination in [CHILD_CONTROL_FD, TARGET_STDOUT_FD] {
                    if libc::dup2(child_control_fd, destination) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop(child_control);
        Self {
            child,
            control,
            state_dir,
            _runner_executable: runner_executable,
        }
    }

    pub fn spawn_with_duplicated_stdout_as_passed_stream(target: &[OsString]) -> Self {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let state_dir = TempDir::new().expect("state directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--stream-fd")
            .arg(TARGET_STDOUT_FD.to_string())
            .arg("--")
            .args(target);
        apply_sanitized_environment(&mut command, &[]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_control_fd, CHILD_CONTROL_FD) == -1
                    || libc::dup2(libc::STDOUT_FILENO, TARGET_STDOUT_FD) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn runner");
        drop(child_control);
        Self {
            child,
            control,
            state_dir,
            _runner_executable: runner_executable,
        }
    }

    pub fn spawn_with_duplicated_control_as_inherited_stdout(target: &[OsString]) -> Self {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let child_control_fd = child_control.as_raw_fd();
        let child_stdout = child_control
            .try_clone()
            .expect("duplicate control endpoint");
        let state_dir = TempDir::new().expect("state directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--control-fd")
            .arg(CHILD_CONTROL_FD.to_string())
            .arg("--")
            .args(target)
            .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                child_stdout,
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
        Self {
            child,
            control,
            state_dir,
            _runner_executable: runner_executable,
        }
    }

    pub fn spawn_with_control_on_stdout(target: &[OsString]) -> Self {
        let (control, child_control) = UnixStream::pair().expect("control socket pair");
        let state_dir = TempDir::new().expect("state directory");
        let runner_executable = RunnerExecutable::packaged();
        let mut command = Command::new(runner_executable.path());
        command
            .arg("--state-dir")
            .arg(state_dir.path())
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
            child,
            control,
            state_dir,
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

    pub fn process_id(&self) -> u32 {
        self.child.id()
    }

    pub fn close_control(&mut self) {
        self.control
            .shutdown(std::net::Shutdown::Both)
            .expect("close control channel");
    }

    pub fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        self.child.wait().expect("wait for runner")
    }

    pub fn kill(&mut self) {
        self.child.kill().expect("kill runner process");
    }
}

fn open_pty() -> (std::fs::File, std::fs::File) {
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
