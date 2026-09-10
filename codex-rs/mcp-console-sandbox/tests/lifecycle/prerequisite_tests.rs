use super::*;
use pretty_assertions::assert_eq;

#[test]
fn explicit_landlock_runs_without_namespace_helpers_and_preserves_policy_checks() {
    let directory = tempfile::tempdir().unwrap();
    let forbidden = directory.path().join("forbidden");
    let mut request = fixture("write", &[forbidden.to_str().unwrap()]);
    request["linux_backend"] = json!("landlock");
    let mut command = runner(directory.path());
    command.env("PATH", "/nonexistent");
    let output = run_command(command, frame(&request), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert!(!forbidden.exists());
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("configuration JSON"),
        "{output:?}"
    );
    request["command"] = json!(["/bin/echo", "explicit landlock"]);
    let output = run(frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"explicit landlock\n");
    request["lifecycle"] = json!({"private_tmp": {"environment": ["TMPDIR"]}});
    let output = run(frame(&request), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("landlock does not provide supervised lifetime"),
        "{output:?}"
    );
}

#[test]
fn explicit_backend_selection_does_not_fall_back_when_namespaces_are_denied() {
    for backend in ["bubblewrap", "landlock"] {
        let directory = tempfile::tempdir().unwrap();
        let mut command = runner(directory.path());
        unsafe {
            command.pre_exec(|| {
                let statement = |code, k| libc::sock_filter {
                    code,
                    jt: 0,
                    jf: 0,
                    k,
                };
                let jump = |code, k, jt, jf| libc::sock_filter { code, k, jt, jf };
                let mut filters = [
                    statement(0x20, 0),
                    jump(0x15, libc::SYS_unshare as u32, 4, 0),
                    jump(0x15, libc::SYS_clone3 as u32, 3, 0),
                    jump(0x15, libc::SYS_clone as u32, 0, 3),
                    statement(0x20, 16),
                    jump(
                        0x45,
                        (libc::CLONE_NEWUSER
                            | libc::CLONE_NEWNS
                            | libc::CLONE_NEWPID
                            | libc::CLONE_NEWNET) as u32,
                        0,
                        1,
                    ),
                    statement(0x06, 0x00050000 | libc::EPERM as u32),
                    statement(0x06, 0x7fff0000),
                ];
                let program = libc::sock_fprog {
                    len: filters.len() as u16,
                    filter: filters.as_mut_ptr(),
                };
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
                    || libc::prctl(libc::PR_SET_SECCOMP, 2, &program) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut request = request(&["/bin/echo", "target"]);
        request["linux_backend"] = json!(backend);
        let output = run_command(command, frame(&request), &[]);
        if backend == "landlock" {
            assert_eq!(
                (output.status.code(), output.stdout, output.stderr),
                (Some(0), b"target\n".to_vec(), vec![])
            );
        } else {
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("namespace"),
                "{output:?}"
            );
        }
    }
}

#[test]
fn landlock_preserves_native_policy_rejections_and_rejects_proxy_switching() {
    let mut request = request(&["/bin/echo", "must not run"]);
    request["linux_backend"] = json!("landlock");
    request["proxy"] = proxy_config();
    let output = run(frame(&request), &[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("landlock does not support managed proxy routing"),
        "{output:?}"
    );
    request["proxy"] = Value::Null;
    request["filesystem"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path": {"type": "path", "path": "/etc"}, "access": "none"}));
    let output = run(frame(&request), &[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("incompatible with --use-legacy-landlock"),
        "{output:?}"
    );
}

fn constrained_runner(directory: &Path, unavailable: i64) -> Command {
    let mut command = runner(directory);
    // Restrict the actual process and all of its native helpers, not a mocked
    // capability probe. Namespace lifetime must still retire detached children.
    unsafe {
        command.pre_exec(move || {
            let statement = |code, k| libc::sock_filter {
                code,
                jt: 0,
                jf: 0,
                k,
            };
            let equal = |k, jt, jf| libc::sock_filter {
                code: 0x15,
                jt,
                jf,
                k,
            };
            let mut filters = [
                statement(0x20, 0),
                equal(unavailable as u32, 3, 0),
                equal(libc::SYS_prctl as u32, 0, 3),
                statement(0x20, 16),
                equal(libc::PR_SET_CHILD_SUBREAPER as u32, 0, 1),
                statement(0x06, 0x00050000 | libc::ENOSYS as u32),
                statement(0x06, 0x7fff0000),
            ];
            let program = libc::sock_fprog {
                len: filters.len() as u16,
                filter: filters.as_mut_ptr(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
                || libc::prctl(libc::PR_SET_SECCOMP, 2, &program) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

#[test]
fn namespace_lifetime_does_not_require_pidfds_or_a_host_subreaper() {
    let directory = tempfile::tempdir().unwrap();
    let command = constrained_runner(directory.path(), libc::SYS_pidfd_open);
    let mut request = fixture("lifecycle", &["nonzero"]);
    request["lifecycle"] =
        json!({"parent_pid": std::process::id(), "private_tmp": {"environment": ["TMPDIR"]}});
    let output = run_command(command, frame(&request), &[]);
    assert_eq!(output.status.code(), Some(42), "{output:?}");
    assert_eq!(output.stderr, b"");
    let temporary = std::str::from_utf8(&output.stdout).unwrap().trim();
    assert!(!Path::new(temporary).exists());
}

#[test]
fn inherited_procfs_identity_does_not_prevent_native_execution() {
    let directory = tempfile::tempdir().unwrap();
    let native_runner = runner(directory.path());
    let library = directory.path().join("procfs.so");
    let source =
        codex_utils_cargo_bin::find_resource!("tests/lifecycle/procfs_interposer.c").unwrap();
    let compiler = Command::new("cc")
        .args(["-shared", "-fPIC"])
        .arg(source)
        .args(["-o"])
        .arg(&library)
        .arg("-ldl")
        .output()
        .unwrap();
    assert!(compiler.status.success(), "{compiler:?}");
    let mut config = request(&["/bin/echo", "target"]);
    for key in ["command", "cwd", "environment"] {
        config.as_object_mut().unwrap().remove(key);
    }
    // Supply a mismatched procfs identity at the real native execution hook.
    // Full differential policy tests exercise real inherited procfs separately.
    let output = Command::new(native_runner.get_program())
        .args([
            "--config-env",
            "SANDBOX_TEST_CONFIG",
            "--",
            "/bin/echo",
            "target",
        ])
        .env("SANDBOX_TEST_CONFIG", config.to_string())
        .env("RUST_BACKTRACE", "1")
        .env("LD_PRELOAD", library)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"target\n");
    assert_eq!(output.stderr, b"");
}

#[test]
fn constrained_host_retires_detached_children_and_forwards_interrupts() {
    use std::io::BufRead;
    use std::io::BufReader;
    for (operation, unavailable) in [
        ("normal-tree", libc::SYS_pidfd_open),
        ("interrupted", libc::SYS_pidfd_open),
        ("detached", libc::SYS_pidfd_open),
        ("detached", libc::SYS_pidfd_send_signal),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut command = constrained_runner(directory.path(), unavailable);
        let (mut child, mut bootstrap) = spawn(command.stdin(Stdio::piped()));
        drop(command);
        let mut request = fixture("lifecycle", &[operation]);
        request["lifecycle"] = json!({"parent_pid": std::process::id(), "sigterm": "retire", "private_tmp": {"environment": ["TMPDIR"]}});
        bootstrap.write_all(&frame(&request)).unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        if operation == "normal-tree" {
            child.stdin.take().unwrap().write_all(b"exit").unwrap();
        } else {
            let signal = if operation == "interrupted" {
                libc::SIGINT
            } else {
                libc::SIGTERM
            };
            assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        }
        let stdin = child.stdin.take();
        let output = child.wait_with_output().unwrap();
        drop(stdin);
        assert_eq!(
            output.status.code(),
            Some(if operation == "detached" { 0 } else { 42 }),
            "{output:?}"
        );
        assert_eq!(output.stderr, b"");
        assert!(!Path::new(ready["temporary"].as_str().unwrap()).exists());
    }
}

#[test]
fn a_stopped_init_without_pidfds_reports_incomplete_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let library = startup::interposer(directory.path());
    let mut command = constrained_runner(directory.path(), libc::SYS_pidfd_open);
    startup::preload(&mut command, &library);
    let (mut events, event_writer) = std::io::pipe().unwrap();
    let (release, mut release_writer) = std::io::pipe().unwrap();
    command
        .env("SANDBOX_TEST_SETUP_STAGE", "blocked")
        .env(
            "SANDBOX_TEST_EVENT_FD",
            event_writer.as_raw_fd().to_string(),
        )
        .env("SANDBOX_TEST_RELEASE_FD", release.as_raw_fd().to_string());
    startup::inherit(
        &mut command,
        &[event_writer.as_raw_fd(), release.as_raw_fd()],
    );
    let mut request = request(&["/bin/echo", "must not run"]);
    request["lifecycle"] = json!({"sigterm": "retire", "cleanup_timeout_ms": 100, "private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let (child, mut bootstrap) = spawn(&mut command);
    drop(command);
    drop(event_writer);
    drop(release);
    bootstrap.write_all(&frame(&request)).unwrap();
    events.read_exact(&mut [0; 4]).unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    release_writer.write_all(b"x").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out retiring sandbox descendants"),
        "{output:?}"
    );
    assert!(
        stderr.contains("private storage retained after incomplete retirement"),
        "{output:?}"
    );
    assert!(std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("sandbox-")
    }));
}

#[test]
fn missing_native_wait_status_never_reports_cleanup_success() {
    let directory = tempfile::tempdir().unwrap();
    let library = startup::interposer(directory.path());
    let mut command = runner(directory.path());
    startup::preload(&mut command, &library);
    command.env("SANDBOX_TEST_FAIL_WAIT", "1");
    let mut request = request(&["/bin/echo", "must not run"]);
    request["lifecycle"] = json!({"cleanup_timeout_ms": 100, "private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let output = run_command(command, frame(&request), &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("descendant retirement failed: Input/output error"),
        "{output:?}"
    );
    assert!(
        stderr.contains("private storage retained after incomplete retirement"),
        "{output:?}"
    );
}

#[test]
fn unavailable_landlock_never_executes_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let command = constrained_runner(directory.path(), libc::SYS_landlock_create_ruleset);
    let mut request = request(&["/bin/echo", "must not run"]);
    request["linux_backend"] = json!("landlock");
    let output = run_command(command, frame(&request), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Landlock"),
        "{output:?}"
    );
}
