use crate::winutil::to_wide;
use anyhow::Result;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::WAIT_ABANDONED;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const EMBEDDING_ACL_MUTEX_NAME: &str = "Local\\CodexSandboxEmbeddingAclV1";

pub(crate) struct EmbeddingAclMutexGuard {
    handle: HANDLE,
}

impl Drop for EmbeddingAclMutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

pub(crate) fn lock_embedding_acl_mutations() -> Result<EmbeddingAclMutexGuard> {
    let name = to_wide(EMBEDDING_ACL_MUTEX_NAME);
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if handle == 0 {
        return Err(anyhow::anyhow!("CreateMutexW failed: {}", unsafe {
            GetLastError()
        }));
    }
    let wait_result = unsafe { WaitForSingleObject(handle, INFINITE) };
    match wait_result {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(EmbeddingAclMutexGuard { handle }),
        WAIT_FAILED => {
            let error = unsafe { GetLastError() };
            unsafe {
                CloseHandle(handle);
            }
            Err(anyhow::anyhow!("WaitForSingleObject failed: {error}"))
        }
        result => {
            unsafe {
                CloseHandle(handle);
            }
            Err(anyhow::anyhow!(
                "WaitForSingleObject returned unexpected result: {result}"
            ))
        }
    }
}
