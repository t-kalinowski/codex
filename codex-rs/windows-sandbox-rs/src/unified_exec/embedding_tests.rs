use super::*;
use crate::unified_exec::WindowsSandboxEmbeddingProcess;
use crate::unified_exec::WindowsSandboxEmbeddingRequest;
use crate::unified_exec::spawn_windows_sandbox_session_for_embedding;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;

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
            stdin_open: false,
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
            stdin_open: false,
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
            mut stdout_rx,
            mut stderr_rx,
            exit_rx,
        } = spawned;
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
        stdout_task.await.expect("stdout task join");
        stderr_task.await.expect("stderr task join");
        assert_eq!(exit_code, 0);

        session
            .terminate()
            .expect("terminate embedding process job after root exit");
        wait_for_process_exit(&descendant_process, Duration::from_secs(10))
            .expect("embedding descendant survived termination after root exit");
        assert!(
            !survival_marker.exists(),
            "embedding descendant completed after termination"
        );
    });
}
