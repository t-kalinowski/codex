use super::*;
use pretty_assertions::assert_eq;

fn interposer(directory: &Path) -> std::path::PathBuf {
    let library = directory.join("interposer.dylib");
    let source = codex_utils_cargo_bin::find_resource!("tests/lifecycle/interposer.c").unwrap();
    let mut command = Command::new("cc");
    #[cfg(target_os = "macos")]
    command.arg("-dynamiclib");
    #[cfg(target_os = "linux")]
    command.args(["-shared", "-fPIC", "-ldl"]);
    let output = command
        .arg(source)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    library
}

fn preload(command: &mut Command, library: &Path) {
    #[cfg(target_os = "macos")]
    command.env("DYLD_INSERT_LIBRARIES", library);
    #[cfg(target_os = "linux")]
    command.env("LD_PRELOAD", library);
}

fn inherit(command: &mut Command, descriptors: &[i32]) {
    let descriptors = descriptors.to_vec();
    unsafe {
        command.pre_exec(move || {
            for &fd in &descriptors {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[test]
fn cancellation_after_native_spawn_keeps_target_gated_and_cleans_storage() {
    let directory = tempfile::tempdir().unwrap();
    let library = interposer(directory.path());
    let (mut events, event_writer) = std::io::pipe().unwrap();
    let (release, mut release_writer) = std::io::pipe().unwrap();
    let mut command = runner(directory.path());
    preload(&mut command, &library);
    command
        .env("SANDBOX_TEST_GATE_SPAWN", "1")
        .env(
            "SANDBOX_TEST_EVENT_FD",
            event_writer.as_raw_fd().to_string(),
        )
        .env("SANDBOX_TEST_RELEASE_FD", release.as_raw_fd().to_string());
    inherit(
        &mut command,
        &[event_writer.as_raw_fd(), release.as_raw_fd()],
    );
    let marker = directory.path().join("target-ran");
    let mut request = fixture("write", &[marker.to_str().unwrap()]);
    request["filesystem"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path": {"type": "path", "path": directory.path()}, "access": "write"}));
    request["lifecycle"] = json!({"sigterm": "retire", "private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let (child, mut bootstrap) = spawn(&mut command);
    drop(command);
    drop(event_writer);
    drop(release);
    bootstrap.write_all(&frame(&request)).unwrap();
    let mut bytes = [0; 4];
    events.read_exact(&mut bytes).unwrap();
    assert!(i32::from_ne_bytes(bytes) > 0);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    release_writer.write_all(b"x").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!marker.exists());
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("sandbox-")
    }));
}

#[test]
fn failed_private_directory_removal_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let library = interposer(directory.path());
    let mut command = runner(directory.path());
    preload(&mut command, &library);
    command.env("SANDBOX_TEST_FAIL_REMOVE", "1");
    let mut request = fixture("lifecycle", &["success"]);
    request["lifecycle"] =
        json!({"private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let output = run_command(command, frame(&request), &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("remove private storage"),
        "{output:?}"
    );
}

#[test]
fn target_loader_runs_only_after_native_enforcement() {
    let directory = tempfile::tempdir().unwrap();
    let library = interposer(directory.path());
    let marker = directory.path().join("loader-escaped");
    let mut request = fixture("context", &[]);
    #[cfg(target_os = "macos")]
    let variable = "DYLD_INSERT_LIBRARIES";
    #[cfg(target_os = "linux")]
    let variable = "LD_PRELOAD";
    request["environment"][variable] = json!(library);
    request["environment"]["SANDBOX_TEST_LOADER_MARKER"] = json!(marker);
    let output = run_command(runner(directory.path()), frame(&request), &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        output.stdout.starts_with(b"loader restricted\n"),
        "{output:?}"
    );
    assert!(!marker.exists());
}

#[test]
fn caller_death_during_native_startup_never_releases_target() {
    use std::io::BufRead;
    use std::io::BufReader;
    let directory = tempfile::tempdir().unwrap();
    let library = interposer(directory.path());
    let marker = directory.path().join("target-ran");
    let mut request = fixture("write", &[marker.to_str().unwrap()]);
    request["filesystem"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path": {"type": "path", "path": directory.path()}, "access": "write"}));
    request["lifecycle"] =
        json!({"private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let (mut events, event_writer) = std::io::pipe().unwrap();
    let (release, mut release_writer) = std::io::pipe().unwrap();
    let runner = runner(directory.path());
    let mut command = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap());
    command
        .arg("owner")
        .arg(runner.get_program())
        .env("SANDBOX_TEST_REQUEST", request.to_string())
        .env("SANDBOX_TEST_RUNNER_PRELOAD", library)
        .env("SANDBOX_TEST_GATE_SPAWN", "1")
        .env(
            "SANDBOX_TEST_EVENT_FD",
            event_writer.as_raw_fd().to_string(),
        )
        .env("SANDBOX_TEST_RELEASE_FD", release.as_raw_fd().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    inherit(
        &mut command,
        &[event_writer.as_raw_fd(), release.as_raw_fd()],
    );
    let mut owner = command.spawn().unwrap();
    let mut stdout = BufReader::new(owner.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    events.read_exact(&mut [0; 4]).unwrap();
    owner.kill().unwrap();
    owner.wait().unwrap();
    release_writer.write_all(b"x").unwrap();
    stdout.read_to_end(&mut Vec::new()).unwrap();
    assert!(!marker.exists());
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("sandbox-")
    }));
}

#[test]
fn cancellation_interrupts_incomplete_bootstrap_without_transport_eof() {
    let directory = tempfile::tempdir().unwrap();
    let library = interposer(directory.path());
    let (mut events, event_writer) = std::io::pipe().unwrap();
    let mut command = runner(directory.path());
    preload(&mut command, &library);
    command.env("SANDBOX_TEST_OBSERVE_POLL", "1").env(
        "SANDBOX_TEST_EVENT_FD",
        event_writer.as_raw_fd().to_string(),
    );
    inherit(&mut command, &[event_writer.as_raw_fd()]);
    let (child, mut writer) = spawn(&mut command);
    writer.write_all(&[0, 0]).unwrap();
    events.read_exact(&mut [0; 4]).unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("startup cancelled by signal"));
    drop(writer);
}

#[test]
fn normal_exit_retires_an_observed_detached_descendant() {
    use std::io::BufRead;
    use std::io::BufReader;
    let directory = tempfile::tempdir().unwrap();
    let mut command = runner(directory.path());
    #[cfg(target_os = "macos")]
    let (mut events, event_writer) = std::io::pipe().unwrap();
    #[cfg(target_os = "macos")]
    {
        let library = interposer(directory.path());
        preload(&mut command, &library);
        command.env("SANDBOX_TEST_OBSERVE_WATCH", "1").env(
            "SANDBOX_TEST_EVENT_FD",
            event_writer.as_raw_fd().to_string(),
        );
        inherit(&mut command, &[event_writer.as_raw_fd()]);
    }
    let mut request = fixture("lifecycle", &["normal-tree"]);
    request["lifecycle"] =
        json!({"private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let (mut child, mut bootstrap) = spawn(command.stdin(Stdio::piped()));
    bootstrap.write_all(&frame(&request)).unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    // Darwin's contract covers observed identities. Release the target only
    // after the real kernel watch for its detached child has been installed.
    #[cfg(target_os = "macos")]
    loop {
        let mut bytes = [0; 4];
        events.read_exact(&mut bytes).unwrap();
        if i32::from_ne_bytes(bytes) == ready["descendant"].as_i64().unwrap() as i32 {
            break;
        }
    }
    #[cfg(target_os = "macos")]
    let exit_watch = {
        use std::os::fd::FromRawFd;
        let queue = unsafe { libc::kqueue() };
        assert!(queue >= 0);
        let queue = unsafe { std::os::fd::OwnedFd::from_raw_fd(queue) };
        let event = libc::kevent {
            ident: ready["pid"].as_u64().unwrap() as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        assert_eq!(
            unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    &event,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            },
            0
        );
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGSTOP) }, 0);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(child.id() as i32, &mut status, libc::WUNTRACED) },
            child.id() as i32
        );
        assert!(libc::WIFSTOPPED(status));
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        queue
    };
    child.stdin.take().unwrap().write_all(b"exit").unwrap();
    #[cfg(target_os = "macos")]
    {
        let mut event = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 10,
            tv_nsec: 0,
        };
        let count = unsafe {
            libc::kevent(
                exit_watch.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        // Resume even when the assertion fails so failure cannot strand a stopped supervisor.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGCONT) }, 0);
        assert_eq!(count, 1);
        assert_ne!(event.fflags & libc::NOTE_EXIT, 0);
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(42), "{output:?}");
    assert!(!Path::new(ready["temporary"].as_str().unwrap()).exists());
}

#[cfg(target_os = "macos")]
#[test]
fn failed_termination_is_reported_and_storage_is_retained() {
    use std::io::BufRead;
    use std::io::BufReader;
    let directory = tempfile::tempdir().unwrap();
    let library = interposer(directory.path());
    let mut command = runner(directory.path());
    preload(&mut command, &library);
    command.env("SANDBOX_TEST_FAIL_KILL", "1");
    let mut request = fixture("lifecycle", &["detached"]);
    request["lifecycle"] = json!({"sigterm": "retire", "cleanup_timeout_ms": 100, "private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let (mut child, mut bootstrap) = spawn(command.stdin(Stdio::piped()));
    bootstrap.write_all(&frame(&request)).unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    let stdin = child.stdin.take();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert_eq!(child.wait().unwrap().code(), Some(1));
    assert!(Path::new(ready["temporary"].as_str().unwrap()).exists());
    for key in ["pid", "descendant"] {
        unsafe {
            libc::kill(ready[key].as_i64().unwrap() as i32, libc::SIGKILL);
        }
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("descendant retirement failed")
            && stderr.contains("private storage retained"),
        "{stderr}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn failed_descendant_discovery_is_reported_and_storage_is_retained() {
    let directory = tempfile::tempdir().unwrap();
    let library = interposer(directory.path());
    let mut command = runner(directory.path());
    preload(&mut command, &library);
    command.env("SANDBOX_TEST_FAIL_DISCOVERY", "1");
    let mut request = fixture("lifecycle", &["success"]);
    request["lifecycle"] = json!({"cleanup_timeout_ms": 100, "private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let output = run_command(command, frame(&request), &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("process discovery"),
        "{output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("private storage retained"),
        "{output:?}"
    );
    assert!(output.stdout.is_empty());
}
