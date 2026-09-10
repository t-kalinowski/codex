use super::*;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

// The test process stays outside the killed group. Pin each observed lifetime
// before killing its owner, and clean up even when an assertion unwinds.
pub(super) struct Process {
    pid: i32,
    exit: OwnedFd,
}
impl Process {
    pub(super) fn watch(pid: i32) -> Self {
        #[cfg(target_os = "linux")]
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        #[cfg(target_os = "macos")]
        let fd = unsafe { libc::kqueue() };
        assert!(fd >= 0, "watch {pid}: {}", std::io::Error::last_os_error());
        let exit = unsafe { OwnedFd::from_raw_fd(fd) };
        #[cfg(target_os = "macos")]
        {
            let event = libc::kevent {
                ident: pid as usize,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            assert_eq!(
                unsafe { libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) },
                0
            );
        }
        Self { pid, exit }
    }

    fn kill(&self) {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.exit.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
        #[cfg(target_os = "macos")]
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let mut fd = libc::pollfd {
            fd: self.exit.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut fd, 1, 0) } == 0 {
            self.kill();
        }
    }
}

pub(super) fn await_exit(processes: &[Process]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    for process in processes {
        let mut fd = libc::pollfd {
            fd: process.exit.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as i32;
        assert_eq!(
            unsafe { libc::poll(&mut fd, 1, remaining) },
            1,
            "process {} survived",
            process.pid
        );
        assert_ne!(fd.revents & libc::POLLIN, 0);
    }
}

#[cfg(target_os = "linux")]
pub(super) fn watch_tree(root: i32) -> Vec<Process> {
    let mut processes = vec![Process::watch(root)];
    let mut index = 0;
    while index < processes.len() {
        for task in std::fs::read_dir(format!("/proc/{}/task", processes[index].pid)).unwrap() {
            let children = std::fs::read_to_string(task.unwrap().path().join("children")).unwrap();
            for pid in children
                .split_whitespace()
                .map(|pid| pid.parse::<i32>().unwrap())
            {
                if !processes.iter().any(|process| process.pid == pid) {
                    processes.push(Process::watch(pid));
                }
            }
        }
        index += 1;
    }
    processes
}

#[derive(Clone, Copy, Debug)]
enum Loss {
    Caller,
    Group,
    #[cfg(target_os = "linux")]
    Runner,
}

#[test]
fn caller_sigkill_retires_workload_and_storage() {
    owned_loss(Loss::Caller);
}

#[test]
fn caller_group_sigkill_with_terminal_stderr_retires_workload_and_storage() {
    owned_loss(Loss::Group);
}

#[cfg(target_os = "linux")]
#[test]
fn runner_sigkill_terminates_native_chain_and_detached_workload() {
    owned_loss(Loss::Runner);
}

fn owned_loss(loss: Loss) {
    #[cfg(target_os = "linux")]
    let paths = [String::new(), std::env::var("PATH").unwrap()];
    #[cfg(target_os = "macos")]
    let paths = [String::new()];
    for path in paths {
        for proxy in [Value::Null, proxy_config()] {
            eprintln!("loss={loss:?}, path={path}, proxy={}", !proxy.is_null());
            let directory = tempfile::tempdir().unwrap();
            let native = runner(directory.path());
            let mut request = fixture("lifecycle", &["detached"]);
            request["cwd"] = json!(directory.path());
            request["proxy"] = proxy;
            request["filesystem"]["entries"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "path": {"type": "path", "path": directory.path()}, "access": "write"
                }));
            request["lifecycle"] =
                json!({"private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
            let mut command = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap());
            command
                .arg("owner")
                .arg(native.get_program())
                // Helper selection uses the launch environment, before target setup.
                .env("PATH", &path)
                .env("SANDBOX_TEST_REQUEST", request.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let terminal = if matches!(loss, Loss::Group) {
                let (mut master, mut slave) = (-1, -1);
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
                let master = unsafe { File::from_raw_fd(master) };
                command.stderr(Stdio::from(unsafe { File::from_raw_fd(slave) }));
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() < 0 || libc::ioctl(2, libc::TIOCSCTTY as _, 0) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                Some(master)
            } else {
                command.process_group(0);
                None
            };
            let mut owner = command.spawn().unwrap();
            drop(command);
            let owner_watch = Process::watch(owner.id() as i32);
            let mut stdout = BufReader::new(owner.stdout.take().unwrap());
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            let supervisor = line.trim().parse::<i32>().unwrap();
            let runner_watch = Process::watch(supervisor);
            line.clear();
            stdout.read_line(&mut line).unwrap();
            let ready: Value = serde_json::from_str(&line).unwrap();
            #[cfg(target_os = "linux")]
            let processes = watch_tree(supervisor);
            #[cfg(target_os = "macos")]
            let processes = ["pid", "descendant"]
                .map(|key| Process::watch(ready[key].as_i64().unwrap() as i32));
            #[cfg(target_os = "linux")]
            {
                // Confirm that the namespace-local detached PID names a live
                // host process in this exact observed tree, outside its parent's session.
                let descendant = ready["descendant"].as_i64().unwrap();
                let detached = processes
                    .iter()
                    .find(|process| {
                        std::fs::read_to_string(format!("/proc/{}/status", process.pid))
                            .unwrap()
                            .lines()
                            .any(|line| {
                                line.starts_with("NSpid:")
                                    && line
                                        .split_whitespace()
                                        .last()
                                        .unwrap()
                                        .parse::<i64>()
                                        .unwrap()
                                        == descendant
                            })
                    })
                    .unwrap();
                assert_eq!(unsafe { libc::getsid(detached.pid) }, detached.pid);
                assert!(
                    processes.len() >= 5,
                    "native chain and workload: {}",
                    processes.len()
                );
            }
            assert_ne!(unsafe { libc::getpgrp() }, owner.id() as i32);
            if let Some(terminal) = &terminal {
                assert_eq!(
                    unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) },
                    owner.id() as i32
                );
            }
            // Capture before death; checking this after SIGKILL could mistake a
            // recycled PID or the runner's retirement for successful isolation.
            let group = unsafe { libc::getpgid(supervisor) };
            match loss {
                Loss::Caller => owner_watch.kill(),
                Loss::Group => assert_eq!(
                    unsafe { libc::kill(-(owner.id() as i32), libc::SIGKILL) },
                    0
                ),
                #[cfg(target_os = "linux")]
                Loss::Runner => runner_watch.kill(),
            }
            await_exit(&processes);
            await_exit(std::slice::from_ref(&runner_watch));
            owner.wait().unwrap();
            assert_eq!(group, supervisor, "{loss:?}: runner shared caller group");
            let temporary = Path::new(ready["temporary"].as_str().unwrap());
            #[cfg(target_os = "linux")]
            if matches!(loss, Loss::Runner) {
                assert!(temporary.exists());
                continue;
            }
            assert!(
                !temporary.exists(),
                "{loss:?}: storage retained; runner stderr: {:?}",
                owner
                    .stderr
                    .take()
                    .map(std::io::read_to_string)
                    .transpose()
                    .unwrap()
            );
            line.clear();
            stdout.read_to_string(&mut line).unwrap();
            assert!(line.is_empty(), "{line}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn runner_death_before_and_after_native_death_signal_setup() {
    for stage in ["before", "armed", "ready", "partial", "blocked"] {
        eprintln!("stage={stage}");
        let directory = tempfile::tempdir().unwrap();
        let library = startup::interposer(directory.path());
        let marker = directory.path().join("target-ran");
        let (mut events, event_writer) = std::io::pipe().unwrap();
        let (release, mut release_writer) = std::io::pipe().unwrap();
        let mut command = runner(directory.path());
        startup::preload(&mut command, &library);
        command
            .env(
                "SANDBOX_TEST_EVENT_FD",
                event_writer.as_raw_fd().to_string(),
            )
            .env("SANDBOX_TEST_RELEASE_FD", release.as_raw_fd().to_string());
        match stage {
            "before" | "armed" => {
                command.env("SANDBOX_TEST_CHILD_STAGE", stage);
            }
            _ => {
                command.env("SANDBOX_TEST_SETUP_STAGE", stage);
            }
        }
        startup::inherit(
            &mut command,
            &[event_writer.as_raw_fd(), release.as_raw_fd()],
        );
        let mut request = fixture("write", &[marker.to_str().unwrap()]);
        request["filesystem"]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "path": {"type": "path", "path": directory.path()}, "access": "write"
            }));
        request["proxy"] = proxy_config();
        request["lifecycle"] =
            json!({"private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
        let (mut child, mut bootstrap) = spawn(command.stdin(Stdio::piped()));
        drop((command, event_writer, release));
        let runner_watch = Process::watch(child.id() as i32);
        bootstrap.write_all(&frame(&request)).unwrap();
        let mut bytes = [0; 4];
        events.read_exact(&mut bytes).unwrap();
        let processes = watch_tree(child.id() as i32);
        assert!(
            processes
                .iter()
                .any(|process| process.pid == i32::from_ne_bytes(bytes))
        );
        runner_watch.kill();
        child.wait().unwrap();
        if stage == "before" {
            // Death before arming cannot deliver PDEATHSIG. Release the child
            // only after its creating parent has been reaped; the recheck must cancel exec.
            release_writer.write_all(b"x").unwrap();
        }
        await_exit(&processes);
        let output = child.wait_with_output().unwrap();
        assert!(output.stdout.is_empty(), "{stage}: {output:?}");
        assert!(!marker.exists(), "{stage}: target ran after owner death");
    }
}
