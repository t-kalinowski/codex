use crate::acl::add_deny_write_ace;
use crate::acl::ensure_allow_mask_aces;
use crate::acl::ensure_allow_write_aces;
use crate::allow::AllowDenyPaths;
use crate::allow::compute_allow_paths_for_permissions;
use crate::cap::workspace_write_root_contains_path;
use crate::cap::workspace_write_root_overlaps_path;
use crate::cap::workspace_write_root_specificity;
use crate::embedding_acl_mutex::lock_embedding_acl_mutations;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::spawn_prep::LegacyAclSids;
use crate::spawn_prep::RootCapabilitySid;
use crate::token::LocalSid;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SE_WINDOW_OBJECT;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::System::StationsAndDesktops::GetProcessWindowStation;

const SE_KERNEL_OBJECT: u32 = 6;
const SET_ACCESS: i32 = 2;
const REVOKE_ACCESS: i32 = 4;
const PATH_ACE_INHERITANCE: u32 = 0x2 | 0x1;
const WINSTA_ALL_ACCESS: u32 = 0x37f;

#[derive(Default)]
pub(crate) struct EmbeddingAclLease {
    path_aces: Vec<(PathBuf, String)>,
    null_device_aces: Vec<String>,
    window_station_aces: Vec<String>,
    serialize_cleanup: bool,
}

impl EmbeddingAclLease {
    fn record_path_ace(&mut self, path: &Path, sid: &str) {
        self.path_aces.push((path.to_path_buf(), sid.to_string()));
    }

    fn record_null_device_ace(&mut self, sid: &str) {
        self.null_device_aces.push(sid.to_string());
    }

    fn record_window_station_ace(&mut self, sid: &str) {
        self.window_station_aces.push(sid.to_string());
    }
}

impl Drop for EmbeddingAclLease {
    fn drop(&mut self) {
        let _mutation_guard = if self.serialize_cleanup {
            let Ok(guard) = lock_embedding_acl_mutations() else {
                return;
            };
            Some(guard)
        } else {
            None
        };
        for sid in self.window_station_aces.drain(..).rev() {
            if let Ok(sid) = LocalSid::from_string(&sid) {
                unsafe {
                    let _ = update_window_station_ace(sid.as_ptr(), REVOKE_ACCESS);
                }
            }
        }
        for sid in self.null_device_aces.drain(..).rev() {
            if let Ok(sid) = LocalSid::from_string(&sid) {
                unsafe {
                    let _ = update_null_device_ace(sid.as_ptr(), REVOKE_ACCESS);
                }
            }
        }
        for (path, sid) in self.path_aces.drain(..).rev() {
            if let Ok(sid) = LocalSid::from_string(&sid) {
                unsafe {
                    let _ = revoke_path_ace(&path, sid.as_ptr());
                }
            }
        }
    }
}

pub(crate) fn apply_embedding_acl_rules(
    permissions: &ResolvedWindowsSandboxPermissions,
    current_dir: &Path,
    env_map: &HashMap<String, String>,
    additional_deny_write_paths: &[PathBuf],
    acl_sids: LegacyAclSids<'_>,
) -> Result<EmbeddingAclLease> {
    let _mutation_guard = lock_embedding_acl_mutations()?;
    let AllowDenyPaths { allow, mut deny } =
        compute_allow_paths_for_permissions(permissions, current_dir, env_map);
    let mut lease = EmbeddingAclLease::default();
    unsafe {
        for path in additional_deny_write_paths {
            if !path.exists() {
                std::fs::create_dir_all(path)
                    .with_context(|| format!("create deny-write path {}", path.display()))?;
            }
            deny.insert(path.clone());
        }
        if let Some(readonly_sid) = acl_sids.readonly_sid {
            let sid_string = acl_sids
                .readonly_sid_str
                .ok_or_else(|| anyhow!("readonly capability SID string missing"))?;
            for path in &allow {
                let added = ensure_allow_mask_aces(
                    path,
                    &[readonly_sid.as_ptr()],
                    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
                )
                .with_context(|| format!("add readonly allow ACE to {}", path.display()))?;
                if added {
                    lease.record_path_ace(path, sid_string);
                }
            }
        } else {
            for path in &allow {
                let root_sid = matching_root_capability(path, acl_sids.write_root_sids)
                    .ok_or_else(|| {
                        anyhow!(
                            "no write capability SID is available for requested root {}",
                            path.display()
                        )
                    })?;
                let added =
                    ensure_allow_write_aces(path, &[root_sid.sid.as_ptr()]).with_context(|| {
                        format!("add writable-root allow ACE to {}", path.display())
                    })?;
                if added {
                    lease.record_path_ace(path, &root_sid.sid_str);
                }
            }
        }
        for path in &deny {
            for root_sid in deny_root_capabilities_for_path(path, acl_sids.write_root_sids) {
                let added = add_deny_write_ace(path, root_sid.sid.as_ptr())
                    .with_context(|| format!("add deny-write ACE to {}", path.display()))?;
                if added {
                    lease.record_path_ace(path, &root_sid.sid_str);
                }
            }
        }
        for root_sid in acl_sids.write_root_sids {
            if update_null_device_ace(root_sid.sid.as_ptr(), SET_ACCESS)? {
                lease.record_null_device_ace(&root_sid.sid_str);
            }
            if update_window_station_ace(root_sid.sid.as_ptr(), SET_ACCESS)? {
                lease.record_window_station_ace(&root_sid.sid_str);
            }
        }
        if let Some(readonly_sid) = acl_sids.readonly_sid {
            let sid_string = acl_sids
                .readonly_sid_str
                .ok_or_else(|| anyhow!("readonly capability SID string missing"))?;
            if update_null_device_ace(readonly_sid.as_ptr(), SET_ACCESS)? {
                lease.record_null_device_ace(sid_string);
            }
            if update_window_station_ace(readonly_sid.as_ptr(), SET_ACCESS)? {
                lease.record_window_station_ace(sid_string);
            }
        }
    }
    lease.serialize_cleanup = true;
    Ok(lease)
}

fn matching_root_capability<'a>(
    path: &Path,
    root_sids: &'a [RootCapabilitySid],
) -> Option<&'a RootCapabilitySid> {
    root_sids
        .iter()
        .filter(|root_sid| workspace_write_root_contains_path(&root_sid.root, path))
        .max_by_key(|root_sid| workspace_write_root_specificity(&root_sid.root))
}

fn deny_root_capabilities_for_path<'a>(
    path: &Path,
    root_sids: &'a [RootCapabilitySid],
) -> Vec<&'a RootCapabilitySid> {
    let matching_root_sids = root_sids
        .iter()
        .filter(|root_sid| workspace_write_root_overlaps_path(&root_sid.root, path))
        .collect::<Vec<_>>();
    if matching_root_sids.is_empty() {
        root_sids.iter().collect()
    } else {
        matching_root_sids
    }
}

unsafe fn revoke_path_ace(path: &Path, sid: *mut c_void) -> Result<()> {
    let mut security_descriptor = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let code = GetNamedSecurityInfoW(
        to_wide(path).as_ptr(),
        1,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return Err(anyhow!("GetNamedSecurityInfoW failed: {code}"));
    }
    let result = update_acl(
        dacl,
        sid,
        REVOKE_ACCESS,
        0,
        PATH_ACE_INHERITANCE,
        |new_dacl| {
            SetNamedSecurityInfoW(
                to_wide(path).as_ptr() as *mut u16,
                1,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            )
        },
    );
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
    result.map(|_| ())
}

unsafe fn update_null_device_ace(sid: *mut c_void, access_mode: i32) -> Result<bool> {
    let desired = 0x0002_0000 | 0x0004_0000; // READ_CONTROL | WRITE_DAC
    let handle = CreateFileW(
        to_wide(r"\\.\NUL").as_ptr(),
        desired,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        std::ptr::null_mut(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        0,
    );
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut security_descriptor = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let code = GetSecurityInfo(
        handle,
        SE_KERNEL_OBJECT as i32,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    let result = if code == ERROR_SUCCESS {
        update_acl(
            dacl,
            sid,
            access_mode,
            if access_mode == REVOKE_ACCESS {
                0
            } else {
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE
            },
            0,
            |new_dacl| {
                SetSecurityInfo(
                    handle,
                    SE_KERNEL_OBJECT as i32,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    new_dacl,
                    std::ptr::null_mut(),
                )
            },
        )
    } else {
        Err(anyhow!("GetSecurityInfo failed for NUL: {code}"))
    };
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
    CloseHandle(handle);
    result
}

unsafe fn update_window_station_ace(sid: *mut c_void, access_mode: i32) -> Result<bool> {
    let handle = GetProcessWindowStation();
    if handle == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut security_descriptor = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let code = GetSecurityInfo(
        handle,
        SE_WINDOW_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    let result = if code == ERROR_SUCCESS {
        update_acl(
            dacl,
            sid,
            access_mode,
            if access_mode == REVOKE_ACCESS {
                0
            } else {
                WINSTA_ALL_ACCESS
            },
            0,
            |new_dacl| {
                SetSecurityInfo(
                    handle,
                    SE_WINDOW_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    new_dacl,
                    std::ptr::null_mut(),
                )
            },
        )
    } else {
        Err(anyhow!("GetSecurityInfo failed for Winsta0: {code}"))
    };
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
    result
}

unsafe fn update_acl(
    dacl: *mut ACL,
    sid: *mut c_void,
    access_mode: i32,
    access_permissions: u32,
    inheritance: u32,
    set_security: impl FnOnce(*mut ACL) -> u32,
) -> Result<bool> {
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let explicit = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_permissions,
        grfAccessMode: access_mode,
        grfInheritance: inheritance,
        Trustee: trustee,
    };
    let mut new_dacl = std::ptr::null_mut();
    let update_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
    if update_code != ERROR_SUCCESS {
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
        return Err(anyhow!("SetEntriesInAclW failed: {update_code}"));
    }
    let set_code = set_security(new_dacl);
    if !new_dacl.is_null() {
        LocalFree(new_dacl as HLOCAL);
    }
    if set_code == ERROR_SUCCESS {
        Ok(true)
    } else {
        Err(anyhow!("setting the updated ACL failed: {set_code}"))
    }
}
