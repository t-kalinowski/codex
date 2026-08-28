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
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT;
use windows_sys::Win32::Security::Authorization::SET_ACCESS;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::CopySid;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::LOGON32_LOGON_NETWORK;
use windows_sys::Win32::Security::LOGON32_PROVIDER_DEFAULT;
use windows_sys::Win32::Security::LogonUserW;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_ALL_ACCESS;

pub(super) struct HelperProcessSecurity {
    runner_sid: Vec<u8>,
    system_sid: Vec<u8>,
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
        Ok(Self {
            runner_sid,
            system_sid: resolve_sid("SYSTEM").context("resolve SYSTEM SID")?,
        })
    }

    pub(super) fn apply(&mut self, process: HANDLE) -> Result<()> {
        let mut trustee_sids = vec![self.runner_sid.as_mut_ptr()];
        if unsafe {
            EqualSid(
                self.runner_sid.as_mut_ptr().cast(),
                self.system_sid.as_mut_ptr().cast(),
            )
        } == 0
        {
            trustee_sids.push(self.system_sid.as_mut_ptr());
        }
        let entries = trustee_sids
            .into_iter()
            .map(|sid| EXPLICIT_ACCESS_W {
                grfAccessPermissions: PROCESS_ALL_ACCESS,
                grfAccessMode: SET_ACCESS,
                grfInheritance: 0,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: ptr::null_mut(),
                    MultipleTrusteeOperation: 0,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: sid.cast(),
                },
            })
            .collect::<Vec<_>>();
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

        let security_code = unsafe {
            SetSecurityInfo(
                process,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                self.runner_sid.as_mut_ptr().cast::<c_void>(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };
        if !dacl.is_null() {
            unsafe {
                LocalFree(dacl as HLOCAL);
            }
        }
        if security_code != ERROR_SUCCESS {
            anyhow::bail!("SetSecurityInfo failed for standalone helper: {security_code}");
        }
        Ok(())
    }
}

fn current_process_user_sid() -> Result<Vec<u8>> {
    let mut token = 0;
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        anyhow::bail!(
            "OpenProcessToken failed for runner: {}",
            unsafe { GetLastError() }
        );
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
        anyhow::bail!(
            "GetTokenInformation(TokenUser) failed: {}",
            unsafe { GetLastError() }
        );
    }
    let token_user = unsafe { ptr::read_unaligned(token_user.as_ptr().cast::<TOKEN_USER>()) };
    let sid_len = unsafe { GetLengthSid(token_user.User.Sid) };
    if sid_len == 0 {
        anyhow::bail!(
            "GetLengthSid(TokenUser) failed: {}",
            unsafe { GetLastError() }
        );
    }
    let mut sid = vec![0_u8; sid_len as usize];
    if unsafe { CopySid(sid_len, sid.as_mut_ptr().cast(), token_user.User.Sid) } == 0 {
        anyhow::bail!(
            "CopySid(TokenUser) failed: {}",
            unsafe { GetLastError() }
        );
    }
    Ok(sid)
}
