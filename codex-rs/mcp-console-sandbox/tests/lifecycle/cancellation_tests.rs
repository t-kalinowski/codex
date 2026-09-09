use super::*;
use pretty_assertions::assert_eq;
use std::io::BufRead;
use std::io::BufReader;
use std::time::Duration;
use std::time::Instant;

fn cancel_setup(stage: &str) {
    for caller_death in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let library = startup::interposer(directory.path());
        let marker = directory.path().join("target-ran");
        let mut request = fixture("write", &[marker.to_str().unwrap()]);
        request["filesystem"]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path": {"type": "path", "path": directory.path()}, "access": "write"}));
        request["lifecycle"] = json!({"sigterm": "retire", "private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
        let (mut events, event_writer) = std::io::pipe().unwrap();
        let (release, mut release_writer) = std::io::pipe().unwrap();
        let native_runner = runner(directory.path());
        let mut command = if caller_death {
            let mut owner = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap());
            owner
                .arg("owner")
                .arg(native_runner.get_program())
                .env("SANDBOX_TEST_REQUEST", request.to_string())
                .env("SANDBOX_TEST_RUNNER_PRELOAD", &library);
            owner
        } else {
            native_runner
        };
        if !caller_death {
            startup::preload(&mut command, &library);
        }
        command
            .env("SANDBOX_TEST_SETUP_STAGE", stage)
            .env(
                "SANDBOX_TEST_EVENT_FD",
                event_writer.as_raw_fd().to_string(),
            )
            .env("SANDBOX_TEST_RELEASE_FD", release.as_raw_fd().to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        startup::inherit(
            &mut command,
            &[event_writer.as_raw_fd(), release.as_raw_fd()],
        );
        let mut child = if caller_death {
            command.spawn().unwrap()
        } else {
            let (child, mut writer) = spawn(&mut command);
            writer.write_all(&frame(&request)).unwrap();
            child
        };
        drop(command);
        drop(event_writer);
        drop(release);
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let supervisor = if caller_death {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            line.trim().parse::<i32>().unwrap()
        } else {
            child.id() as i32
        };
        let mut bytes = [0; 4];
        events.read_exact(&mut bytes).unwrap();
        let native = i32::from_ne_bytes(bytes);
        assert!(native > 0);
        if caller_death {
            child.kill().unwrap();
            child.wait().unwrap();
        } else {
            assert_eq!(unsafe { libc::kill(supervisor, libc::SIGTERM) }, 0);
        }
        release_writer.write_all(b"x").unwrap();
        // Only descriptor closure satisfies completion; a partial setup reader
        // must not keep either stream alive, even while stopped by the fixture.
        let mut fds = [
            libc::pollfd {
                fd: stdout.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: child.stderr.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let deadline = Instant::now() + Duration::from_secs(5);
        while fds.iter().any(|fd| fd.fd >= 0) && Instant::now() < deadline {
            assert!(unsafe { libc::poll(fds.as_mut_ptr(), 2, 10) } >= 0);
            for fd in &mut fds {
                if fd.revents & libc::POLLHUP != 0 {
                    fd.fd = -1;
                }
            }
        }
        let completed = fds.iter().all(|fd| fd.fd == -1);
        if !completed {
            unsafe {
                libc::kill(native, libc::SIGKILL);
                libc::kill(supervisor, libc::SIGKILL);
            }
        }
        let mut rest = Vec::new();
        stdout.read_to_end(&mut rest).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            completed,
            "{stage}, caller_death={caller_death}: cancellation did not close streams: {output:?}; target executed: {}",
            marker.exists()
        );
        assert!(
            !marker.exists(),
            "{stage}, caller_death={caller_death}: target executed after cancellation"
        );
        assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("sandbox-")
        }));
        assert!(rest.is_empty(), "{rest:?}");
        if !caller_death {
            assert_eq!(output.status.code(), Some(0), "{output:?}");
        }
        assert_eq!(
            output.stderr, b"",
            "{stage}, caller_death={caller_death}: {output:?}"
        );
    }
}

#[test]
fn cancellation_at_native_readiness_prevents_target_release() {
    cancel_setup("ready");
}

#[test]
fn cancellation_during_partial_setup_retires_reader_before_closing_gate() {
    cancel_setup("partial");
}

#[test]
fn cancellation_retires_a_stopped_partial_setup_reader() {
    cancel_setup("blocked");
}
