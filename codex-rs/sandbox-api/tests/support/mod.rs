use codex_sandbox_api::CommandSpec;
#[cfg(target_os = "linux")]
use codex_sandbox_api::LinuxHelper;
use codex_sandbox_api::SandboxPolicy;
use codex_sandbox_api::SandboxRuntime;
use codex_sandbox_api::SandboxRuntimeConfig;
use codex_sandbox_api::SandboxedOutput;
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

pub(crate) fn fixture() -> PathBuf {
    match codex_utils_cargo_bin::cargo_bin("sandbox-api-fixture") {
        Ok(path) => path,
        Err(error) => panic!("sandbox-api-fixture should be built for integration tests: {error}"),
    }
}

pub(crate) struct TestRuntime {
    runtime: SandboxRuntime,
    _state: TempDir,
}

impl Deref for TestRuntime {
    type Target = SandboxRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

pub(crate) fn runtime() -> TestResult<TestRuntime> {
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

pub(crate) fn command(cwd: &Path, mode: &str) -> CommandSpec {
    CommandSpec::new(fixture(), cwd, required_environment()).arg(mode)
}

pub(crate) fn writable_policy(path: &Path) -> SandboxPolicy {
    SandboxPolicy::host_read_only()
        .read_write(path)
        .network_unrestricted()
}

pub(crate) fn required_environment() -> BTreeMap<OsString, OsString> {
    let mut environment = BTreeMap::new();
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
        if let Some(value) = std::env::var_os(name) {
            environment.insert(OsString::from(name), value);
        }
    }
    environment
}

pub(crate) async fn collect(mut output: SandboxedOutput) -> TestResult<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = output.read_chunk().await? {
        bytes.extend(chunk);
    }
    Ok(bytes)
}
