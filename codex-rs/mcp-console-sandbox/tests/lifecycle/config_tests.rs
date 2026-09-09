use super::*;
use pretty_assertions::assert_eq;

fn config() -> Value {
    let mut value = request(&[]);
    for field in ["command", "cwd", "environment"] {
        value.as_object_mut().unwrap().remove(field);
    }
    value
}

#[test]
fn environment_configuration_preserves_launch_inputs_and_removes_transport() {
    for name in ["SANDBOX_TEST_CONFIG", "PWD"] {
        let directory = tempfile::tempdir().unwrap();
        let output = runner(directory.path())
            .args(["--config-env", name, "--"])
            .arg(cargo_bin("mcp-console-sandbox-fixture").unwrap())
            .arg("context")
            .current_dir(directory.path())
            .env(name, config().to_string())
            .env("ORDINARY_TARGET_VARIABLE", "unchanged")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let context: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            context["cwd"],
            json!(directory.path().canonicalize().unwrap())
        );
        assert_eq!(
            context["environment"]["ORDINARY_TARGET_VARIABLE"],
            "unchanged"
        );
        assert!(
            context["environment"].get(name).is_none(),
            "transport variable {name} reached target"
        );
    }
}

#[test]
fn configuration_is_explicit_json_without_file_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    std::fs::write(&path, config().to_string()).unwrap();
    for value in [
        path.to_str().unwrap(),
        "{}",
        "null",
        "{\"version\":2,\"version\":2}",
    ] {
        let output = runner(directory.path())
            .args([
                "--config-env",
                "SANDBOX_TEST_CONFIG",
                "--",
                "/bin/echo",
                "executed",
            ])
            .env("SANDBOX_TEST_CONFIG", value)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    let output = runner(directory.path())
        .args([
            "--config-env",
            "SANDBOX_TEST_CONFIG",
            "--",
            "/bin/echo",
            "executed",
        ])
        .env_remove("SANDBOX_TEST_CONFIG")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
}

#[test]
fn environment_configuration_rejects_duplicated_launch_inputs() {
    for (field, value) in [
        ("command", json!(["/bin/echo", "injected"])),
        ("environment", json!({})),
        ("cwd", json!("/")),
        ("reload", json!(true)),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut configuration = config();
        configuration[field] = value;
        let output = runner(directory.path())
            .args([
                "--config-env",
                "SANDBOX_TEST_CONFIG",
                "--",
                "/bin/echo",
                "executed",
            ])
            .env("SANDBOX_TEST_CONFIG", configuration.to_string())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{field}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
}

#[test]
fn invalid_lifecycle_configuration_never_executes_target() {
    for configuration in [
        json!({"parent_pid": 1}),
        json!({"parent_pid": std::process::id() + 1000000}),
        json!({"sigterm": "ignore"}),
        json!({"cleanup_timeout_ms": 0}),
        json!({"private_tmp": {"parent": "relative", "environment": []}}),
        json!({"private_tmp": {"environment": ["INVALID=NAME"]}}),
        json!({"reload": true}),
    ] {
        let mut request = request(&["/bin/echo", "executed"]);
        request["lifecycle"] = configuration;
        let output = run(frame(&request), &[]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_setup_rejects_procfs_fallback_before_target_execution() {
    let directory = tempfile::tempdir().unwrap();
    let output = runner(directory.path())
        .args([
            "--target-setup-fd",
            "3",
            "--sandbox-policy-cwd",
            "/",
            "--no-proc",
            "--",
            "/bin/echo",
            "executed",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("namespace-local procfs"),
        "{output:?}"
    );
}
