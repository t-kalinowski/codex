use crate::identity::SandboxCreds;
use crate::winutil::resolve_sid;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use std::ffi::c_void;
use std::ptr;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::DENY_ACCESS;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT;
use windows_sys::Win32::Security::Authorization::SET_ACCESS;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::CopySid;
use windows_sys::Win32::Security::CreateWellKnownSid;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::IsValidSid;
use windows_sys::Win32::Security::LOGON32_LOGON_NETWORK;
use windows_sys::Win32::Security::LOGON32_PROVIDER_DEFAULT;
use windows_sys::Win32::Security::LogonUserW;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::Security::WinCreatorOwnerRightsSid;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_ALL_ACCESS;
use windows_sys::Win32::System::Threading::THREAD_ALL_ACCESS;

struct LocalAcl(*mut ACL);

impl LocalAcl {
    fn as_ptr(&self) -> *mut ACL {
        self.0
    }
}

impl Drop for LocalAcl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

pub(super) struct HelperProcessSecurity {
    runner_sid: Vec<u8>,
    system_sid: Vec<u8>,
    owner_rights_sid: Vec<u8>,
}

impl HelperProcessSecurity {
    pub(super) fn prepare(creds: &SandboxCreds) -> Result<Self> {
        let mut runner_sid = current_process_user_sid().context("resolve runner TokenUser")?;
        let mut sandbox_sid = sandbox_user_sid(creds).context("resolve sandbox TokenUser")?;
        if unsafe {
            EqualSid(
                runner_sid.as_mut_ptr().cast(),
                sandbox_sid.as_mut_ptr().cast(),
            )
        } != 0
        {
            anyhow::bail!("standalone helper must use an identity distinct from the runner");
        }
        Self::from_runner_sid(runner_sid)
    }

    pub(super) fn from_trusted_runner_sid(mut runner_sid: Vec<u8>) -> Result<Self> {
        validate_sid(&mut runner_sid, "runner TokenUser")?;
        let mut helper_sid =
            current_process_user_sid().context("resolve standalone helper TokenUser")?;
        if unsafe {
            EqualSid(
                runner_sid.as_mut_ptr().cast(),
                helper_sid.as_mut_ptr().cast(),
            )
        } != 0
        {
            anyhow::bail!("standalone helper runner SID matches its sandbox TokenUser");
        }
        Self::from_runner_sid(runner_sid)
    }

    fn from_runner_sid(mut runner_sid: Vec<u8>) -> Result<Self> {
        validate_sid(&mut runner_sid, "runner TokenUser")?;
        Ok(Self {
            runner_sid,
            system_sid: resolve_sid("SYSTEM").context("resolve SYSTEM SID")?,
            owner_rights_sid: well_known_sid(WinCreatorOwnerRightsSid)
                .context("resolve OWNER RIGHTS SID")?,
        })
    }

    pub(super) fn runner_sid(&self) -> &[u8] {
        &self.runner_sid
    }

    pub(super) fn seal_process_and_initial_thread(
        &mut self,
        process: HANDLE,
        initial_thread: HANDLE,
    ) -> Result<()> {
        self.seal_runner_owned_object(process, PROCESS_ALL_ACCESS, "process")?;
        self.seal_runner_owned_object(initial_thread, THREAD_ALL_ACCESS, "initial thread")
    }

    pub(super) fn seal_helper_owned_thread(&mut self, thread: HANDLE) -> Result<()> {
        let dacl = self.build_dacl(THREAD_ALL_ACCESS, Some(THREAD_ALL_ACCESS))?;
        let security_code = unsafe {
            SetSecurityInfo(
                thread,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl.as_ptr(),
                ptr::null(),
            )
        };
        if security_code != ERROR_SUCCESS {
            anyhow::bail!(
                "SetSecurityInfo failed for standalone helper control thread: {security_code}"
            );
        }
        Ok(())
    }

    fn seal_runner_owned_object(&mut self, object: HANDLE, access: u32, label: &str) -> Result<()> {
        let dacl = self.build_dacl(access, None)?;
        let security_code = unsafe {
            SetSecurityInfo(
                object,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                self.runner_sid.as_mut_ptr().cast::<c_void>(),
                ptr::null_mut(),
                dacl.as_ptr(),
                ptr::null(),
            )
        };
        if security_code != ERROR_SUCCESS {
            anyhow::bail!("SetSecurityInfo failed for standalone helper {label}: {security_code}");
        }
        Ok(())
    }

    fn build_dacl(&mut self, access: u32, owner_deny_access: Option<u32>) -> Result<LocalAcl> {
        let mut entries = Vec::with_capacity(3);
        if let Some(owner_deny_access) = owner_deny_access {
            entries.push(explicit_access(
                self.owner_rights_sid.as_mut_ptr(),
                owner_deny_access,
                DENY_ACCESS,
            ));
        }
        entries.push(explicit_access(
            self.runner_sid.as_mut_ptr(),
            access,
            SET_ACCESS,
        ));
        if unsafe {
            EqualSid(
                self.runner_sid.as_mut_ptr().cast(),
                self.system_sid.as_mut_ptr().cast(),
            )
        } == 0
        {
            entries.push(explicit_access(
                self.system_sid.as_mut_ptr(),
                access,
                SET_ACCESS,
            ));
        }
        let mut dacl = ptr::null_mut();
        let acl_code = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                ptr::null(),
                &mut dacl,
            )
        };
        if acl_code != ERROR_SUCCESS || dacl.is_null() {
            if !dacl.is_null() {
                unsafe {
                    LocalFree(dacl as HLOCAL);
                }
            }
            if acl_code == ERROR_SUCCESS {
                anyhow::bail!("SetEntriesInAclW returned no standalone helper DACL");
            }
            anyhow::bail!("SetEntriesInAclW failed for standalone helper: {acl_code}");
        }
        Ok(LocalAcl(dacl))
    }
}

fn explicit_access(sid: *mut u8, access: u32, mode: i32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: mode,
        grfInheritance: 0,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    }
}

fn well_known_sid(sid_type: i32) -> Result<Vec<u8>> {
    let mut needed = 0;
    unsafe {
        CreateWellKnownSid(sid_type, ptr::null_mut(), ptr::null_mut(), &mut needed);
    }
    if needed == 0 {
        anyhow::bail!("CreateWellKnownSid size query failed: {}", unsafe {
            GetLastError()
        });
    }
    let mut sid = vec![0_u8; needed as usize];
    if unsafe {
        CreateWellKnownSid(
            sid_type,
            ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut needed,
        )
    } == 0
    {
        anyhow::bail!("CreateWellKnownSid failed: {}", unsafe { GetLastError() });
    }
    sid.truncate(needed as usize);
    Ok(sid)
}

fn validate_sid(sid: &mut [u8], label: &str) -> Result<()> {
    if sid.is_empty() || unsafe { IsValidSid(sid.as_mut_ptr().cast()) } == 0 {
        anyhow::bail!("standalone helper received an invalid {label} SID");
    }
    let length = unsafe { GetLengthSid(sid.as_mut_ptr().cast()) };
    if length == 0 || length as usize != sid.len() {
        anyhow::bail!("standalone helper received a non-canonical {label} SID");
    }
    Ok(())
}

fn current_process_user_sid() -> Result<Vec<u8>> {
    let mut token = 0;
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        anyhow::bail!("OpenProcessToken failed for current process: {}", unsafe {
            GetLastError()
        });
    }
    let result = token_user_sid(token);
    unsafe {
        CloseHandle(token);
    }
    result
}

fn sandbox_user_sid(creds: &SandboxCreds) -> Result<Vec<u8>> {
    let username = to_wide(&creds.username);
    let domain = to_wide(".");
    let password = to_wide(&creds.password);
    let mut token = 0;
    if unsafe {
        LogonUserW(
            username.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON32_LOGON_NETWORK,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    } == 0
    {
        anyhow::bail!(
            "LogonUserW failed for standalone sandbox identity: {}",
            unsafe { GetLastError() }
        );
    }
    let result = token_user_sid(token);
    unsafe {
        CloseHandle(token);
    }
    result
}

fn token_user_sid(token: HANDLE) -> Result<Vec<u8>> {
    let mut needed = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            ptr::null_mut(),
            /*tokeninformationlength*/ 0,
            &mut needed,
        );
    }
    if needed == 0 {
        anyhow::bail!(
            "GetTokenInformation(TokenUser) size query failed: {}",
            unsafe { GetLastError() }
        );
    }
    let mut token_user = vec![0_u8; needed as usize];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            token_user.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        anyhow::bail!("GetTokenInformation(TokenUser) failed: {}", unsafe {
            GetLastError()
        });
    }
    let token_user = unsafe { ptr::read_unaligned(token_user.as_ptr().cast::<TOKEN_USER>()) };
    let sid_len = unsafe { GetLengthSid(token_user.User.Sid) };
    if sid_len == 0 {
        anyhow::bail!("GetLengthSid(TokenUser) failed: {}", unsafe {
            GetLastError()
        });
    }
    let mut sid = vec![0_u8; sid_len as usize];
    if unsafe { CopySid(sid_len, sid.as_mut_ptr().cast(), token_user.User.Sid) } == 0 {
        anyhow::bail!("CopySid(TokenUser) failed: {}", unsafe { GetLastError() });
    }
    Ok(sid)
}
