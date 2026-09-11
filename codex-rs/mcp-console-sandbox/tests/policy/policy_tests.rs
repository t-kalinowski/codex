use super::*;
use pretty_assertions::assert_eq;
use std::io::BufRead;
use std::io::BufReader;

#[test]
fn unrestricted_filesystem_preserves_independent_network_policy() {
    let outside = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    for kind in ["unrestricted", "external-sandbox"] {
        for network in ["restricted", "enabled"] {
            let file = outside.path().join(format!("{kind}-{network}"));
            let mut request = fixture("write", &[file.to_str().unwrap()]);
            request["cwd"] = json!(workspace.path());
            request["filesystem"] = json!({"kind": kind});
            request["network"] = json!(network);
            let output = run(frame(&request), &[]);
            assert!(output.status.success(), "{kind}/{network}: {output:?}");
            assert_eq!(std::fs::read(file).unwrap(), b"created");

            request["command"] = fixture("connect", &[&address])["command"].clone();
            let output = run(frame(&request), &[]);
            assert_eq!(
                output.status.success(),
                network == "enabled" || kind == "external-sandbox",
                "{kind}/{network}: {output:?}"
            );
            request["command"] = fixture("stdout", &[])["command"].clone();
            let output = run(frame(&request), b"target input\0\xff");
            assert_eq!(
                (output.status.code(), output.stdout, output.stderr),
                (Some(0), b"target input\0\xff".to_vec(), vec![])
            );
        }
    }
}

#[test]
fn unrestricted_and_external_managed_proxy_preserve_routing_and_allowlist() {
    for kind in ["unrestricted", "external-sandbox"] {
        for network in ["restricted", "enabled"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nallowed",
                    )
                    .unwrap();
            });
            let mut request = fixture("proxy-get", &[&format!("http://{address}/")]);
            request["filesystem"] = json!({"kind": kind});
            request["network"] = json!(network);
            request["proxy"] = proxy_config();
            request["proxy"]["allowLocalBinding"] = json!(false);
            let output = run(frame(&request), &[]);
            assert!(output.status.success(), "{kind}/{network}: {output:?}");
            assert!(output.stdout.ends_with(b"allowed"), "{output:?}");
            server.join().unwrap();
            request["proxy"]["domains"] = json!({"127.0.0.1": "deny"});
            let output = run(frame(&request), &[]);
            assert!(output.status.success(), "{output:?}");
            assert!(output.stdout.starts_with(b"HTTP/1.1 403"), "{output:?}");
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            request["command"] = fixture("connect", &[&listener.local_addr().unwrap().to_string()])
                ["command"]
                .clone();
            let output = run(frame(&request), &[]);
            assert!(
                !output.status.success(),
                "direct connection bypassed proxy: {output:?}"
            );
        }
    }
}

#[test]
fn unrestricted_signal_forwarding_retires_descendants_and_private_storage() {
    for network in ["restricted", "enabled"] {
        let directory = tempfile::tempdir().unwrap();
        let (mut child, mut bootstrap) = spawn(runner(directory.path()).stdin(Stdio::piped()));
        let mut request = fixture("lifecycle", &["interrupted"]);
        request["filesystem"] = json!({"kind": "unrestricted"});
        request["network"] = json!(network);
        request["lifecycle"] =
            json!({"parent_pid": std::process::id(), "private_tmp": {"environment": ["TMPDIR"]}});
        bootstrap.write_all(&frame(&request)).unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "target failed before readiness");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(42), "{output:?}");
        assert_eq!(output.stderr, b"");
        assert!(!Path::new(value["temporary"].as_str().unwrap()).exists());
    }
}

#[test]
fn external_sandbox_supervises_the_process_group_without_native_enforcement() {
    let directory = tempfile::tempdir().unwrap();
    let (mut child, mut bootstrap) = spawn(runner(directory.path()).stdin(Stdio::piped()));
    let mut request = fixture("lifecycle", &["group-tree"]);
    request["filesystem"] = json!({"kind": "external-sandbox"});
    request["lifecycle"] = json!({"sigterm": "retire", "private_tmp": {"environment": ["TMPDIR"]}});
    bootstrap.write_all(&frame(&request)).unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "target failed before readiness");
    let value: Value = serde_json::from_str(&line).unwrap();
    child.stdin.as_mut().unwrap().write_all(&[1]).unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert!(line.trim().parse::<u32>().unwrap() > 1);
    let mut partial = [0; b"{\"partial\":".len()];
    stdout.read_exact(&mut partial).unwrap();
    assert_eq!(&partial, b"{\"partial\":");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let stdin = child.stdin.take();
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!((output.status.code(), output.stderr), (Some(0), vec![]));
    assert!(!Path::new(value["temporary"].as_str().unwrap()).exists());
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "");
}

#[test]
fn unrestricted_caller_death_retires_the_lifetime() {
    for network in ["restricted", "enabled"] {
        let directory = tempfile::tempdir().unwrap();
        let runner = runner(directory.path());
        let mut request = fixture("lifecycle", &["detached"]);
        request["filesystem"] = json!({"kind": "unrestricted"});
        request["network"] = json!(network);
        request["lifecycle"] = json!({"private_tmp": {"environment": ["TMPDIR"]}});
        let mut owner = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap())
            .arg("owner")
            .arg(runner.get_program())
            .env("SANDBOX_TEST_REQUEST", request.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(owner.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert!(line.trim().parse::<u32>().unwrap() > 1);
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        let stdin = owner.stdin.take();
        owner.kill().unwrap();
        let output = owner.wait_with_output().unwrap();
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).unwrap();
        drop(stdin);
        assert_eq!((rest, output.stderr), (String::new(), vec![]));
        assert!(!Path::new(value["temporary"].as_str().unwrap()).exists());
    }
}

#[test]
fn external_sandbox_runs_inside_existing_native_network_restrictions() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(directory.path());
    let mut request = request(&[
        runner.get_program().to_str().unwrap(),
        "--config-env",
        "OUTER_POLICY",
        "--",
        "/bin/echo",
        "externally enforced",
    ]);
    request["environment"]["OUTER_POLICY"] = json!(
        json!({
            "version":2,"filesystem":{"kind":"external-sandbox"},"network":"restricted"
        })
        .to_string()
    );
    let output = run(frame(&request), &[]);
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (Some(0), b"externally enforced\n".to_vec(), vec![])
    );
}
