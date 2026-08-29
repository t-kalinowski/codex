use anyhow::Result;
use anyhow::bail;
use std::collections::HashMap;

pub type TargetEnvironment = HashMap<String, String>;

pub fn target_environment() -> Result<TargetEnvironment> {
    let mut environment = TargetEnvironment::new();
    for (key, value) in std::env::vars_os() {
        let key = key.into_string().map_err(|_| {
            anyhow::anyhow!("target environment names must be valid Unicode in protocol version 1")
        })?;
        if is_loader_affecting_key(&key) {
            bail!(
                "loader-affecting environment variable `{key}` is forbidden at the runner boundary"
            )
        }
        if is_runner_private_key(&key) {
            continue;
        }
        let value = value.into_string().map_err(|_| {
            anyhow::anyhow!(
                "target environment value for `{key}` must be valid Unicode in protocol version 1"
            )
        })?;
        environment.insert(key, value);
    }
    Ok(environment)
}

fn is_runner_private_key(key: &str) -> bool {
    const PREFIX: &str = "MCP_CONSOLE_SANDBOX_";
    if cfg!(windows) {
        key.get(..PREFIX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
    } else {
        key.starts_with(PREFIX)
    }
}

fn is_loader_affecting_key(key: &str) -> bool {
    cfg!(unix) && (key.starts_with("LD_") || key.starts_with("DYLD_"))
}
