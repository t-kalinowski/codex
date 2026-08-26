use crate::allow::compute_allow_paths_for_permissions;
use crate::embedding_acl::apply_embedding_acl_rules;
use crate::embedding_token::prepare_embedding_session_security;
use crate::process::read_handle_loop;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::spawn_prep::LegacyAclSids;
use crate::unified_exec::WindowsSandboxEmbeddingStdio;
use crate::unified_exec::WindowsSandboxEmbeddingStdioMode;
use crate::unified_exec::embedding_process::EmbeddingStdinRequest;
use crate::unified_exec::embedding_process::WindowsSandboxEmbeddingProcess;
use crate::unified_exec::embedding_spawn::EmbeddingStdioSpawnHandles;
use crate::unified_exec::embedding_spawn::spawn_embedding_process_with_stdio;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::JobObject;
use std::collections::HashMap;
use std::io;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(crate) async fn spawn_windows_sandbox_session_for_embedding(
    permission_profile: &PermissionProfile,
    state_dir: &AbsolutePathBuf,
    command: Vec<String>,
    cwd: &AbsolutePathBuf,
    env_map: HashMap<String, String>,
    additional_deny_write_paths: &[AbsolutePathBuf],
    stdio: WindowsSandboxEmbeddingStdio,
) -> Result<WindowsSandboxEmbeddingProcess> {
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            &[],
        )?;
    if permissions.should_apply_network_block() {
        anyhow::bail!("network restriction is unavailable for embedding sessions");
    }
    if !permissions.has_full_disk_read_access() {
        anyhow::bail!("restricted read access requires the elevated Windows sandbox backend");
    }

    std::fs::create_dir_all(state_dir)?;
    let session_state = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(state_dir)?;
    let session_state_path = session_state.path();
    let allow_paths = compute_allow_paths_for_permissions(&permissions, cwd, &env_map)
        .allow
        .into_iter()
        .collect::<Vec<_>>();
    let security = prepare_embedding_session_security(
        permissions.uses_write_capabilities_for_cwd(cwd, &env_map),
        session_state_path,
        cwd,
        allow_paths,
    )?;
    let deny_write_paths = additional_deny_write_paths
        .iter()
        .map(AbsolutePathBuf::to_path_buf)
        .collect::<Vec<_>>();
    let acl_lease = match apply_embedding_acl_rules(
        &permissions,
        cwd,
        &env_map,
        &deny_write_paths,
        LegacyAclSids {
            readonly_sid: security.readonly_sid.as_ref(),
            readonly_sid_str: security.readonly_sid_str.as_deref(),
            write_root_sids: &security.write_root_sids,
        },
    ) {
        Ok(lease) => lease,
        Err(error) => {
            unsafe {
                CloseHandle(security.h_token);
            }
            return Err(error);
        }
    };
    let capability_sids = security
        .readonly_sid
        .iter()
        .map(|sid| sid.as_ptr())
        .chain(
            security
                .write_root_sids
                .iter()
                .map(|root| root.sid.as_ptr()),
        )
        .collect::<Vec<_>>();

    let stdio_handles = match spawn_embedding_process_with_stdio(
        security.h_token,
        &capability_sids,
        &command,
        cwd,
        &env_map,
        stdio,
    ) {
        Ok(handles) => handles,
        Err(error) => {
            unsafe {
                CloseHandle(security.h_token);
            }
            return Err(error);
        }
    };
    Ok(finish_embedding_spawn(
        stdio_handles,
        security.h_token,
        stdio,
        session_state,
        acl_lease,
    ))
}

fn finish_embedding_spawn(
    handles: EmbeddingStdioSpawnHandles,
    token_handle: HANDLE,
    stdio: WindowsSandboxEmbeddingStdio,
    session_state: tempfile::TempDir,
    acl_lease: crate::embedding_acl::EmbeddingAclLease,
) -> WindowsSandboxEmbeddingProcess {
    let EmbeddingStdioSpawnHandles {
        process,
        job,
        stdin_write,
        stdout_read,
        stderr_read,
        desktop,
    } = handles;
    let (writer_tx, writer_handle) = match stdin_write {
        Some(stdin_write) => {
            let (writer_tx, writer_rx) = mpsc::channel(128);
            (
                Some(writer_tx),
                Some(spawn_input_writer(stdin_write, writer_rx)),
            )
        }
        None => (None, None),
    };
    let (stdout_rx, stdout_join) = output_transport(stdout_read);
    let (stderr_rx, stderr_join) = output_transport(stderr_read);
    let output_join = std::thread::spawn(move || {
        if let Some(stdout_join) = stdout_join {
            let _ = stdout_join.join();
        }
        if let Some(stderr_join) = stderr_join {
            let _ = stderr_join.join();
        }
    });
    let (driver_exit_tx, driver_exit_rx) = oneshot::channel();
    let process_handle = Arc::new(Mutex::new(Some(process.hProcess)));
    let wait_process_handle = Arc::clone(&process_handle);
    std::thread::spawn(move || {
        let _desktop = desktop;
        let mut exit_code = 1;
        unsafe {
            WaitForSingleObject(process.hProcess, INFINITE);
            GetExitCodeProcess(process.hProcess, &mut exit_code);
        }
        let _ = driver_exit_tx.send(exit_code as i32);
        let _ = output_join.join();
        unsafe {
            if process.hThread != 0 && process.hThread != INVALID_HANDLE_VALUE {
                CloseHandle(process.hThread);
            }
            if let Ok(mut handle) = wait_process_handle.lock()
                && let Some(handle) = handle.take()
            {
                CloseHandle(handle);
            }
            if token_handle != 0 && token_handle != INVALID_HANDLE_VALUE {
                CloseHandle(token_handle);
            }
        }
    });

    let terminate_job = Arc::clone(&job);
    let terminate_process_handle = Arc::clone(&process_handle);
    let terminator =
        Box::new(move || terminate_job_or_process(&terminate_job, &terminate_process_handle));
    debug_assert_eq!(
        writer_tx.is_some(),
        matches!(stdio.stdin, WindowsSandboxEmbeddingStdioMode::Pipe)
    );
    debug_assert_eq!(
        stdout_rx.is_some(),
        matches!(stdio.stdout, WindowsSandboxEmbeddingStdioMode::Pipe)
    );
    debug_assert_eq!(
        stderr_rx.is_some(),
        matches!(stdio.stderr, WindowsSandboxEmbeddingStdioMode::Pipe)
    );
    WindowsSandboxEmbeddingProcess::new(
        writer_tx,
        stdout_rx,
        stderr_rx,
        driver_exit_rx,
        terminator,
        writer_handle,
        session_state,
        acl_lease,
    )
}

fn output_transport(
    output_read: Option<HANDLE>,
) -> (
    Option<mpsc::Receiver<Vec<u8>>>,
    Option<std::thread::JoinHandle<()>>,
) {
    match output_read {
        Some(output_read) => {
            let (output_tx, output_rx) = mpsc::channel(256);
            (
                Some(output_rx),
                Some(spawn_output_reader(output_read, output_tx)),
            )
        }
        None => (None, None),
    }
}

fn spawn_output_reader(
    output_read: HANDLE,
    output_tx: mpsc::Sender<Vec<u8>>,
) -> std::thread::JoinHandle<()> {
    read_handle_loop(output_read, move |chunk| {
        let _ = output_tx.blocking_send(chunk.to_vec());
    })
}

fn spawn_input_writer(
    input_write: HANDLE,
    mut writer_rx: mpsc::Receiver<EmbeddingStdinRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut input_write = Some(input_write);
        while let Some(request) = writer_rx.blocking_recv() {
            match request {
                EmbeddingStdinRequest::Write { bytes, completed } => {
                    let result = match input_write {
                        Some(handle) => write_all_handle(handle, &bytes),
                        None => Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "process stdin closed",
                        )),
                    };
                    let failed = result.is_err();
                    let _ = completed.send(result);
                    if failed {
                        break;
                    }
                }
                EmbeddingStdinRequest::Close { completed } => {
                    let result = close_input_handle(input_write.take());
                    let _ = completed.send(result);
                    return;
                }
            }
        }
        let _ = close_input_handle(input_write.take());
    })
}

fn write_all_handle(handle: HANDLE, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let mut written = 0;
        let result = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr() as *const _,
                bytes.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "WriteFile returned success but wrote 0 bytes",
            ));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn close_input_handle(handle: Option<HANDLE>) -> io::Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };
    if unsafe { CloseHandle(handle) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn terminate_job_or_process(
    job: &JobObject,
    process_handle: &Arc<Mutex<Option<HANDLE>>>,
) -> io::Result<()> {
    let Err(job_error) = job.terminate() else {
        return Ok(());
    };
    let process_error = match process_handle.lock() {
        Ok(handle) => match handle.as_ref() {
            Some(handle) if unsafe { TerminateProcess(*handle, 1) } == 0 => {
                Some(io::Error::last_os_error())
            }
            Some(_) | None => None,
        },
        Err(_) => Some(io::Error::other("process handle lock poisoned")),
    };
    let message = match process_error {
        Some(process_error) => format!(
            "failed to terminate process job: {job_error}; failed to terminate root process: {process_error}"
        ),
        None => format!(
            "failed to terminate process job: {job_error}; the root process was terminated but descendants may remain"
        ),
    };
    Err(io::Error::other(message))
}
