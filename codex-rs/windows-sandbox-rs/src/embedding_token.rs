use crate::cap::load_or_create_cap_sids;
use crate::spawn_prep::RootCapabilitySid;
use crate::spawn_prep::root_capability_sids;
use crate::token::LocalSid;
use crate::token::enable_single_privilege;
use crate::token::get_current_token_for_restriction;
use crate::token::get_logon_sid_bytes;
use crate::token::set_default_dacl;
use crate::token::world_sid;
use anyhow::Result;
use anyhow::anyhow;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::CreateRestrictedToken;
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;

const DISABLE_MAX_PRIVILEGE: u32 = 0x01;
const LUA_TOKEN: u32 = 0x04;
const WRITE_RESTRICTED: u32 = 0x08;

pub(crate) struct EmbeddingSessionSecurity {
    pub(crate) h_token: HANDLE,
    pub(crate) readonly_sid: Option<LocalSid>,
    pub(crate) readonly_sid_str: Option<String>,
    pub(crate) write_root_sids: Vec<RootCapabilitySid>,
}

pub(crate) fn prepare_embedding_session_security(
    uses_write_capabilities: bool,
    state_dir: &Path,
    cwd: &Path,
    capability_roots: impl IntoIterator<Item = PathBuf>,
) -> Result<EmbeddingSessionSecurity> {
    let (readonly_sid, readonly_sid_str, write_root_sids) = if uses_write_capabilities {
        let write_root_sids = root_capability_sids(state_dir, cwd, capability_roots)?;
        if write_root_sids.is_empty() {
            anyhow::bail!("workspace-write sandbox has no writable root capability SIDs");
        }
        (None, None, write_root_sids)
    } else {
        let caps = load_or_create_cap_sids(state_dir)?;
        let readonly_sid = LocalSid::from_string(&caps.readonly)?;
        (Some(readonly_sid), Some(caps.readonly), Vec::new())
    };
    let cap_ptrs = if let Some(readonly_sid) = &readonly_sid {
        vec![readonly_sid.as_ptr()]
    } else {
        write_root_sids
            .iter()
            .map(|root| root.sid.as_ptr())
            .collect()
    };

    let h_token = unsafe {
        let base_token = get_current_token_for_restriction()?;
        let result = create_embedding_token_with_caps_from(base_token, &cap_ptrs);
        CloseHandle(base_token);
        result?
    };
    Ok(EmbeddingSessionSecurity {
        h_token,
        readonly_sid,
        readonly_sid_str,
        write_root_sids,
    })
}

/// Creates a write-restricted token whose restricting list contains only the
/// per-spawn capability SIDs.
///
/// Logon and Everyone remain in the token's default DACL so objects created by
/// the child can pass the normal access check. They are deliberately excluded
/// from the restricting list so host ACLs granted to either SID cannot widen
/// the embedding policy's writable roots.
///
/// # Safety
/// `base_token` must be a valid primary token and every capability pointer must
/// remain valid for this call. The caller must close the returned handle.
unsafe fn create_embedding_token_with_caps_from(
    base_token: HANDLE,
    capability_sids: &[*mut c_void],
) -> Result<HANDLE> {
    if capability_sids.is_empty() {
        return Err(anyhow!("no capability SIDs provided"));
    }
    let mut entries = capability_sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: *sid,
            Attributes: 0,
        })
        .collect::<Vec<_>>();
    let mut new_token = 0;
    let flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED;
    let ok = CreateRestrictedToken(
        base_token,
        flags,
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
        entries.len() as u32,
        entries.as_mut_ptr(),
        &mut new_token,
    );
    if ok == 0 {
        return Err(anyhow!("CreateRestrictedToken failed: {}", GetLastError()));
    }

    let result = (|| {
        let mut logon_sid = get_logon_sid_bytes(base_token)?;
        let mut everyone_sid = world_sid()?;
        let mut default_dacl_sids = Vec::with_capacity(capability_sids.len() + 2);
        default_dacl_sids.push(logon_sid.as_mut_ptr() as *mut c_void);
        default_dacl_sids.push(everyone_sid.as_mut_ptr() as *mut c_void);
        default_dacl_sids.extend_from_slice(capability_sids);
        set_default_dacl(new_token, &default_dacl_sids)?;
        enable_single_privilege(new_token, "SeChangeNotifyPrivilege")
    })();
    if let Err(error) = result {
        CloseHandle(new_token);
        return Err(error);
    }
    Ok(new_token)
}
