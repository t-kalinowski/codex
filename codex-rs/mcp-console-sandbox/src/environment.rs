use codex_network_proxy::PROXY_ENV_KEYS;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;

pub type NativeEnvironment = BTreeMap<OsString, OsString>;

pub fn target_environment() -> Result<NativeEnvironment> {
    let mut environment = NativeEnvironment::new();
    for (key, value) in std::env::vars_os() {
        if is_loader_affecting_key(&key) {
            bail!(
                "loader-affecting environment variable `{}` is forbidden at the runner boundary",
                key.to_string_lossy()
            )
        }
        if !is_runner_private_key(&key) {
            environment.insert(key, value);
        }
    }
    Ok(environment)
}

pub fn apply_managed_proxy_environment(
    environment: &mut NativeEnvironment,
    updates: impl IntoIterator<Item = (String, String)>,
) -> Result<()> {
    #[cfg(windows)]
    let updates = collapse_windows_environment_updates(updates)?;
    #[cfg(not(windows))]
    let updates = updates
        .into_iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect::<NativeEnvironment>();

    environment.retain(|key, _| {
        !is_managed_proxy_key(key)
            && !updates
                .keys()
                .any(|update_key| environment_keys_equal(key, update_key))
    });
    environment.extend(updates);
    Ok(())
}

#[cfg(windows)]
fn collapse_windows_environment_updates(
    updates: impl IntoIterator<Item = (String, String)>,
) -> Result<NativeEnvironment> {
    let mut canonical = NativeEnvironment::new();
    for (key, value) in updates {
        if !key.is_ascii() {
            bail!("managed proxy environment name must be ASCII")
        }
        // Windows keys are case-insensitive; protocol documentation uses the
        // uppercase proxy spellings.
        let key = OsString::from(key.to_ascii_uppercase());
        match canonical.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(OsString::from(value));
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if entry.get() != OsStr::new(&value) {
                    bail!(
                        "managed proxy environment aliases disagree for `{}`",
                        entry.key().to_string_lossy()
                    )
                }
            }
        }
    }
    Ok(canonical)
}

#[cfg(windows)]
fn environment_keys_equal(left: &OsStr, right: &OsStr) -> bool {
    left.to_str()
        .zip(right.to_str())
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
}

#[cfg(not(windows))]
fn environment_keys_equal(left: &OsStr, right: &OsStr) -> bool {
    left == right
}

#[cfg(windows)]
pub(crate) fn remove_windows_setup_only_environment(environment: &mut NativeEnvironment) {
    environment.retain(|key, _| {
        !codex_windows_sandbox::is_windows_sandbox_standalone_setup_only_environment_variable(key)
    });
}

#[cfg(windows)]
fn is_runner_private_key(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    key.get(.."CARGO_BIN_EXE_".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("CARGO_BIN_EXE_"))
        || key
            .get(.."CODEX_".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("CODEX_"))
        || key
            .get(.."MCP_CONSOLE_SANDBOX_".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("MCP_CONSOLE_SANDBOX_"))
}

#[cfg(unix)]
fn is_runner_private_key(key: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let key = key.as_bytes();
    key.starts_with(b"CARGO_BIN_EXE_")
        || key.starts_with(b"CODEX_")
        || key.starts_with(b"MCP_CONSOLE_SANDBOX_")
}

#[cfg(not(any(unix, windows)))]
fn is_runner_private_key(key: &OsStr) -> bool {
    key.to_str().is_some_and(|key| {
        key.starts_with("CARGO_BIN_EXE_")
            || key.starts_with("CODEX_")
            || key.starts_with("MCP_CONSOLE_SANDBOX_")
    })
}

#[cfg(windows)]
fn is_managed_proxy_key(key: &OsStr) -> bool {
    key.to_str().is_some_and(|key| {
        PROXY_ENV_KEYS
            .iter()
            .any(|proxy_key| key.eq_ignore_ascii_case(proxy_key))
    })
}

#[cfg(not(windows))]
fn is_managed_proxy_key(key: &OsStr) -> bool {
    key.to_str()
        .is_some_and(|key| PROXY_ENV_KEYS.contains(&key))
}

fn is_loader_affecting_key(key: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let key = key.as_bytes();
        key.starts_with(b"LD_") || key.starts_with(b"DYLD_")
    }
    #[cfg(not(unix))]
    {
        let _ = key;
        false
    }
}
use anyhow::Result;
use anyhow::bail;
