use super::*;
use pretty_assertions::assert_eq;
use std::io::BufRead;
use std::io::BufReader;

#[test]
fn same_user_target_cannot_control_supervisor_or_change_accepted_policy() {
    for environment_mode in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let forbidden = directory.path().join("forbidden");
        let mut request = fixture("adversary", &[]);
        request["lifecycle"] = json!({"private_tmp": {"environment": ["TMPDIR"]}});
        let (mut child, bootstrap) = if environment_mode {
            let mut config = request.clone();
            for field in ["command", "cwd", "environment"] {
                config.as_object_mut().unwrap().remove(field);
            }
            config["inherit_environment"] = json!(false);
            let child = runner(directory.path())
                .args(["--config-env", "SANDBOX_TEST_CONFIG", "--"])
                .arg(cargo_bin("mcp-console-sandbox-fixture").unwrap())
                .arg("adversary")
                .env("SANDBOX_TEST_CONFIG", config.to_string())
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            (child, None)
        } else {
            let (child, mut bootstrap) = spawn(runner(directory.path()).stdin(Stdio::piped()));
            bootstrap.write_all(&frame(&request)).unwrap();
            (child, Some(bootstrap))
        };
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line, "ready\n");
        // The original transport no longer has a reader. A second request cannot
        // mutate the accepted policy, even when sent by the authorized caller.
        request["filesystem"] = json!({"type": "unrestricted"});
        if let Some(mut bootstrap) = bootstrap {
            assert!(bootstrap.write_all(&frame(&request)).is_err());
        }
        let input = json!({"supervisor": child.id(), "forbidden": forbidden});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(!forbidden.exists());
    }
}

#[test]
fn supervisor_loss_cannot_remove_native_enforcement() {
    let directory = tempfile::tempdir().unwrap();
    let forbidden = directory.path().join("forbidden");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut request = fixture(
        "survivor",
        &[
            forbidden.to_str().unwrap(),
            &listener.local_addr().unwrap().to_string(),
        ],
    );
    request["lifecycle"] =
        json!({"private_tmp": {"parent": directory.path(), "environment": ["TMPDIR"]}});
    let (mut child, mut bootstrap) = spawn(runner(directory.path()).stdin(Stdio::piped()));
    bootstrap.write_all(&frame(&request)).unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    let mut stdin = child.stdin.take().unwrap();
    #[cfg(target_os = "linux")]
    let processes = ownership::watch_tree(child.id() as i32);
    child.kill().unwrap();
    child.wait().unwrap();
    #[cfg(target_os = "linux")]
    ownership::await_exit(&processes);
    let _ = stdin.write_all(b"continue");
    drop(stdin);
    line.clear();
    stdout.read_to_string(&mut line).unwrap();
    #[cfg(target_os = "macos")]
    assert_eq!(line, "still restricted\n");
    #[cfg(target_os = "linux")]
    assert_eq!(line, "");
    assert!(!forbidden.exists());
    // SIGKILL deliberately has no custom cleanup owner. The test owns leftovers.
    assert!(Path::new(ready["temporary"].as_str().unwrap()).exists());
}
