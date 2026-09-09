use super::*;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::os::fd::FromRawFd;

#[test]
fn controlling_terminal_delivers_interrupt_once() {
    terminal_case("exclusive");
}

#[cfg(target_os = "macos")]
#[test]
fn foreground_peer_keeps_terminal_ownership() {
    terminal_case("peer");
}

fn terminal_case(kind: &str) {
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
