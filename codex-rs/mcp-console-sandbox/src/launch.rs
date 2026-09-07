use crate::bootstrap::Bootstrap;
use crate::codex::FileSystemSandboxPolicy;
use crate::codex::NetworkProxy;
use crate::codex::NetworkProxyState;
use crate::codex::PathUri;
use crate::codex::PermissionProfile;
use crate::codex::RemoteNetworkProxyLaunchConfig;
use crate::codex::SandboxCommand;
use crate::codex::SandboxManager;
use crate::codex::SandboxTransformRequest;
use crate::codex::SandboxType;
use crate::codex::SandboxablePreference;
use crate::codex::WindowsSandboxLevel;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use std::fs::File;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

pub async fn run(request: Bootstrap, stdin: File) -> Result<ExitStatus> {
    let filesystem = FileSystemSandboxPolicy::try_from(request.filesystem)
        .map_err(anyhow::Error::msg)
        .context("invalid filesystem policy")?;
    let permissions = PermissionProfile::from_runtime_permissions(&filesystem, request.network);
    let proxy = if let Some(config) = request.proxy {
        let state = NetworkProxyState::from_remote_launch_config(
            RemoteNetworkProxyLaunchConfig::new(config),
        )?;
        Some(
            NetworkProxy::builder()
                .state(Arc::new(state))
                .build()
                .await?,
        )
    } else {
        None
    };
    let (environment, managed_network) = if let Some(proxy) = &proxy {
        let prepared = proxy
            .prepare_for_optional_environment(request.environment, /*environment_id*/ None)?;
        (prepared.env, Some(prepared.sandbox_context))
    } else {
        (request.environment, None)
    };
    let manager = SandboxManager::default();
    let sandbox = manager.select_initial(
        &permissions,
        SandboxablePreference::Require,
        WindowsSandboxLevel::Disabled,
        /*has_managed_network_requirements*/ proxy.is_some(),
    );
    ensure!(
        sandbox != SandboxType::None,
        "native sandbox is unavailable"
    );
    let cwd = PathUri::from(request.cwd.clone());
    let executable = std::env::current_exe()?;
    let mut args = request.command.into_iter();
    let program = args.next().context("command must contain a program")?;
    let prepared = manager.transform(SandboxTransformRequest {
        command: SandboxCommand {
            program: program.into(),
            args: args.collect(),
            cwd: cwd.clone(),
            env: environment,
            managed_network,
            additional_permissions: None,
        },
        permissions: &permissions,
        sandbox,
        enforce_managed_network: proxy.is_some(),
        environment_id: None,
        network: proxy.as_ref(),
        sandbox_policy_cwd: &cwd,
        codex_linux_sandbox_exe: Some(&executable),
        use_legacy_landlock: false,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
    })?;
    let handle = if let Some(proxy) = &proxy {
        Some(proxy.run().await?)
    } else {
        None
    };
    let result = async {
        let (program, args) = prepared
            .command
            .split_first()
            .context("empty native sandbox command")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(request.cwd.as_path())
            .env_clear()
            .envs(prepared.env);
        // Keep the real executable path in argv[0]. Native bwrap's no-argv0
        // compatibility path re-execs that path; main dispatches helper args.
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("launch native sandbox")?
            .wait()
            .await
            .context("wait for native sandbox")
    }
    .await;
    if let Some(handle) = handle {
        handle.shutdown().await?;
    }
    result
}
