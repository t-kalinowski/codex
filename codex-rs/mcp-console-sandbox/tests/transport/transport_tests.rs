use super::*;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

#[test]
fn prequeued_stdin_can_itself_be_a_legacy_bootstrap_frame() {
    let mut legacy = request(&["/bin/echo", "wrong-command"]);
    legacy["version"] = json!(1);
    let input = [frame(&legacy), (0..=255).collect()].concat();
    let (read, mut write) = std::io::pipe().unwrap();
    // This fits in an anonymous pipe and is queued before process creation.
    write.write_all(&input).unwrap();
    drop(write);
    let staging = tempfile::tempdir().unwrap();
    let (child, mut bootstrap) = spawn(runner(staging.path()).stdin(Stdio::from(read)));
    bootstrap
        .write_all(&frame(&fixture("stdout", &[])))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!((output.stdout, output.stderr), (input, vec![]));
}

#[test]
fn regular_file_stdin_preserves_offset_seekability_and_shared_description() {
    let input = b"unused-prefix\0remaining\xff";
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(input).unwrap();
    file.seek(SeekFrom::Start(13)).unwrap();
    let staging = tempfile::tempdir().unwrap();
    let (child, mut bootstrap) =
        spawn(runner(staging.path()).stdin(Stdio::from(file.try_clone().unwrap())));
    bootstrap
        .write_all(&frame(&fixture("stdin-file", &[])))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"offset": 13, "tail": input[13..]})
    );
    assert_eq!(file.stream_position().unwrap(), 1);
    assert_eq!(output.stderr, b"");
}

#[test]
fn terminal_stdin_preserves_its_device_identity() {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty initializes both descriptors with default terminal settings.
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
    // SAFETY: these are the two new descriptors owned solely by this test.
    let (_master, slave) = unsafe { (OwnedFd::from_raw_fd(master), File::from_raw_fd(slave)) };
    let expected = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap())
        .arg("stdin-identity")
        .stdin(slave.try_clone().unwrap())
        .output()
        .unwrap();
    assert!(expected.status.success(), "{expected:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&expected.stdout).unwrap()["tty"],
        true
    );
    let staging = tempfile::tempdir().unwrap();
    let (child, mut bootstrap) = spawn(runner(staging.path()).stdin(slave));
    bootstrap
        .write_all(&frame(&fixture("stdin-identity", &[])))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output, expected);
}

#[test]
fn null_and_closed_stdin_follow_native_runtime_behavior() {
    for closed in [false, true] {
        let mut native = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap());
        native.arg("stdin-identity").stdin(Stdio::null());
        let staging = tempfile::tempdir().unwrap();
        let mut command = runner(staging.path());
        command.stdin(Stdio::null());
        if closed {
            for command in [&mut native, &mut command] {
                // SAFETY: only close the child's stdin immediately before exec.
                unsafe {
                    command.pre_exec(|| {
                        if libc::close(libc::STDIN_FILENO) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
        }
        let expected = native.output().unwrap();
        assert!(expected.status.success(), "{expected:?}");
        let (child, mut bootstrap) = spawn(&mut command);
        bootstrap
            .write_all(&frame(&fixture("stdin-identity", &[])))
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output, expected);
    }
}

#[test]
fn fragmented_bootstrap_and_nondefault_descriptor_work() {
    let (read, mut write) = std::io::pipe().unwrap();
    // SAFETY: duplicate a live descriptor with CLOEXEC so concurrent children
    // cannot inherit it. Only the intended child clears its copy's flag.
    let raw = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 73) };
    assert!(raw >= 73);
    let relocated = unsafe { OwnedFd::from_raw_fd(raw) };
    drop(read);
    let staging = tempfile::tempdir().unwrap();
    let child = spawn_with_bootstrap(runner(staging.path()).stdin(Stdio::null()), &relocated);
    drop(relocated);
    let bytes = frame(&request(&["/bin/echo", "fragmented"]));
    for byte in &bytes[..4] {
        write.write_all(&[*byte]).unwrap();
    }
    for fragment in bytes[4..].chunks(7) {
        write.write_all(fragment).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    // The bootstrap writer remains open throughout native setup and target exit.
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        (output.stdout, output.stderr),
        (b"fragmented\n".to_vec(), vec![])
    );
}

#[test]
fn bootstrap_reads_exactly_one_frame() {
    let bytes = frame(&request(&["/bin/echo", "one-frame"]));
    let mut bootstrap = tempfile::tempfile().unwrap();
    bootstrap.write_all(&bytes).unwrap();
    bootstrap.write_all(b"unconsumed\0\xff").unwrap();
    bootstrap.rewind().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let child = spawn_with_bootstrap(runner(staging.path()).stdin(Stdio::null()), &bootstrap);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"one-frame\n");
    assert_eq!(bootstrap.stream_position().unwrap(), bytes.len() as u64);
}

#[test]
fn bootstrap_resource_is_closed_while_target_and_proxy_are_alive() {
    for proxy in [Value::Null, proxy_config()] {
        let staging = tempfile::tempdir().unwrap();
        let (mut child, mut bootstrap) = spawn(runner(staging.path()).stdin(Stdio::piped()));
        let mut stdin = child.stdin.take().unwrap();
        let mut request = fixture("ready-stdin", &[]);
        request["proxy"] = proxy;
        bootstrap.write_all(&frame(&request)).unwrap();
        let mut ready = [0; 5];
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        let result = bootstrap.write_all(b"no reader may remain");
        stdin.write_all(b"released").unwrap();
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(ready, *b"ready");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(
            (output.stdout, output.stderr),
            (b"released".to_vec(), vec![])
        );
    }
}

#[test]
fn incomplete_bootstrap_cancels_startup_even_with_valid_configuration_on_stdin() {
    let mut request = request(&["/bin/echo", "must-not-launch"]);
    request["proxy"] = proxy_config();
    let bytes = frame(&request);
    for length in [0, 1, 2, 3, 4, bytes.len() - 1] {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(&bytes).unwrap();
        input.rewind().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let (child, mut bootstrap) = spawn(runner(staging.path()).stdin(input));
        bootstrap.write_all(&bytes[..length]).unwrap();
        drop(bootstrap);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(output.stdout, b"");
        assert!(String::from_utf8_lossy(&output.stderr).contains("read bootstrap"));
    }
}

fn reject_without_stdin(mut command: Command) -> Output {
    let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut event = libc::pollfd {
        fd: child.stderr.as_ref().unwrap().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // A deadline bounds failure of the no-stdin-read assertion; closing stdin
    // afterwards lets even the old executable exit before we report failure.
    let ready = unsafe { libc::poll(&mut event, 1, 5000) };
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert_eq!(ready, 1, "{output:?}");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, b"");
    assert!(!output.stderr.is_empty(), "{output:?}");
    output
}

#[test]
fn invalid_invocations_fail_without_reading_stdin() {
    for args in [
        vec![],
        vec!["--bootstrap-fd"],
        vec!["--unknown", "3"],
        vec!["--bootstrap-fd=3"],
        vec!["--bootstrap-fd", "3", "extra"],
        vec!["--bootstrap-fd", "3", "--bootstrap-fd", "3"],
    ] {
        let staging = tempfile::tempdir().unwrap();
        let mut command = runner(staging.path());
        command.args(args);
        reject_without_stdin(command);
    }
    for number in [
        "",
        "abc",
        "3.5",
        "+3",
        "-1",
        "0",
        "1",
        "2",
        "2147483648",
        "999999999999999999999",
    ] {
        let staging = tempfile::tempdir().unwrap();
        let mut command = runner(staging.path());
        command.args(["--bootstrap-fd", number]);
        reject_without_stdin(command);
    }
}

#[test]
fn closed_bootstrap_is_rejected_before_descriptor_reuse() {
    for fd in [3, 73] {
        let staging = tempfile::tempdir().unwrap();
        let mut command = runner(staging.path());
        command.args(["--bootstrap-fd", &fd.to_string()]);
        // SAFETY: only close this child's descriptor before exec. It may
        // already be closed; the executable must detect that before read_dir.
        unsafe {
            command.pre_exec(move || {
                libc::close(fd);
                Ok(())
            });
        }
        let output = reject_without_stdin(command);
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid bootstrap fd"));
    }
}

#[test]
fn unreadable_bootstrap_is_rejected() {
    let (_read, write) = std::io::pipe().unwrap();
    let mut descriptors = vec![OwnedFd::from(write)];
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_PATH descriptors report O_RDONLY access bits but cannot be read.
        descriptors.push(
            File::options()
                .read(true)
                .custom_flags(libc::O_PATH)
                .open("/dev/null")
                .unwrap()
                .into(),
        );
    }
    for descriptor in descriptors.drain(..) {
        let staging = tempfile::tempdir().unwrap();
        let child = spawn_with_bootstrap(runner(staging.path()).stdin(Stdio::null()), &descriptor);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(output.stdout, b"");
        assert!(String::from_utf8_lossy(&output.stderr).contains("bootstrap fd must be readable"));
    }
}

#[test]
fn bootstrap_errors_close_the_resource_without_consuming_stdin() {
    let mut unsupported = request(&["/bin/echo", "must-not-launch"]);
    unsupported["version"] = json!(1);
    for bytes in [frame(&unsupported), 1048577u32.to_be_bytes().to_vec()] {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(b"original-input").unwrap();
        input.rewind().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let (child, mut bootstrap) =
            spawn(runner(staging.path()).stdin(input.try_clone().unwrap()));
        bootstrap.write_all(&bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(output.stdout, b"");
        assert!(!output.stderr.is_empty());
        assert_eq!(input.stream_position().unwrap(), 0);
        assert_eq!(
            bootstrap.write_all(b"probe").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }
}

#[test]
fn waiting_executable_releases_target_stdin() {
    let staging = tempfile::tempdir().unwrap();
    let (gate, mut release) = std::io::pipe().unwrap();
    let mut command = runner(staging.path());
    let (mut child, mut bootstrap) = spawn(command.stdin(Stdio::piped()).stderr(Stdio::from(gate)));
    drop(command);
    let mut stdin = child.stdin.take().unwrap();
    // Fill stdin before releasing bootstrap. With the target's reader closed,
    // this pipe can become writable only when the waiting parent releases it.
    // SAFETY: fcntl changes only this test's live pipe writer to nonblocking I/O.
    assert_eq!(
        unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        0
    );
    loop {
        match stdin.write(&[0; 4096]) {
            Ok(written) => assert!(written > 0),
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                break;
            }
        }
    }
    let mut request = fixture("close-stdin", &[]);
    // Linux's restricted-filesystem helpers can themselves retain stdin. Use
    // the native seccomp-only path to isolate this executable's ownership.
    // The filesystem and host-bwrap contracts still exercise the full path.
    request["filesystem"] = json!({"kind": "unrestricted"});
    bootstrap.write_all(&frame(&request)).unwrap();
    let mut ready = [0; 6];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    let mut event = libc::pollfd {
        fd: stdin.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: event is initialized and stdin remains open throughout poll.
    let ready_to_write = unsafe { libc::poll(&mut event, 1, 5000) };
    let write = stdin.write_all(b"closed-input-probe");
    release.write_all(b"x").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(ready, *b"closed");
    assert_eq!(ready_to_write, 1);
    assert_eq!(write.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
}
