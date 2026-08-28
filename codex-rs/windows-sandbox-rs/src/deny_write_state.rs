use crate::acl::add_deny_write_ace;
use crate::acl::revoke_ace_checked;
use crate::deny_read_acl::lexical_path_key;
use crate::setup::sandbox_dir;
use crate::token::convert_string_sid_to_sid;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;

const DENY_WRITE_ACL_STATE_FILE: &str = "deny_write_acl_state.json";

#[derive(Default, Deserialize, Serialize)]
struct PersistentDenyWriteAclState {
    principals: BTreeMap<String, Vec<PathBuf>>,
}

/// Reconciles the complete persistent deny-write ACE set owned by standalone
/// workspace capability SIDs.
///
/// The setup helper leaves ACLs in place across target generations. Persisting
/// the paths per capability SID lets a later policy revoke carveouts that are
/// no longer requested without disturbing denials owned by another SID.
///
/// # Safety
/// Every principal key must be a valid Windows SID string.
pub unsafe fn sync_persistent_deny_write_acls(
    codex_home: &Path,
    desired_principals: &BTreeMap<String, Vec<PathBuf>>,
) -> Result<usize> {
    let state_path = sandbox_dir(codex_home).join(DENY_WRITE_ACL_STATE_FILE);
    let previous = load_state(&state_path)?;
    let mut applied = BTreeMap::new();

    for (principal_sid, desired_paths) in desired_principals {
        let psid = unsafe { convert_string_sid_to_sid(principal_sid) }
            .ok_or_else(|| anyhow::anyhow!("convert deny-write capability SID failed"))?;
        let result = apply_paths(desired_paths, psid);
        unsafe {
            LocalFree(psid as HLOCAL);
        }
        applied.insert(principal_sid.clone(), result?);
    }

    for (principal_sid, previous_paths) in previous.principals {
        let desired_keys = applied
            .get(&principal_sid)
            .into_iter()
            .flatten()
            .map(|path| lexical_path_key(path))
            .collect::<HashSet<_>>();
        let stale_paths = previous_paths
            .into_iter()
            .filter(|path| !desired_keys.contains(&lexical_path_key(path)))
            .collect::<Vec<_>>();
        if stale_paths.is_empty() {
            continue;
        }
        let psid = unsafe { convert_string_sid_to_sid(&principal_sid) }
            .ok_or_else(|| anyhow::anyhow!("convert stale deny-write capability SID failed"))?;
        let result = (|| -> Result<()> {
            for path in stale_paths {
                if path.exists() {
                    unsafe { revoke_ace_checked(&path, psid) }.with_context(|| {
                        format!("revoke stale deny-write ACE from {}", path.display())
                    })?;
                }
            }
            Ok(())
        })();
        unsafe {
            LocalFree(psid as HLOCAL);
        }
        result?;
    }

    applied.retain(|_, paths| !paths.is_empty());
    let applied_count = applied.values().map(Vec::len).sum();
    store_state(
        &state_path,
        &PersistentDenyWriteAclState {
            principals: applied,
        },
    )?;
    Ok(applied_count)
}

unsafe fn apply_paths(paths: &[PathBuf], psid: *mut std::ffi::c_void) -> Result<Vec<PathBuf>> {
    let mut applied = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        let key = lexical_path_key(path);
        if !seen.insert(key) {
            continue;
        }
        unsafe { add_deny_write_ace(path, psid) }
            .with_context(|| format!("apply deny-write ACE to {}", path.display()))?;
        applied.push(path.clone());
    }
    Ok(applied)
}

fn load_state(path: &Path) -> Result<PersistentDenyWriteAclState> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse deny-write ACL state {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistentDenyWriteAclState::default())
        }
        Err(err) => {
            Err(err).with_context(|| format!("read deny-write ACL state {}", path.display()))
        }
    }
}

fn store_state(path: &Path, state: &PersistentDenyWriteAclState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("serialize deny-write ACL state")?;
    std::fs::write(path, bytes)
        .with_context(|| format!("write deny-write ACL state {}", path.display()))
}
