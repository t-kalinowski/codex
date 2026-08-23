use crate::desktop::LaunchDesktop;
use crate::proc_thread_attr::ProcThreadAttributeList;
use crate::winutil::argv_to_command_line;
use crate::winutil::format_last_error;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_utils_pty::JobObject;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOEXW;

pub(crate) struct EmbeddingPipeSpawnHandles {
    pub(crate) process: PROCESS_INFORMATION,
    pub(crate) job: Arc<JobObject>,
    pub(crate) stdin_write: Option<HANDLE>,
    pub(crate) stdout_read: HANDLE,
    pub(crate) stderr_read: HANDLE,
    pub(crate) desktop: LaunchDesktop,
}

struct ChildStdioHandles {
    stdin: HANDLE,
    stdout: HANDLE,
    stderr: HANDLE,
}

pub(crate) fn spawn_embedding_process_with_pipes(
    token: HANDLE,
    capability_sids: &[*mut c_void],
    command: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    stdin_open: bool,
) -> Result<EmbeddingPipeSpawnHandles> {
    let mut stdin_read = 0;
    let mut stdin_write = 0;
    let mut stdout_read = 0;
    let mut stdout_write = 0;
    let mut stderr_read = 0;
    let mut stderr_write = 0;
    unsafe {
        if CreatePipe(&mut stdin_read, &mut stdin_write, ptr::null_mut(), 0) == 0 {
            return Err(anyhow!("CreatePipe stdin failed: {}", GetLastError()));
        }
        if CreatePipe(&mut stdout_read, &mut stdout_write, ptr::null_mut(), 0) == 0 {
            let error = GetLastError();
            CloseHandle(stdin_read);
            CloseHandle(stdin_write);
            return Err(anyhow!("CreatePipe stdout failed: {error}"));
        }
        if CreatePipe(&mut stderr_read, &mut stderr_write, ptr::null_mut(), 0) == 0 {
            let error = GetLastError();
            close_handles(&[stdin_read, stdin_write, stdout_read, stdout_write]);
            return Err(anyhow!("CreatePipe stderr failed: {error}"));
        }
    }

    let process = unsafe {
        create_embedding_process(
            token,
            capability_sids,
            command,
            cwd,
            env_map,
            ChildStdioHandles {
                stdin: stdin_read,
                stdout: stdout_write,
                stderr: stderr_write,
            },
        )
    };
    let (process, job, desktop) = match process {
        Ok(process) => process,
        Err(error) => {
            unsafe {
                close_handles(&[
                    stdin_read,
                    stdin_write,
                    stdout_read,
                    stdout_write,
                    stderr_read,
                    stderr_write,
                ]);
            }
            return Err(error);
        }
    };
    unsafe {
        close_handles(&[stdin_read, stdout_write, stderr_write]);
        if !stdin_open {
            CloseHandle(stdin_write);
        }
    }

    Ok(EmbeddingPipeSpawnHandles {
        process,
        job,
        stdin_write: stdin_open.then_some(stdin_write),
        stdout_read,
        stderr_read,
        desktop,
    })
}

unsafe fn create_embedding_process(
    token: HANDLE,
    capability_sids: &[*mut c_void],
    command: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    stdio: ChildStdioHandles,
) -> Result<(PROCESS_INFORMATION, Arc<JobObject>, LaunchDesktop)> {
    let command_line = argv_to_command_line(command);
    let mut command_line = to_wide(command_line);
    let environment = make_embedding_env_block(env_map);
    let desktop = LaunchDesktop::prepare_for_embedding(capability_sids)?;
    let job =
        Arc::new(JobObject::create_without_breakaway().context("create embedding process job")?);
    let inherited_handles = vec![stdio.stdin, stdio.stdout, stdio.stderr];
    for handle in &inherited_handles {
        if SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(anyhow!(
                "SetHandleInformation failed for stdio handle: {}",
                GetLastError()
            ));
        }
    }
    let mut attributes = ProcThreadAttributeList::new(2)?;
    attributes.set_job(job.as_raw_handle() as HANDLE)?;
    attributes.set_handle_list(inherited_handles)?;

    let mut startup_info: STARTUPINFOEXW = std::mem::zeroed();
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info.StartupInfo.lpDesktop = desktop.startup_info_desktop();
    startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup_info.StartupInfo.hStdInput = stdio.stdin;
    startup_info.StartupInfo.hStdOutput = stdio.stdout;
    startup_info.StartupInfo.hStdError = stdio.stderr;
    startup_info.lpAttributeList = attributes.as_mut_ptr();

    let mut process: PROCESS_INFORMATION = std::mem::zeroed();
    let cwd_wide = to_wide(cwd);
    let creation_flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    let created = CreateProcessAsUserW(
        token,
        std::ptr::null(),
        command_line.as_mut_ptr(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        1,
        creation_flags,
        environment.as_ptr() as *mut c_void,
        cwd_wide.as_ptr(),
        &startup_info.StartupInfo,
        &mut process,
    );
    if created == 0 {
        let error_code = GetLastError() as i32;
        let message = format!(
            "CreateProcessAsUserW failed: {} ({}) | cwd={} | env_u16_len={} | si_flags={} | creation_flags={}",
            error_code,
            format_last_error(error_code),
            cwd.display(),
            environment.len(),
            startup_info.StartupInfo.dwFlags,
            creation_flags,
        );
        return Err(std::io::Error::from_raw_os_error(error_code)).context(message);
    }
    Ok((process, job, desktop))
}

unsafe fn close_handles(handles: &[HANDLE]) {
    for handle in handles {
        CloseHandle(*handle);
    }
}

fn make_embedding_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut items = env.iter().collect::<Vec<_>>();
    items.sort_by(|(left_key, _), (right_key, _)| {
        left_key
            .to_uppercase()
            .cmp(&right_key.to_uppercase())
            .then(left_key.cmp(right_key))
    });
    let mut block = Vec::new();
    for (key, value) in items {
        let mut entry = to_wide(format!("{key}={value}"));
        entry.pop();
        block.extend_from_slice(&entry);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}
