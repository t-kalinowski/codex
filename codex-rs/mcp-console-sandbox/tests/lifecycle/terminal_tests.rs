use super::*;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::os::fd::FromRawFd;

#[test]
fn controlling_terminal_delivers_interrupt_once() {
    for filesystem in ["restricted", "unrestricted", "external-sandbox"] {
        terminal_case("exclusive", filesystem);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn foreground_peer_keeps_terminal_ownership() {
    for filesystem in ["restricted", "unrestricted", "external-sandbox"] {
        terminal_case("peer", filesystem);
    }
}

fn terminal_case(kind: &str, filesystem: &str) {
    let directory = tempfile::tempdir().unwrap();
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
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let mut request = fixture("terminal", &[kind]);
    request["filesystem"]["kind"] = json!(filesystem);
    for field in ["command", "cwd", "environment"] {
        request.as_object_mut().unwrap().remove(field);
    }
    let native = runner(directory.path());
    let mut command = if kind == "peer" {
        let mut wrapper = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap());
        wrapper.arg("foreground-peer").arg(native.get_program());
        wrapper
    } else {
        Command::new(native.get_program())
    };
    command
        .args(["--config-env", "SANDBOX_TEST_CONFIG", "--"])
        .arg(cargo_bin("mcp-console-sandbox-fixture").unwrap())
        .args(["terminal", kind])
        .env("SANDBOX_TEST_CONFIG", request.to_string())
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let _runner_watch = ownership::Process::watch(child.id() as i32);
    drop(command);
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let peer = if kind == "peer" {
        stdout.read_line(&mut line).unwrap();
        let pid = line.trim().parse::<i32>().unwrap();
        line.clear();
        Some(pid)
    } else {
        None
    };
    stdout.read_line(&mut line).unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    #[cfg(target_os = "linux")]
    let _target_watch = ownership::watch_tree(child.id() as i32);
    #[cfg(target_os = "macos")]
    let _target_watch = ownership::Process::watch(ready["pid"].as_i64().unwrap() as i32);
    let foreground = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
    #[cfg(target_os = "macos")]
    assert_eq!(
        foreground,
        if kind == "peer" {
            child.id() as i32
        } else {
            ready["group"].as_i64().unwrap() as i32
        }
    );
    #[cfg(target_os = "linux")]
    assert_eq!(foreground, child.id() as i32);
    if kind == "exclusive" {
        master.write_all(b"terminal input\n").unwrap();
        let mut readable = libc::pollfd {
            fd: stdout.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut readable, 1, 5000) },
            1,
            "terminal input stalled for {filesystem}: {ready}"
        );
        line.clear();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line, "terminal input\n");
    }
    master.write_all(&[3]).unwrap();
    line.clear();
    stdout.read_to_string(&mut line).unwrap();
    let output = child.wait_with_output().unwrap();
    if let Some(pid) = peer {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert_eq!(output.status.code(), Some(42), "{output:?}, {ready}");
    assert_eq!(line, "1\n");
}

#[test]
fn parent_owned_terminal_input_or_output_keeps_callers_group() {
    for terminal_fd in [0, 1] {
        let directory = tempfile::tempdir().unwrap();
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
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut command = runner(directory.path());
        command.stdin(Stdio::piped());
        if terminal_fd == 0 {
            command.stdin(slave);
        } else {
            command.stdout(slave);
        }
        let mut request = fixture("terminal", &["peer"]);
        request["lifecycle"] = json!({"parent_pid": std::process::id()});
        let (mut child, mut bootstrap) = spawn(&mut command);
        let _runner_watch = ownership::Process::watch(child.id() as i32);
        drop(command);
        bootstrap.write_all(&frame(&request)).unwrap();
        let mut stdout: Box<dyn BufRead> = if terminal_fd == 0 {
            Box::new(BufReader::new(child.stdout.take().unwrap()))
        } else {
            Box::new(BufReader::new(master))
        };
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        #[cfg(target_os = "macos")]
        let _target_watch = ownership::Process::watch(ready["pid"].as_i64().unwrap() as i32);
        #[cfg(target_os = "linux")]
        let _target_watch = ownership::watch_tree(child.id() as i32);
        assert!(ready["pid"].as_i64().unwrap() > 0);
        let group = unsafe { libc::getpgid(child.id() as i32) };
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(42), "{output:?}");
        assert_eq!(group, unsafe { libc::getpgrp() });
    }
}
