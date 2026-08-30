use codex_utils_cargo_bin::cargo_bin;
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::copy;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
#[cfg(target_os = "linux")]
use tempfile::NamedTempFile;
use tempfile::TempDir;

const TEST_RUNNER_ENV: &str = "MCP_CONSOLE_SANDBOX_TEST_RUNNER";
#[cfg(target_os = "linux")]
const TEST_BWRAP_ENV: &str = "MCP_CONSOLE_SANDBOX_TEST_BWRAP";

pub struct RunnerExecutable {
    path: PathBuf,
    _staging_directory: Option<TempDir>,
}

impl RunnerExecutable {
    pub fn packaged() -> Self {
        #[cfg(target_os = "linux")]
        let executable = Self::stage_linux(/*include_bwrap*/ true);
        #[cfg(not(target_os = "linux"))]
        let executable = Self {
            path: test_binary(TEST_RUNNER_ENV, "mcp-console-sandbox"),
            _staging_directory: None,
        };
        executable
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(target_os = "linux")]
    pub(super) fn stage_linux(include_bwrap: bool) -> Self {
        let companion = include_bwrap.then(|| test_binary(TEST_BWRAP_ENV, "bwrap"));
        Self::stage_linux_with_companion(companion.as_deref())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn stage_linux_with_companion(companion: Option<&Path>) -> Self {
        let staging_directory = TempDir::new().expect("runner staging directory");
        let path = staging_directory.path().join("mcp-console-sandbox");
        copy_executable(&test_binary(TEST_RUNNER_ENV, "mcp-console-sandbox"), &path);
        if let Some(companion) = companion {
            let resources = staging_directory.path().join("codex-resources");
            std::fs::create_dir(&resources).expect("create runner resources directory");
            copy_executable(companion, &resources.join("bwrap"));
        }
        Self {
            path,
            _staging_directory: Some(staging_directory),
        }
    }
}

fn test_binary(override_env: &str, cargo_name: &str) -> PathBuf {
    let path = std::env::var_os(override_env)
        .map(PathBuf::from)
        .unwrap_or_else(|| cargo_bin(cargo_name).expect("Cargo-built test binary"));
    assert!(
        path.is_absolute() && path.is_file(),
        "{override_env} must resolve to an absolute executable file: {}",
        path.display()
    );
    path
}

#[cfg(target_os = "linux")]
fn copy_executable(source: &Path, destination: &Path) {
    let mut source_file = File::open(source)
        .unwrap_or_else(|error| panic!("open executable {}: {error}", source.display()));
    let permissions = source_file
        .metadata()
        .unwrap_or_else(|error| panic!("inspect executable {}: {error}", source.display()))
        .permissions();
    let parent = destination
        .parent()
        .expect("staged executable must have a parent directory");
    let mut staged = NamedTempFile::new_in(parent).unwrap_or_else(|error| {
        panic!(
            "create temporary executable beside {}: {error}",
            destination.display()
        )
    });
    copy(&mut source_file, staged.as_file_mut()).unwrap_or_else(|error| {
        panic!(
            "copy executable {} to temporary file beside {}: {error}",
            source.display(),
            destination.display()
        )
    });
    staged
        .as_file()
        .set_permissions(permissions)
        .unwrap_or_else(|error| {
            panic!(
                "set executable permissions for {}: {error}",
                destination.display()
            )
        });
    let staged = staged.into_temp_path();
    staged
        .persist_noclobber(destination)
        .unwrap_or_else(|error| {
            panic!(
                "publish executable {} to {}: {}",
                source.display(),
                destination.display(),
                error.error
            )
        });
}

pub fn apply_sanitized_environment(command: &mut Command, overrides: &[(&str, &str)]) {
    apply_sanitized_native_environment(
        command,
        &overrides
            .iter()
            .map(|(key, value)| (OsString::from(*key), OsString::from(*value)))
            .collect::<Vec<_>>(),
    );
}

pub fn apply_sanitized_native_environment(
    command: &mut Command,
    overrides: &[(OsString, OsString)],
) {
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("LD_") || key.starts_with("DYLD_"))
        {
            command.env_remove(key);
        }
    }
    command.envs(overrides.iter().cloned());
}
