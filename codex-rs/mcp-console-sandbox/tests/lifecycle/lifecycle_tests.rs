use super::*;
use pretty_assertions::assert_eq;
use std::io::BufRead;
use std::io::BufReader;

fn managed(operation: &str) -> Value {
    let mut value = fixture("lifecycle", &[operation]);
    value["lifecycle"] = json!({"private_tmp": {"environment": ["TMPDIR"]}, "sigterm": "retire"});
    value
}

#[test]
fn private_storage_is_removed_after_success_and_nonzero_exit() {
    for operation in ["success", "nonzero", "replace", "mode-zero", "symlink"] {
        let output = run(frame(&managed(operation)), &[]);
        assert_eq!(
            output.status.code(),
            Some(if operation == "nonzero" { 42 } else { 0 }),
            "{operation}: {output:?}"
        );
        let path = Path::new(std::str::from_utf8(&output.stdout).unwrap().trim());
        assert!(path.is_absolute(), "{output:?}");
        assert!(!path.exists(), "private storage remained: {path:?}");
    }
}

#[test]
fn retirement_kills_detached_descendant_and_waits_for_storage_cleanup() {
    for ignored in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut command = runner(directory.path());
        if ignored {
            unsafe {
                command.pre_exec(|| {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        let (mut child, mut bootstrap) = spawn(command.stdin(Stdio::piped()));
        drop(command);
        bootstrap.write_all(&frame(&managed("detached"))).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "runner failed before target readiness");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        let stdin = child.stdin.take();
        let output = child.wait_with_output().unwrap();
        drop(stdin);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(!Path::new(value["temporary"].as_str().unwrap()).exists());
        // An exited zombie cannot retain storage or execute; the OS may reap it later.
        #[cfg(target_os = "macos")]
        {
            let pid = value["descendant"].as_i64().unwrap() as i32;
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let count = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    1,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    std::mem::size_of_val(&info) as i32,
                )
            };
            assert!(count == 0 || info.pbi_status == libc::SZOMB);
        }
    }
}

#[test]
fn inherited_signal_state_reaches_target_and_sigchld_remains_waitable() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = runner(directory.path());
    unsafe {
        command.pre_exec(|| {
            for signal in [
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGTERM,
                libc::SIGCHLD,
                libc::SIGUSR2,
                #[cfg(target_os = "linux")]
                libc::SIGRTMAX(),
            ] {
                libc::signal(signal, libc::SIG_IGN);
            }
            let mut mask = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGUSR1);
            libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
            Ok(())
        });
    }
    let output = run_command(command, frame(&managed("signals")), &[]);
    assert_eq!(output.status.code(), Some(42), "{output:?}");
}

#[test]
fn interruption_preserves_target_status_and_retires_its_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let (mut child, mut bootstrap) = spawn(runner(directory.path()).stdin(Stdio::piped()));
    let mut request = managed("interrupted");
    request["lifecycle"]["parent_pid"] = json!(std::process::id());
    bootstrap.write_all(&frame(&request)).unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let stdin = child.stdin.take();
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(output.status.code(), Some(42), "{output:?}");
    assert!(!Path::new(value["temporary"].as_str().unwrap()).exists());
}

#[test]
fn configured_caller_death_retires_the_lifetime() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(directory.path());
    let mut owner = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap())
        .arg("owner")
        .arg(runner.get_program())
        .env("SANDBOX_TEST_REQUEST", managed("detached").to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(owner.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let _runner_pid: i32 = line.trim().parse().unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    let stdin = owner.stdin.take();
    owner.kill().unwrap();
    owner.wait().unwrap();
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    drop(stdin);
    assert!(rest.is_empty(), "{rest}");
    assert!(!Path::new(value["temporary"].as_str().unwrap()).exists());
}
