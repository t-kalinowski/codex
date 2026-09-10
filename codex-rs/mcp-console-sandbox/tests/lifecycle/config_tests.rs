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
    for name in ["SANDBOX_TEST_CONFIG", "PWD", "TMPDIR"] {
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
fn target_environment_overrides_are_optional_and_transport_cannot_be_reintroduced() {
    for inherit in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut configuration = config();
        configuration["inherit_environment"] = json!(inherit);
        configuration["environment"] = json!({
            "OVERRIDE": "Unicode: 雪, é; quotes: \" ' $() ` ; \\ \n",
            "SANDBOX_TEST_CONFIG": "must disappear",
            "MCP_CONSOLE_SANDBOX_CONFIG": "must disappear too"
        });
        let output = runner(directory.path())
            .args(["--config-env", "SANDBOX_TEST_CONFIG", "--"])
            .arg(cargo_bin("mcp-console-sandbox-fixture").unwrap())
            .arg("context")
            .env("SANDBOX_TEST_CONFIG", configuration.to_string())
            .env(
                "MCP_CONSOLE_SANDBOX_CONFIG",
                "ambient policy is not selected",
            )
            .env("INHERITED", "present")
            .env("OVERRIDE", "old")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let context: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            context["environment"]["OVERRIDE"],
            configuration["environment"]["OVERRIDE"]
        );
        assert_eq!(context["environment"].get("INHERITED").is_some(), inherit);
        for name in ["SANDBOX_TEST_CONFIG", "MCP_CONSOLE_SANDBOX_CONFIG"] {
            assert!(context["environment"].get(name).is_none(), "{context}");
        }
    }
}

#[test]
fn descriptor_environment_cannot_export_reserved_transport() {
    let mut request = fixture("context", &[]);
    request["environment"]["MCP_CONSOLE_SANDBOX_CONFIG"] = json!("must disappear");
    let output = run(frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    let context: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        context["environment"]
            .get("MCP_CONSOLE_SANDBOX_CONFIG")
            .is_none()
    );
}

#[test]
fn parent_environment_changes_after_exec_do_not_change_unparsed_policy() {
    let directory = tempfile::tempdir().unwrap();
    let library = startup::interposer(directory.path());
    let runner = runner(directory.path());
    let output = Command::new(cargo_bin("mcp-console-sandbox-fixture").unwrap())
        .arg("environment-copy")
        .arg(runner.get_program())
        .arg(library)
        .arg(directory.path().join("forbidden"))
        .env("SANDBOX_TEST_CONFIG", config().to_string())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        b"launch environment remained fixed before parsing\n"
    );
    assert_eq!(output.stderr, b"");
}

#[test]
fn environment_mode_applies_target_loader_only_after_enforcement() {
    let directory = tempfile::tempdir().unwrap();
    let library = startup::interposer(directory.path());
    let marker = directory.path().join("loader-escaped");
    let mut config = config();
    #[cfg(target_os = "macos")]
    let variable = "DYLD_INSERT_LIBRARIES";
    #[cfg(target_os = "linux")]
    let variable = "LD_PRELOAD";
    config["environment"] = json!({variable: library, "SANDBOX_TEST_LOADER_MARKER": marker});
    let output = runner(directory.path())
        .args(["--config-env", "SANDBOX_TEST_CONFIG", "--"])
        .arg(cargo_bin("mcp-console-sandbox-fixture").unwrap())
        .arg("context")
        .env("SANDBOX_TEST_CONFIG", config.to_string())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stdout.starts_with(b"loader restricted\n"),
        "{output:?}"
    );
    assert!(!marker.exists());
}

#[test]
fn malformed_values_do_not_dump_environment_contents() {
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir().unwrap();
    let secret = "PRIVATE_ENVIRONMENT_SENTINEL";
    let mut configuration = config();
    configuration["environment"] = json!(secret);
    for value in [
        std::ffi::OsString::from(configuration.to_string()),
        std::ffi::OsString::from_vec([secret.as_bytes(), &[0xff]].concat()),
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
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
}

#[test]
fn environment_mode_excludes_inherited_and_native_setup_descriptors() {
    let directory = tempfile::tempdir().unwrap();
    let inherited = tempfile::tempfile().unwrap();
    let raw = inherited.as_raw_fd();
    let mut command = runner(directory.path());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(raw, libc::F_DUPFD, 180) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut config = config();
    config["proxy"] = proxy_config();
    let output = command
        .args(["--config-env", "SANDBOX_TEST_CONFIG", "--"])
        .arg(cargo_bin("mcp-console-sandbox-fixture").unwrap())
        .arg("descriptors")
        .env("SANDBOX_TEST_CONFIG", config.to_string())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!([])
    );
}

#[test]
fn descriptor_transport_cannot_bypass_target_exec_limits() {
    let mut request = fixture("context", &[]);
    let secret = "PRIVATE_ENVIRONMENT_SENTINEL".repeat(8000);
    request["environment"] = json!({"ONE": secret, "TWO": secret});
    #[cfg(target_os = "macos")]
    {
        // macOS counts argv pointers in its exec budget. Keep the JSON below
        // the frame cap while exceeding the host's actual argument limit.
        let count = unsafe { libc::sysconf(libc::_SC_ARG_MAX) } as usize / 8;
        request["command"]
            .as_array_mut()
            .unwrap()
            .extend(std::iter::repeat_n(json!(""), count));
    }
    assert!(serde_json::to_vec(&request).unwrap().len() <= 1048576);
    let output = run(frame(&request), &[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("os error 7"), "{error}");
    assert!(!error.contains("PRIVATE_ENVIRONMENT_SENTINEL"));
}

#[cfg(target_os = "linux")]
#[test]
fn target_path_cannot_select_a_host_helper() {
    let directory = tempfile::tempdir().unwrap();
    let host = directory.path().join("target-bin");
    std::fs::create_dir(&host).unwrap();
    copy_executable(
        &cargo_bin("mcp-console-sandbox-fixture").unwrap(),
        &host.join("bwrap"),
    );
    let mut request = fixture("context", &[]);
    request["environment"]["PATH"] = json!(host);
    request["environment"]["TMPDIR"] = json!("/target-only-missing-directory");
    let output = run_command(runner(directory.path()), frame(&request), &[]);
    assert!(output.status.success(), "{output:?}");
    let context: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(context["environment"]["PATH"], json!(host));
    assert_eq!(
        context["environment"]["TMPDIR"],
        "/target-only-missing-directory"
    );
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
