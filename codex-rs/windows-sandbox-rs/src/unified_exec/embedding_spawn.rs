use crate::desktop::LaunchDesktop;
use crate::proc_thread_attr::ProcThreadAttributeList;
use crate::unified_exec::WindowsSandboxEmbeddingStdio;
use crate::unified_exec::WindowsSandboxEmbeddingStdioMode;
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
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOEXW;

pub(crate) struct EmbeddingStdioSpawnHandles {
    pub(crate) process: PROCESS_INFORMATION,
    pub(crate) job: Arc<JobObject>,
    pub(crate) stdin_write: Option<HANDLE>,
    pub(crate) stdout_read: Option<HANDLE>,
    pub(crate) stderr_read: Option<HANDLE>,
    pub(crate) desktop: LaunchDesktop,
}

#[derive(Clone, Copy)]
enum StandardStream {
    Stdin,
    Stdout,
    Stderr,
}

impl StandardStream {
    fn native_id(self) -> u32 {
        match self {
            Self::Stdin => STD_INPUT_HANDLE,
            Self::Stdout => STD_OUTPUT_HANDLE,
            Self::Stderr => STD_ERROR_HANDLE,
        }
    }

    fn null_access(self) -> u32 {
        match self {
            Self::Stdin => GENERIC_READ,
            Self::Stdout | Self::Stderr => GENERIC_WRITE,
        }
    }
}

struct OwnedWinHandle(HANDLE);

impl OwnedWinHandle {
    fn new(handle: HANDLE) -> Result<Self> {
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            Err(anyhow!("invalid Windows handle"))
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_raw(mut self) -> HANDLE {
        let handle = self.0;
        self.0 = 0;
        handle
    }
}

impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct PreparedStream {
    child_handle: HANDLE,
    _owned_child_handle: Option<OwnedWinHandle>,
    parent_pipe: Option<OwnedWinHandle>,
}

impl PreparedStream {
    fn inherit(stream: StandardStream) -> Result<Self> {
        let source = unsafe { GetStdHandle(stream.native_id()) };
        if source == 0 || source == INVALID_HANDLE_VALUE {
            return Ok(Self {
                child_handle: 0,
                _owned_child_handle: None,
                parent_pipe: None,
            });
        }

        let current_process = unsafe { GetCurrentProcess() };
        let mut duplicate = 0;
        if unsafe {
            DuplicateHandle(
                current_process,
                source,
                current_process,
                &mut duplicate,
                /*dw_desired_access*/ 0,
                /*b_inherit_handle*/ 1,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(anyhow!(
                "DuplicateHandle failed for inherited standard stream: {}",
                unsafe { GetLastError() }
            ));
        }
        let duplicate = OwnedWinHandle::new(duplicate)?;
        Ok(Self {
            child_handle: duplicate.raw(),
            _owned_child_handle: Some(duplicate),
            parent_pipe: None,
        })
    }

    fn pipe(stream: StandardStream) -> Result<Self> {
        let mut read = 0;
        let mut write = 0;
        if unsafe { CreatePipe(&mut read, &mut write, ptr::null_mut(), 0) } == 0 {
            return Err(anyhow!(
                "CreatePipe failed for standard stream: {}",
                unsafe { GetLastError() }
            ));
        }
        let (child, parent) = match stream {
            StandardStream::Stdin => (read, write),
            StandardStream::Stdout | StandardStream::Stderr => (write, read),
        };
        let child = OwnedWinHandle::new(child)?;
        let parent = OwnedWinHandle::new(parent)?;
        make_inheritable(child.raw())?;
        Ok(Self {
            child_handle: child.raw(),
            _owned_child_handle: Some(child),
            parent_pipe: Some(parent),
        })
    }

    fn null(stream: StandardStream) -> Result<Self> {
        let null_path = to_wide(r"\\.\NUL");
        let handle = unsafe {
            CreateFileW(
                null_path.as_ptr(),
                stream.null_access(),
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(anyhow!(
                "CreateFileW failed for NUL standard stream: {}",
                unsafe { GetLastError() }
            ));
        }
        let handle = OwnedWinHandle::new(handle)?;
        make_inheritable(handle.raw())?;
        Ok(Self {
            child_handle: handle.raw(),
            _owned_child_handle: Some(handle),
            parent_pipe: None,
        })
    }

    fn prepare(mode: WindowsSandboxEmbeddingStdioMode, stream: StandardStream) -> Result<Self> {
        match mode {
            WindowsSandboxEmbeddingStdioMode::Inherit => Self::inherit(stream),
            WindowsSandboxEmbeddingStdioMode::Pipe => Self::pipe(stream),
            WindowsSandboxEmbeddingStdioMode::Null => Self::null(stream),
        }
    }

    fn take_parent_pipe(&mut self) -> Option<HANDLE> {
        self.parent_pipe.take().map(OwnedWinHandle::into_raw)
    }
}

struct ChildStdioHandles {
    stdin: HANDLE,
    stdout: HANDLE,
    stderr: HANDLE,
}

pub(crate) fn spawn_embedding_process_with_stdio(
    token: HANDLE,
    capability_sids: &[*mut c_void],
    command: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    stdio: WindowsSandboxEmbeddingStdio,
) -> Result<EmbeddingStdioSpawnHandles> {
    let mut stdin = PreparedStream::prepare(stdio.stdin, StandardStream::Stdin)?;
    let mut stdout = PreparedStream::prepare(stdio.stdout, StandardStream::Stdout)?;
    let mut stderr = PreparedStream::prepare(stdio.stderr, StandardStream::Stderr)?;
    let (process, job, desktop) = unsafe {
        create_embedding_process(
            token,
            capability_sids,
            command,
            cwd,
            env_map,
            ChildStdioHandles {
                stdin: stdin.child_handle,
                stdout: stdout.child_handle,
                stderr: stderr.child_handle,
            },
        )
    }?;

    Ok(EmbeddingStdioSpawnHandles {
        process,
        job,
        stdin_write: stdin.take_parent_pipe(),
        stdout_read: stdout.take_parent_pipe(),
        stderr_read: stderr.take_parent_pipe(),
        desktop,
    })
}

fn make_inheritable(handle: HANDLE) -> Result<()> {
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        Err(anyhow!(
            "SetHandleInformation failed for standard stream: {}",
            unsafe { GetLastError() }
        ))
    } else {
        Ok(())
    }
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
    let mut inherited_handles = [stdio.stdin, stdio.stdout, stdio.stderr]
        .into_iter()
        .filter(|handle| *handle != 0 && *handle != INVALID_HANDLE_VALUE)
        .collect::<Vec<_>>();
    inherited_handles.sort_unstable();
    inherited_handles.dedup();
    let inherit_handles = !inherited_handles.is_empty();
    let mut attributes = ProcThreadAttributeList::new(if inherit_handles { 2 } else { 1 })?;
    attributes.set_job(job.as_raw_handle() as HANDLE)?;
    if inherit_handles {
        attributes.set_handle_list(inherited_handles)?;
    }

    let mut startup_info: STARTUPINFOEXW = std::mem::zeroed();
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info.StartupInfo.lpDesktop = desktop.startup_info_desktop();
    if inherit_handles {
        startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    }
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
        if inherit_handles { 1 } else { 0 },
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
