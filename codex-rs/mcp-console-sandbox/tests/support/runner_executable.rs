use codex_utils_cargo_bin::cargo_bin;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

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
            path: cargo_bin("mcp-console-sandbox").expect("runner binary"),
            _staging_directory: None,
        };
        executable
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(target_os = "linux")]
    pub(super) fn stage_linux(include_bwrap: bool) -> Self {
        let companion = include_bwrap.then(|| cargo_bin("bwrap").expect("workspace bwrap binary"));
        Self::stage_linux_with_companion(companion.as_deref())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn stage_linux_with_companion(companion: Option<&Path>) -> Self {
        let staging_directory = TempDir::new().expect("runner staging directory");
        let path = staging_directory.path().join("mcp-console-sandbox");
        copy_executable(
            &cargo_bin("mcp-console-sandbox").expect("runner binary"),
            &path,
        );
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

#[cfg(target_os = "linux")]
fn copy_executable(source: &Path, destination: &Path) {
    std::fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "copy executable {} to {}: {error}",
            source.display(),
            destination.display()
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
