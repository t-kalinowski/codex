use codex_sandbox_api::CommandSpec;
#[cfg(target_os = "linux")]
use codex_sandbox_api::LinuxHelper;
use codex_sandbox_api::PathAccess;
use codex_sandbox_api::PathRule;
use codex_sandbox_api::SandboxError;
use codex_sandbox_api::SandboxExitStatus;
use codex_sandbox_api::SandboxFeature;
use codex_sandbox_api::SandboxPolicy;
use codex_sandbox_api::SandboxRequest;
use codex_sandbox_api::SandboxRuntime;
use codex_sandbox_api::SandboxRuntimeConfig;
use codex_sandbox_api::SandboxedOutput;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

fn fixture() -> PathBuf {
    match codex_utils_cargo_bin::cargo_bin("sandbox-api-fixture") {
        Ok(path) => path,
        Err(error) => panic!("sandbox-api-fixture should be built for integration tests: {error}"),
    }
}

struct TestRuntime {
    runtime: SandboxRuntime,
    _state: TempDir,
}

impl Deref for TestRuntime {
    type Target = SandboxRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

fn runtime(_work: &TempDir) -> TestResult<TestRuntime> {
    let state = tempfile::tempdir()?;
    let state_dir = state.path().join("state");
    fs::create_dir(&state_dir)?;
    let config = SandboxRuntimeConfig::new(state_dir);
    #[cfg(target_os = "linux")]
    let config = {
        let mut config = config;
        let helper = state.path().join("codex-linux-sandbox");
        std::os::unix::fs::symlink(fixture(), &helper)?;
        config.linux.helper = LinuxHelper::External(helper);
        config
    };
    Ok(TestRuntime {
        runtime: SandboxRuntime::new(config)?,
        _state: state,
    })
}

fn required_environment(omitted: Option<&OsString>) -> BTreeMap<OsString, OsString> {
    let mut env = BTreeMap::new();
    for name in [
        "PATH",
        "SystemRoot",
        "SystemDrive",
        "ComSpec",
        "PATHEXT",
        "TEMP",
        "TMP",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
    ] {
        if (cfg!(target_os = "linux") && name.starts_with("LD_"))
            || (cfg!(target_os = "macos") && name.starts_with("DYLD_"))
        {
            continue;
        }
        let name = OsString::from(name);
        if omitted == Some(&name) {
            continue;
        }
        if let Some(value) = std::env::var_os(&name) {
            env.insert(name, value);
        }
    }
    env.insert(
        OsString::from("SANDBOX_API_EXACT_ENV"),
        OsString::from("present"),
    );
    env
}

fn inherited_environment_to_omit() -> TestResult<OsString> {
    ["HOME", "USERPROFILE", "CARGO_MANIFEST_DIR", "RUSTUP_HOME"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
        .map(OsString::from)
        .ok_or_else(|| std::io::Error::other("no inherited environment value to omit").into())
}

fn command(cwd: &Path, mode: &str) -> CommandSpec {
    CommandSpec::new(fixture(), cwd, required_environment(/*omitted*/ None)).arg(mode)
}

fn writable_host_policy(path: &Path) -> SandboxPolicy {
    SandboxPolicy::host_read_only()
        .read_write(path)
        .network_unrestricted()
}

async fn collect(mut output: SandboxedOutput) -> Result<Vec<u8>, SandboxError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = output.read_chunk().await? {
        bytes.extend(chunk);
    }
    Ok(bytes)
}

async fn run_and_collect(
    runtime: &SandboxRuntime,
    request: SandboxRequest,
    stdin: Option<&[u8]>,
) -> TestResult<(SandboxExitStatus, Vec<u8>, Vec<u8>)> {
    let mut child = runtime.spawn(request).await?;
    assert_eq!(child.backend(), runtime.capabilities().backend);
    let stdout = required(child.take_stdout(), "stdout should be piped")?;
    let stderr = required(child.take_stderr(), "stderr should be piped")?;
    let stdout_task = tokio::spawn(collect(stdout));
    let stderr_task = tokio::spawn(collect(stderr));
    if let Some(bytes) = stdin {
        let mut child_stdin = required(child.take_stdin(), "stdin should be piped")?;
        child_stdin.write_all(bytes).await?;
        child_stdin.close().await?;
    } else {
        assert!(child.take_stdin().is_none());
    }
    let status = child.wait().await?;
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    Ok((status, stdout, stderr))
}

fn required<T>(value: Option<T>, message: &'static str) -> TestResult<T> {
    value.ok_or_else(|| std::io::Error::other(message).into())
}

fn spawned_error<T>(result: Result<T, SandboxError>) -> SandboxError {
    match result {
        Ok(_) => panic!("sandbox unexpectedly spawned the target"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn preserves_command_cwd_environment_and_raw_streams() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let omitted_env = inherited_environment_to_omit()?;
    let mut env = required_environment(Some(&omitted_env));
    env.insert(
        OsString::from("SANDBOX_API_EXACT_ENV"),
        OsString::from("present"),
    );
    let command = CommandSpec::new(fixture(), temp.path(), env)
        .arg("contract")
        .arg(temp.path())
        .arg(omitted_env)
        .arg("--stdio")
        .arg("-v");
    let request = SandboxRequest::new(command, writable_host_policy(temp.path())).stdin_open();
    let input = b"request\0\x80\xff\n";

    let (status, stdout, stderr) = run_and_collect(&runtime, request, Some(input)).await?;

    assert!(
        status.success(),
        "child failed with {status:?}; stdout={stdout:?}; stderr={stderr:?}"
    );
    let mut expected_stdout = b"stdin\0".to_vec();
    expected_stdout.extend(input);
    expected_stdout.extend([0x80, 0xff]);
    assert_eq!(stdout, expected_stdout);
    assert_eq!(stderr, b"stderr\0\x80\xff");
    Ok(())
}

#[tokio::test]
async fn drains_large_stdout_and_stderr_independently() -> TestResult {
    const EXPECTED_STREAM_SIZE: usize = 32 * 1024 * 128;
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let request = SandboxRequest::new(
        command(temp.path(), "large-output"),
        writable_host_policy(temp.path()),
    );

    let (status, stdout, stderr) = timeout(
        Duration::from_secs(20),
        run_and_collect(&runtime, request, /*stdin*/ None),
    )
    .await??;

    assert!(status.success());
    assert_eq!(stdout, vec![0xa5; EXPECTED_STREAM_SIZE]);
    assert_eq!(stderr, vec![0x5a; EXPECTED_STREAM_SIZE]);
    Ok(())
}

#[tokio::test]
async fn reports_ordinary_exit_and_try_status() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let request = SandboxRequest::new(
        command(temp.path(), "exit").arg("23"),
        writable_host_policy(temp.path()),
    );
    let mut child = runtime.spawn(request).await?;
    let stdout_task = tokio::spawn(collect(required(
        child.take_stdout(),
        "stdout should be piped",
    )?));
    let stderr_task = tokio::spawn(collect(required(
        child.take_stderr(),
        "stderr should be piped",
    )?));

    assert!(child.try_status().is_none());
    let status = child.wait().await?;

    assert_eq!(status.code(), Some(23));
    assert_eq!(status.signal(), None);
    assert!(!status.success());
    assert_eq!(
        child.try_status().map(SandboxExitStatus::code),
        Some(Some(23))
    );
    assert_eq!(stdout_task.await??, b"");
    assert_eq!(stderr_task.await??, b"");
    Ok(())
}

#[tokio::test]
async fn missing_path_error_prevents_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let missing = temp.path().join("missing");
    let marker = temp.path().join("target-ran");
    let policy =
        writable_host_policy(temp.path()).rule(PathRule::new(missing.clone(), PathAccess::Read));
    let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

    let error = spawned_error(runtime.spawn(request).await);

    match error {
        SandboxError::InvalidPath { path, .. } => assert_eq!(path, missing),
        error => panic!("unexpected error: {error:?}"),
    }
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn relative_rule_prevents_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let policy = writable_host_policy(temp.path()).read_only("relative");
    let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::InvalidPath { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn interior_nul_is_rejected_before_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let command = command(temp.path(), "marker")
        .arg(&marker)
        .arg(OsString::from("invalid\0argument"));
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::InvalidCommand { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn relative_program_is_rejected_before_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let command = CommandSpec::new(
        OsString::from("sandbox-api-fixture"),
        temp.path(),
        required_environment(/*omitted*/ None),
    )
    .arg("marker")
    .arg(&marker);
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::InvalidCommand { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn empty_environment_key_is_rejected_before_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let mut command = command(temp.path(), "marker").arg(&marker);
    command.env.insert(OsString::new(), "value".into());
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::InvalidCommand { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn leading_equals_windows_environment_key_is_accepted() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let mut command = command(temp.path(), "exit").arg("0");
    command.env.insert("=C:".into(), temp.path().into());
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let (status, stdout, stderr) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert_eq!(stdout, b"");
    assert_eq!(stderr, b"");
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn ambiguous_windows_environment_is_rejected_before_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let mut command = command(temp.path(), "marker").arg(&marker);
    command.env.insert("Path".into(), "first".into());
    command.env.insert("PATH".into(), "second".into());
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::InvalidCommand { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn non_utf8_windows_argument_is_rejected_before_target_execution() -> TestResult {
    use std::os::windows::ffi::OsStringExt;

    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let command = command(temp.path(), "marker")
        .arg(&marker)
        .arg(OsString::from_wide(&[0xd800]));
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::InvalidCommand { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn missing_path_ignore_omits_the_rule() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let missing = temp.path().join("missing");
    let marker = temp.path().join("target-ran");
    let policy = writable_host_policy(temp.path())
        .rule(PathRule::new(missing, PathAccess::Read).ignore_if_missing());
    let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert_eq!(fs::read(marker)?, b"target ran");
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_write_overlaps_are_rejected_before_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    for (name, policy) in [
        (
            "platform-default",
            SandboxPolicy::platform_minimal()
                .read_only("/tmp")
                .network_unrestricted(),
        ),
        (
            "host-device",
            SandboxPolicy::host_read_only()
                .read_only("/dev")
                .network_unrestricted(),
        ),
    ] {
        let marker = temp.path().join(format!("target-ran-{name}"));
        let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

        let error = spawned_error(runtime.spawn(request).await);

        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::DeniedWritePaths,
                ..
            }
        ));
        assert!(!marker.exists());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_root_deny_is_rejected_before_target_execution() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    for (name, policy) in [
        (
            "platform-minimal",
            SandboxPolicy::platform_minimal()
                .deny("/")
                .network_unrestricted(),
        ),
        (
            "host-read-only",
            SandboxPolicy::host_read_only()
                .deny("/")
                .network_unrestricted(),
        ),
    ] {
        let marker = temp.path().join(format!("target-ran-{name}"));
        let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

        let error = spawned_error(runtime.spawn(request).await);

        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::DeniedReadPaths,
                ..
            }
        ));
        assert!(!marker.exists());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn dynamic_loader_environment_is_rejected_before_seatbelt_launch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    for key in [
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FUTURE",
    ] {
        let marker = temp.path().join(format!("target-ran-{key}"));
        let mut command = command(temp.path(), "marker").arg(&marker);
        command.env.insert(key.into(), "/untrusted/value".into());
        let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

        let error = spawned_error(runtime.spawn(request).await);

        assert!(matches!(error, SandboxError::InvalidCommand { .. }));
        assert!(!marker.exists());
    }
    Ok(())
}

#[tokio::test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn unsupported_nested_allow_does_not_run_unsandboxed() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let denied = temp.path().join("denied");
    let nested_allow = denied.join("allowed");
    fs::create_dir_all(&nested_allow)?;
    let marker = temp.path().join("target-ran");
    let policy = writable_host_policy(temp.path())
        .deny(&denied)
        .read_only(&nested_allow);
    let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(
        error,
        SandboxError::UnsupportedPolicy {
            feature: SandboxFeature::NestedAllowUnderDeny,
            ..
        }
    ));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn nested_allow_under_deny_is_detected_through_a_symlink_alias() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let denied = temp.path().join("denied");
    let nested = denied.join("nested");
    let alias = temp.path().join("alias");
    fs::create_dir_all(&nested)?;
    std::os::unix::fs::symlink(&denied, &alias)?;
    let marker = temp.path().join("target-ran");
    let policy = writable_host_policy(temp.path())
        .deny(&denied)
        .read_only(alias.join("nested"));
    let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(
        error,
        SandboxError::UnsupportedPolicy {
            feature: SandboxFeature::NestedAllowUnderDeny,
            ..
        }
    ));
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn explicitly_allowed_read_and_write_are_enforced() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let capabilities = runtime.capabilities();
    let allowed_read = temp.path().join("allowed-read");
    let writable = temp.path().join("writable");
    fs::write(&allowed_read, "allowed contents")?;
    fs::create_dir(&writable)?;
    let written = writable.join("created");
    let policy = SandboxPolicy::platform_minimal()
        .read_only(fixture())
        .read_only(&allowed_read)
        .read_write(&writable)
        .network_unrestricted();
    let read_request = SandboxRequest::new(
        command(&writable, "fs-read")
            .arg(&allowed_read)
            .arg("allowed contents"),
        policy.clone(),
    );

    if !capabilities.minimal_read_policy {
        let error = spawned_error(runtime.spawn(read_request).await);
        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::MinimalReadPolicy,
                ..
            }
        ));
        return Ok(());
    }

    let (read_status, _, _) = run_and_collect(&runtime, read_request, /*stdin*/ None).await?;
    let write_request = SandboxRequest::new(command(&writable, "fs-write").arg(&written), policy);
    let (write_status, _, _) = run_and_collect(&runtime, write_request, /*stdin*/ None).await?;

    assert!(read_status.success());
    assert!(write_status.success());
    assert_eq!(fs::read(written)?, b"written by sandbox fixture");
    Ok(())
}

#[tokio::test]
async fn host_read_only_allows_an_explicit_read_root() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let cwd = temp.path().join("cwd");
    let readable = temp.path().join("readable");
    fs::create_dir(&cwd)?;
    fs::write(&readable, "allowed contents")?;
    let request = SandboxRequest::new(
        command(&cwd, "fs-read")
            .arg(&readable)
            .arg("allowed contents"),
        SandboxPolicy::host_read_only()
            .read_only(&readable)
            .read_write(&cwd)
            .network_unrestricted(),
    );

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn canonical_read_only_metadata_rule_overrides_a_symlinked_writable_root() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let real_root = temp.path().join("real-root");
    let metadata = real_root.join(".git");
    let alias = temp.path().join("writable-alias");
    fs::create_dir_all(&metadata)?;
    std::os::unix::fs::symlink(&real_root, &alias)?;
    let escaped = metadata.join("created");
    let request = SandboxRequest::new(
        command(&real_root, "fs-write-denied").arg(&escaped),
        SandboxPolicy::host_read_only()
            .read_write(&alias)
            .read_only(&metadata)
            .network_unrestricted(),
    );

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert!(!escaped.exists());
    Ok(())
}

#[tokio::test]
async fn denied_read_and_write_paths_are_enforced() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let capabilities = runtime.capabilities();
    let denied_read = temp.path().join("denied-read");
    let denied_write = temp.path().join("denied-write");
    fs::write(&denied_read, b"secret")?;
    fs::create_dir(&denied_write)?;
    let escaped = denied_write.join("created");

    let read_policy = writable_host_policy(temp.path()).deny(&denied_read);
    let read_request = SandboxRequest::new(
        command(temp.path(), "fs-read-denied").arg(&denied_read),
        read_policy,
    );
    if capabilities.denied_read_paths {
        let (status, _, _) = run_and_collect(&runtime, read_request, /*stdin*/ None).await?;
        assert!(status.success());
    } else {
        let error = spawned_error(runtime.spawn(read_request).await);
        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::DeniedReadPaths,
                ..
            }
        ));
    }

    let write_policy = writable_host_policy(temp.path()).read_only(&denied_write);
    let write_request = SandboxRequest::new(
        command(temp.path(), "fs-write-denied").arg(&escaped),
        write_policy,
    );
    if capabilities.denied_write_paths {
        let (status, _, _) = run_and_collect(&runtime, write_request, /*stdin*/ None).await?;
        assert!(status.success());
        assert!(!escaped.exists());
    } else {
        let error = spawned_error(runtime.spawn(write_request).await);
        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::DeniedWritePaths,
                ..
            }
        ));
    }
    Ok(())
}

#[tokio::test]
async fn nested_deny_overrides_a_writable_parent() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let capabilities = runtime.capabilities();
    if !capabilities.denied_read_paths || !capabilities.denied_write_paths {
        return Ok(());
    }
    let parent = temp.path().join("writable-parent");
    let denied = parent.join("denied-child");
    fs::create_dir_all(&denied)?;
    let secret = denied.join("secret");
    let escaped = denied.join("created");
    fs::write(&secret, b"secret")?;
    let policy = writable_host_policy(&parent).deny(&denied);

    let read_request = SandboxRequest::new(
        command(&parent, "fs-read-denied").arg(&secret),
        policy.clone(),
    );
    let write_request =
        SandboxRequest::new(command(&parent, "fs-write-denied").arg(&escaped), policy);
    let (read_status, _, _) = run_and_collect(&runtime, read_request, /*stdin*/ None).await?;
    let (write_status, _, _) = run_and_collect(&runtime, write_request, /*stdin*/ None).await?;

    assert!(read_status.success());
    assert!(write_status.success());
    assert!(!escaped.exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_reopens_a_writable_child_below_a_denied_parent() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let cwd = temp.path().join("cwd");
    let denied = temp.path().join("denied-parent");
    let writable_child = denied.join("writable-child");
    fs::create_dir(&cwd)?;
    fs::create_dir_all(&writable_child)?;
    let written = writable_child.join("created");
    let policy = SandboxPolicy::host_read_only()
        .read_write(&cwd)
        .deny(&denied)
        .read_write(&writable_child)
        .network_unrestricted();
    let request = SandboxRequest::new(command(&cwd, "fs-write").arg(&written), policy);

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert_eq!(fs::read(written)?, b"written by sandbox fixture");
    Ok(())
}

#[tokio::test]
async fn host_read_only_rejects_writes_outside_allowed_roots() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let allowed = temp.path().join("allowed");
    let elsewhere = temp.path().join("elsewhere");
    fs::create_dir(&allowed)?;
    fs::create_dir(&elsewhere)?;
    let forbidden = elsewhere.join("created");
    let request = SandboxRequest::new(
        command(&allowed, "fs-write-denied").arg(&forbidden),
        writable_host_policy(&allowed),
    );

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert!(!forbidden.exists());
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn host_read_only_rejects_writes_to_everyone_writable_paths() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let allowed = temp.path().join("allowed");
    let everyone_writable = temp.path().join("everyone-writable");
    fs::create_dir(&allowed)?;
    fs::create_dir(&everyone_writable)?;
    let acl_status = std::process::Command::new("icacls")
        .arg(&everyone_writable)
        .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
        .status()?;
    assert!(acl_status.success(), "failed to grant the fixture ACL");
    let forbidden = everyone_writable.join("created");
    let request = SandboxRequest::new(
        command(&allowed, "fs-write-denied").arg(&forbidden),
        writable_host_policy(&allowed),
    );

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert!(!forbidden.exists());
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn deny_write_carveout_does_not_leak_into_a_later_child() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let writable = temp.path().join("writable");
    let carveout = writable.join("carveout");
    fs::create_dir_all(&carveout)?;
    let first_target = carveout.join("first");
    let first_policy = SandboxPolicy::host_read_only()
        .read_write(&writable)
        .read_only(&carveout)
        .network_unrestricted();
    let first_request = SandboxRequest::new(
        command(&writable, "fs-write-denied").arg(&first_target),
        first_policy,
    );

    let (first_status, _, _) = run_and_collect(&runtime, first_request, /*stdin*/ None).await?;

    assert!(first_status.success());
    assert!(!first_target.exists());

    let second_target = carveout.join("second");
    let second_request = SandboxRequest::new(
        command(&writable, "fs-write").arg(&second_target),
        SandboxPolicy::host_read_only()
            .read_write(&writable)
            .network_unrestricted(),
    );
    let (second_status, _, _) = run_and_collect(&runtime, second_request, /*stdin*/ None).await?;

    assert!(second_status.success());
    assert_eq!(fs::read(second_target)?, b"written by sandbox fixture");
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn writable_child_below_read_only_host_root_is_supported() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let read_only = temp.path().join("read-only");
    let writable = read_only.join("writable");
    fs::create_dir_all(&writable)?;
    let written = writable.join("created");
    let policy = SandboxPolicy::host_read_only()
        .read_only(&read_only)
        .read_write(&writable)
        .network_unrestricted();
    let request = SandboxRequest::new(command(&writable, "fs-write").arg(&written), policy);

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert_eq!(fs::read(written)?, b"written by sandbox fixture");
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn nested_write_below_read_only_carveout_is_rejected_before_launch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let read_only = temp.path().join("read-only");
    let writable = read_only.join("writable");
    fs::create_dir_all(&writable)?;
    let marker = temp.path().join("target-ran");
    let policy = SandboxPolicy::host_read_only()
        .read_write(temp.path())
        .read_only(&read_only)
        .read_write(&writable)
        .network_unrestricted();
    let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(
        error,
        SandboxError::UnsupportedPolicy {
            feature: SandboxFeature::NestedAllowUnderDeny,
            ..
        }
    ));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn nested_write_below_symlinked_read_only_carveout_is_rejected_before_launch() -> TestResult {
    let writable = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let runtime = runtime(&writable)?;
    let reopened = outside.path().join("reopened");
    let alias = writable.path().join("alias");
    fs::create_dir(&reopened)?;
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&alias)
        .arg(outside.path())
        .output()?;
    assert!(output.status.success());
    let marker = writable.path().join("target-ran");
    let policy = SandboxPolicy::host_read_only()
        .read_write(writable.path())
        .read_only(&alias)
        .read_write(&reopened)
        .network_unrestricted();
    let request = SandboxRequest::new(command(writable.path(), "marker").arg(&marker), policy);

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(
        error,
        SandboxError::UnsupportedPolicy {
            feature: SandboxFeature::NestedAllowUnderDeny,
            ..
        }
    ));
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn network_denial_blocks_a_local_tcp_connection() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?.to_string();
    let request = SandboxRequest::new(
        command(temp.path(), "network-denied").arg(address),
        SandboxPolicy::host_read_only()
            .read_write(temp.path())
            .network_denied(),
    );

    if !runtime.capabilities().network_denial {
        let error = spawned_error(runtime.spawn(request).await);
        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::NetworkDenial,
                ..
            }
        ));
        return Ok(());
    }
    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    let accept_error = listener
        .accept()
        .expect_err("sandbox connected to listener");
    assert_eq!(accept_error.kind(), std::io::ErrorKind::WouldBlock);
    Ok(())
}

#[tokio::test]
async fn unrestricted_network_reaches_a_local_tcp_listener() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?.to_string();
    let request = SandboxRequest::new(
        command(temp.path(), "network-connect").arg(address),
        writable_host_policy(temp.path()),
    );

    if !runtime.capabilities().network_unrestricted {
        let error = spawned_error(runtime.spawn(request).await);
        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::NetworkUnrestricted,
                ..
            }
        ));
        return Ok(());
    }
    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;
    assert!(status.success());
    let (mut stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"connected");
    Ok(())
}

async fn waiting_child(
    runtime: &SandboxRuntime,
    cwd: &Path,
) -> TestResult<codex_sandbox_api::SandboxedChild> {
    let request = SandboxRequest::new(command(cwd, "wait"), writable_host_policy(cwd));
    let mut child = runtime.spawn(request).await?;
    let mut stdout = required(child.take_stdout(), "stdout should be piped")?;
    let stderr = required(child.take_stderr(), "stderr should be piped")?;
    let stderr_task = tokio::spawn(collect(stderr));
    let ready = timeout(Duration::from_secs(10), stdout.read_chunk()).await??;
    assert_eq!(ready.as_deref(), Some(b"ready\n".as_slice()));
    drop(stdout);
    drop(stderr_task);
    Ok(child)
}

#[tokio::test]
async fn terminate_stops_the_child() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let mut child = waiting_child(&runtime, temp.path()).await?;
    assert!(child.try_status().is_none());

    child.terminate()?;
    let status = timeout(Duration::from_secs(10), child.wait()).await??;

    assert!(!status.success());
    #[cfg(unix)]
    {
        assert_eq!(status.code(), None);
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
    Ok(())
}

#[tokio::test]
async fn interrupt_stops_the_child_when_supported() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    if !runtime.capabilities().interrupt {
        return Ok(());
    }
    let mut child = waiting_child(&runtime, temp.path()).await?;

    child.interrupt()?;
    let status = timeout(Duration::from_secs(10), child.wait()).await??;

    assert!(!status.success());
    #[cfg(unix)]
    {
        assert_eq!(status.code(), None);
        assert_eq!(status.signal(), Some(libc::SIGINT));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rejects_non_utf8_argument_without_running_target() -> TestResult {
    use std::os::unix::ffi::OsStringExt;

    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let command = command(temp.path(), "marker")
        .arg(&marker)
        .arg(OsString::from_vec(vec![0xff]));
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::InvalidCommand { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn inheritable_file_descriptors_are_closed_before_the_helper_executes() -> TestResult {
    use std::os::fd::AsRawFd;

    const SECRET: &str = "sandbox-api-inherited-fd-secret";
    let work = tempfile::tempdir()?;
    let secret_dir = tempfile::tempdir()?;
    let runtime = runtime(&work)?;
    let secret_path = secret_dir.path().join("secret");
    fs::write(&secret_path, SECRET)?;
    let secret = std::fs::File::open(&secret_path)?;
    let fd = secret.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_ne!(flags, -1);
    let updated = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    assert_ne!(updated, -1);
    let request = SandboxRequest::new(
        command(work.path(), "fd-unavailable")
            .arg(fd.to_string())
            .arg(SECRET),
        SandboxPolicy::host_read_only()
            .read_write(work.path())
            .deny(&secret_path)
            .network_unrestricted(),
    );

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn incomplete_file_descriptor_enumeration_prevents_target_execution() -> TestResult {
    const OPEN_DESCRIPTOR_COUNT: usize = 1_100;

    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let marker = temp.path().join("target-ran");
    let request = SandboxRequest::new(
        command(temp.path(), "marker").arg(&marker),
        writable_host_policy(temp.path()),
    );
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
        0
    );
    let original_limit = limit;
    limit.rlim_cur = limit.rlim_cur.max((OPEN_DESCRIPTOR_COUNT + 64) as _);
    assert!(limit.rlim_max == libc::RLIM_INFINITY || limit.rlim_cur <= limit.rlim_max);
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let _restore_limit = RestoreFileLimit(original_limit);
    let _descriptors = (0..OPEN_DESCRIPTOR_COUNT)
        .map(|_| std::fs::File::open("/dev/null"))
        .collect::<Result<Vec<_>, _>>()?;

    let error = match runtime.spawn(request).await {
        Err(error) => error,
        Ok(mut child) => {
            let _ = child.wait().await;
            return Err("target launched after incomplete descriptor enumeration".into());
        }
    };

    assert!(matches!(error, SandboxError::Spawn { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(target_os = "macos")]
struct RestoreFileLimit(libc::rlimit);

#[cfg(target_os = "macos")]
impl Drop for RestoreFileLimit {
    fn drop(&mut self) {
        let _ = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rejects_non_utf8_state_directory() -> TestResult {
    use std::os::unix::ffi::OsStringExt;

    let temp = tempfile::tempdir()?;
    let state_dir = temp.path().join(OsString::from_vec(vec![b's', 0xff]));
    let error = match SandboxRuntime::new(SandboxRuntimeConfig::new(state_dir.clone())) {
        Ok(_) => panic!("non-UTF-8 state directory should fail"),
        Err(error) => error,
    };

    assert!(matches!(error, SandboxError::InvalidPath { path, .. } if path == state_dir));
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unavailable_bubblewrap_is_reported_before_target_launch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let state_dir = temp.path().join("state");
    let helper = temp.path().join("codex-linux-sandbox");
    fs::create_dir(&state_dir)?;
    fs::copy(fixture(), &helper)?;
    let mut config = SandboxRuntimeConfig::new(state_dir);
    config.linux.helper = LinuxHelper::External(helper.clone());
    let runtime = SandboxRuntime::new(config)?;
    fs::remove_file(helper)?;
    let marker = temp.path().join("target-ran");
    let command = CommandSpec::new(
        fixture(),
        temp.path(),
        BTreeMap::from([(
            OsString::from("SANDBOX_API_EXACT_ENV"),
            OsString::from("present"),
        )]),
    )
    .arg("marker")
    .arg(&marker);
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let error = spawned_error(runtime.spawn(request).await);

    assert!(matches!(error, SandboxError::BackendUnavailable { .. }));
    assert!(!marker.exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn external_regular_file_helper_is_available_to_platform_minimal() -> TestResult {
    let work = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let state_dir = state.path().join("state");
    let helper = state.path().join("codex-linux-sandbox");
    fs::create_dir(&state_dir)?;
    fs::copy(fixture(), &helper)?;
    let mut config = SandboxRuntimeConfig::new(state_dir);
    config.linux.helper = LinuxHelper::External(helper);
    let runtime = SandboxRuntime::new(config)?;
    let marker = work.path().join("target-ran");
    let policy = SandboxPolicy::platform_minimal()
        .read_only(fixture())
        .read_write(work.path())
        .network_unrestricted();
    let request = SandboxRequest::new(command(work.path(), "marker").arg(&marker), policy);

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert_eq!(fs::read(marker)?, b"target ran");
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn child_path_cannot_replace_the_runtime_bubblewrap_launcher() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin)?;
    let fake_bwrap = fake_bin.join("bwrap");
    fs::write(
        &fake_bwrap,
        "#!/bin/sh\n: > \"$SANDBOX_API_FAKE_BWRAP_MARKER\"\nif [ \"$1\" = \"--help\" ]; then\n  printf '%s\\n' '--as-pid-1 --perms --argv0 --ro-bind-fd'\nfi\nexit 0\n",
    )?;
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(&fake_bwrap)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_bwrap, permissions)?;
    let fake_bwrap_marker = temp.path().join("fake-bwrap-ran");
    let target_marker = temp.path().join("target-ran");
    let mut command = command(temp.path(), "marker").arg(&target_marker);
    command.env.insert(OsString::from("PATH"), fake_bin.into());
    command.env.insert(
        OsString::from("SANDBOX_API_FAKE_BWRAP_MARKER"),
        fake_bwrap_marker.clone().into(),
    );
    let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert_eq!(fs::read(target_marker)?, b"target ran");
    assert!(!fake_bwrap_marker.exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn target_tmpdir_does_not_receive_linux_helper_state() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let target_tmpdir = temp.path().join("target-tmpdir");
    fs::create_dir(&target_tmpdir)?;
    let transient_mount_target = temp.path().join(".git");
    fs::write(&transient_mount_target, [])?;
    let mut command = command(temp.path(), "exit").arg("0");
    command
        .env
        .insert(OsString::from("TMPDIR"), target_tmpdir.clone().into());
    let request = SandboxRequest::new(
        command,
        writable_host_policy(temp.path()).read_only(transient_mount_target),
    );

    let (status, _, _) = run_and_collect(&runtime, request, /*stdin*/ None).await?;

    assert!(status.success());
    assert!(fs::read_dir(target_tmpdir)?.next().transpose()?.is_none());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn explicit_read_overlapping_writable_dev_is_rejected_before_target_launch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    for (name, policy) in [
        (
            "platform-minimal",
            SandboxPolicy::platform_minimal()
                .read_only("/")
                .read_write(temp.path())
                .network_unrestricted(),
        ),
        (
            "host-read-only",
            SandboxPolicy::host_read_only()
                .read_only("/dev")
                .read_write(temp.path())
                .network_unrestricted(),
        ),
    ] {
        let marker = temp.path().join(format!("target-ran-{name}"));
        let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

        let error = spawned_error(runtime.spawn(request).await);

        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy {
                feature: SandboxFeature::DeniedWritePaths,
                ..
            }
        ));
        assert!(!marker.exists());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dynamic_loader_environment_is_rejected_before_helper_launch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    for key in ["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH", "LD_FUTURE"] {
        let marker = temp.path().join(format!("target-ran-{key}"));
        let mut command = command(temp.path(), "marker").arg(&marker);
        command.env.insert(key.into(), "/untrusted/value".into());
        let request = SandboxRequest::new(command, writable_host_policy(temp.path()));

        let error = spawned_error(runtime.spawn(request).await);

        assert!(matches!(error, SandboxError::InvalidCommand { .. }));
        assert!(!marker.exists());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rules_overlapping_proc_are_rejected_before_target_launch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    for access in [PathAccess::Read, PathAccess::Write, PathAccess::Deny] {
        let marker = temp.path().join(format!("target-ran-{access:?}"));
        let policy = SandboxPolicy::host_read_only()
            .read_write(temp.path())
            .rule(PathRule::new("/proc", access))
            .network_unrestricted();
        let request = SandboxRequest::new(command(temp.path(), "marker").arg(&marker), policy);

        let error = spawned_error(runtime.spawn(request).await);

        assert!(matches!(error, SandboxError::UnsupportedPolicy { .. }));
        assert!(!marker.exists());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn writable_symlink_policy_conflicts_are_rejected_before_target_launch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime(&temp)?;
    let writable = temp.path().join("writable");
    let outside = temp.path().join("outside");
    let link = writable.join("link");
    fs::create_dir(&writable)?;
    fs::create_dir(&outside)?;
    std::os::unix::fs::symlink(&outside, &link)?;

    for access in [PathAccess::Read, PathAccess::Deny] {
        let marker = temp.path().join(format!("target-ran-{access:?}"));
        let policy = SandboxPolicy::host_read_only()
            .read_write(&writable)
            .rule(PathRule::new(&link, access))
            .network_unrestricted();
        let request = SandboxRequest::new(command(&writable, "marker").arg(&marker), policy);

        let error = spawned_error(runtime.spawn(request).await);

        assert!(matches!(error, SandboxError::UnsupportedPolicy { .. }));
        assert!(!marker.exists());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn current_executable_requires_the_startup_dispatch_hook() -> TestResult {
    let temp = tempfile::tempdir()?;
    let error = match SandboxRuntime::new(SandboxRuntimeConfig::new(temp.path().join("state"))) {
        Ok(_) => panic!("CurrentExecutable should require startup helper registration"),
        Err(error) => error,
    };

    assert!(matches!(error, SandboxError::BackendUnavailable { .. }));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn current_executable_helper_dispatches_and_normal_startup_returns() -> TestResult {
    let temp = tempfile::tempdir()?;
    let state_dir = temp.path().join("current-executable-state");
    fs::create_dir(&state_dir)?;

    let output = std::process::Command::new(fixture())
        .arg("current-executable")
        .arg(state_dir)
        .output()?;

    assert!(
        output.status.success(),
        "fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"current-executable-ok\n");
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn current_executable_helper_state_cannot_be_made_writable() -> TestResult {
    let temp = tempfile::tempdir()?;
    let state_dir = temp.path().join("current-executable-state");
    fs::create_dir(&state_dir)?;

    let output = std::process::Command::new(fixture())
        .arg("current-executable-state-write-rejected")
        .arg(state_dir)
        .output()?;

    assert!(
        output.status.success(),
        "fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"current-executable-state-write-rejected\n");
    Ok(())
}
