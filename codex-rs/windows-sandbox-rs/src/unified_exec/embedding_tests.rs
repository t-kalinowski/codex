use super::*;
use crate::unified_exec::WindowsSandboxEmbeddingProcess;
use crate::unified_exec::WindowsSandboxEmbeddingRequest;
use crate::unified_exec::WindowsSandboxEmbeddingStdio;
use crate::unified_exec::WindowsSandboxEmbeddingStdioMode;
use crate::unified_exec::spawn_windows_sandbox_session_for_embedding;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use windows_sys::Win32::Foundation::GetHandleInformation;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;

#[test]
fn spawn_failure_does_not_disclose_or_persist_the_command() {
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async {
        let cwd = AbsolutePathBuf::from_absolute_path(sandbox_cwd()).expect("absolute cwd");
        let state = sandbox_home("embedding-sanitized-spawn-failure");
        let state_dir = AbsolutePathBuf::from_absolute_path(state.path()).expect("absolute state");
        let permission_profile = PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::read_only(),
            NetworkSandboxPolicy::Enabled,
        );
        let secret_program = r"C:\codex-embedding-secret-program-never-exists.exe";
        let secret_argument = "embedding-secret-argument";

        let error = spawn_windows_sandbox_session_for_embedding(WindowsSandboxEmbeddingRequest {
            permission_profile: &permission_profile,
            state_dir: &state_dir,
            command: vec![secret_program.to_string(), secret_argument.to_string()],
            cwd: &cwd,
            env_map: HashMap::new(),
            additional_deny_write_paths: &[],
            stdio: WindowsSandboxEmbeddingStdio {
                stdin: WindowsSandboxEmbeddingStdioMode::Null,
                stdout: WindowsSandboxEmbeddingStdioMode::Pipe,
                stderr: WindowsSandboxEmbeddingStdioMode::Pipe,
            },
        })
        .await
        .expect_err("the nonexistent embedding command should fail to spawn");
        let error = format!("{error:#}");

        assert!(error.contains("CreateProcessAsUserW failed"), "{error}");
        assert!(!error.contains(secret_program), "{error}");
        assert!(!error.contains(secret_argument), "{error}");
        let persisted_paths = fs::read_dir(state.path())
            .expect("read embedding state directory")
            .map(|entry| entry.expect("read embedding state entry").path())
            .collect::<Vec<_>>();
        assert_eq!(persisted_paths, Vec::<PathBuf>::new());
    });
}

#[test]
fn session_retains_process_tree_supervision_after_root_exit() {
    let Some(pwsh) = pwsh_path() else {
        eprintln!("skipping embedding process-tree test: PowerShell 7 is not installed");
        return;
    };
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async move {
        let root = sandbox_home("embedding-process-tree");
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&state).expect("create state directory");
        let ready_marker = workspace.join("descendant-started");
        let survival_marker = workspace.join("descendant-survived");
        let descendant_command = format!(
            "Set-Content -LiteralPath '{}' -Value $PID; Start-Sleep -Seconds 30; Set-Content -LiteralPath '{}' -Value survived",
            powershell_literal(&ready_marker),
            powershell_literal(&survival_marker),
        );
        let parent_tail = format!(
            "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 25 }}",
            powershell_literal(&ready_marker),
        );
        let parent_command =
            start_powershell_child(&pwsh, &workspace, &descendant_command, &parent_tail);
        let workspace =
            AbsolutePathBuf::from_absolute_path(workspace).expect("absolute workspace");
        let state_dir = AbsolutePathBuf::from_absolute_path(state).expect("absolute state");
        let policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry::new(
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                FileSystemAccessMode::Read,
            ),
            FileSystemSandboxEntry::new(workspace.clone().into(), FileSystemAccessMode::Write),
        ]);
        let permission_profile =
            PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Enabled);
        let spawned = spawn_windows_sandbox_session_for_embedding(WindowsSandboxEmbeddingRequest {
            permission_profile: &permission_profile,
            state_dir: &state_dir,
            command: vec![
                pwsh.display().to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                parent_command,
            ],
            cwd: &workspace,
            env_map: std::env::vars().collect(),
            additional_deny_write_paths: &[],
            stdio: WindowsSandboxEmbeddingStdio {
                stdin: WindowsSandboxEmbeddingStdioMode::Null,
                stdout: WindowsSandboxEmbeddingStdioMode::Pipe,
                stderr: WindowsSandboxEmbeddingStdioMode::Pipe,
            },
        })
        .await
        .expect("spawn embedding process-tree session");
        assert!(
            wait_for_path(&ready_marker, Duration::from_secs(10)),
            "embedding descendant did not start"
        );
        let descendant_pid = fs::read_to_string(&ready_marker)
            .expect("read descendant pid")
            .trim()
            .parse()
            .expect("parse descendant pid");
        let descendant_process =
            open_process_for_wait(descendant_pid).expect("open embedding descendant");

        let WindowsSandboxEmbeddingProcess {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;
        let mut stdout_rx = stdout_rx.expect("stdout should be piped");
        let mut stderr_rx = stderr_rx.expect("stderr should be piped");
        let stdout_task = tokio::spawn(async move {
            while stdout_rx.recv().await.is_some() {}
        });
        let stderr_task = tokio::spawn(async move {
            while stderr_rx.recv().await.is_some() {}
        });
        let exit_code = timeout(Duration::from_secs(10), exit_rx)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for embedding root exit\n{}",
                    sandbox_log(state_dir.as_path())
                )
            })
            .expect("embedding root should report an exit code");
        assert_eq!(exit_code, 0);
        assert!(
            !stdout_task.is_finished(),
            "descendant-held stdout should remain open after root observation"
        );
        assert!(
            !stderr_task.is_finished(),
            "descendant-held stderr should remain open after root observation"
        );

        session
            .terminate()
            .expect("terminate embedding process job after root exit");
        stdout_task.await.expect("stdout task join");
        stderr_task.await.expect("stderr task join");
        wait_for_process_exit(&descendant_process, Duration::from_secs(10))
            .expect("embedding descendant survived termination after root exit");
        assert!(
            !survival_marker.exists(),
            "embedding descendant completed after termination"
        );
    });
}

#[test]
fn embedding_stdio_modes_are_independent() {
    let Some(pwsh) = pwsh_path() else {
        eprintln!("skipping embedding stdio test: PowerShell 7 is not installed");
        return;
    };
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async move {
        let root = sandbox_home("embedding-stdio-modes");
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&state).expect("create state directory");
        let workspace = AbsolutePathBuf::from_absolute_path(workspace).expect("absolute workspace");
        let state_dir = AbsolutePathBuf::from_absolute_path(state).expect("absolute state");
        let policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry::new(
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                FileSystemAccessMode::Read,
            ),
            FileSystemSandboxEntry::new(workspace.clone().into(), FileSystemAccessMode::Write),
        ]);
        let permission_profile =
            PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Enabled);
        let command = "\
            $input = [Console]::OpenStandardInput(); \
            $output = [Console]::OpenStandardOutput(); \
            $bytes = New-Object IO.MemoryStream; \
            $input.CopyTo($bytes); \
            $bytes = $bytes.ToArray(); \
            $output.Write($bytes, 0, $bytes.Length)";
        let spawned = spawn_windows_sandbox_session_for_embedding(WindowsSandboxEmbeddingRequest {
            permission_profile: &permission_profile,
            state_dir: &state_dir,
            command: vec![
                pwsh.display().to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
            cwd: &workspace,
            env_map: std::env::vars().collect(),
            additional_deny_write_paths: &[],
            stdio: WindowsSandboxEmbeddingStdio {
                stdin: WindowsSandboxEmbeddingStdioMode::Pipe,
                stdout: WindowsSandboxEmbeddingStdioMode::Pipe,
                stderr: WindowsSandboxEmbeddingStdioMode::Null,
            },
        })
        .await
        .expect("spawn embedding stdio session");
        let WindowsSandboxEmbeddingProcess {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;
        assert!(stderr_rx.is_none());
        let mut stdout_rx = stdout_rx.expect("stdout should be piped");
        session
            .write_all(b"raw\0io\n")
            .await
            .expect("write piped stdin");
        session.close_stdin().await.expect("close piped stdin");

        let mut stdout = Vec::new();
        while let Some(chunk) = stdout_rx.recv().await {
            stdout.extend(chunk);
        }
        let exit_code = timeout(Duration::from_secs(10), exit_rx)
            .await
            .expect("timed out waiting for embedding process")
            .expect("embedding process should report an exit code");

        assert_eq!(exit_code, 0);
        assert_eq!(stdout, b"raw\0io\n");
    });
}

#[test]
fn inherited_and_null_stdio_do_not_expose_pipe_transports() {
    let Some(pwsh) = pwsh_path() else {
        eprintln!("skipping embedding stdio test: PowerShell 7 is not installed");
        return;
    };
    let _guard = legacy_process_test_guard();
    current_thread_runtime().block_on(async move {
        let inherited_stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let mut original_stdout_flags = 0;
        let inspect_inherited_stdout = inherited_stdout != 0
            && inherited_stdout != INVALID_HANDLE_VALUE
            && unsafe { GetHandleInformation(inherited_stdout, &mut original_stdout_flags) } != 0;
        let root = sandbox_home("embedding-unpiped-stdio");
        let workspace =
            AbsolutePathBuf::from_absolute_path(root.path()).expect("absolute workspace");
        let state = root.path().join("state");
        fs::create_dir_all(&state).expect("create state directory");
        let state_dir = AbsolutePathBuf::from_absolute_path(state).expect("absolute state");
        let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry::new(
            FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            FileSystemAccessMode::Read,
        )]);
        let permission_profile =
            PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Enabled);
        let spawned = spawn_windows_sandbox_session_for_embedding(WindowsSandboxEmbeddingRequest {
            permission_profile: &permission_profile,
            state_dir: &state_dir,
            command: vec![
                pwsh.display().to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "exit 0".to_string(),
            ],
            cwd: &workspace,
            env_map: std::env::vars().collect(),
            additional_deny_write_paths: &[],
            stdio: WindowsSandboxEmbeddingStdio {
                stdin: WindowsSandboxEmbeddingStdioMode::Null,
                stdout: WindowsSandboxEmbeddingStdioMode::Inherit,
                stderr: WindowsSandboxEmbeddingStdioMode::Null,
            },
        })
        .await
        .expect("spawn embedding unpiped stdio session");
        let WindowsSandboxEmbeddingProcess {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;

        assert!(stdout_rx.is_none());
        assert!(stderr_rx.is_none());
        let error = session
            .write_all(b"unavailable")
            .await
            .expect_err("non-piped stdin should reject writes");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        let exit_code = timeout(Duration::from_secs(10), exit_rx)
            .await
            .expect("timed out waiting for embedding process")
            .expect("embedding process should report an exit code");
        assert_eq!(exit_code, 0);
        if inspect_inherited_stdout {
            let mut final_stdout_flags = 0;
            assert_ne!(
                unsafe { GetHandleInformation(inherited_stdout, &mut final_stdout_flags) },
                0,
                "embedding launch closed the parent's inherited stdout handle"
            );
            assert_eq!(final_stdout_flags, original_stdout_flags);
        }
    });
}
