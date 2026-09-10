// Pinned upstream integration. Lifecycle modules use only these owned results.
#[cfg(test)]
pub use codex_utils_cargo_bin::cargo_bin;
#[cfg(not(test))]
pub use upstream::*;

#[cfg(not(test))]
mod upstream {
    use crate::bootstrap::Bootstrap;
    use crate::native::TargetSetup;
    use crate::signals::SignalState;
    use crate::storage::Storage;
    use anyhow::Context;
    use anyhow::Result;
    use anyhow::ensure;
    use codex_network_proxy::NetworkProxy;
    use codex_network_proxy::NetworkProxyHandle;
    use codex_network_proxy::NetworkProxyState;
    pub use codex_network_proxy::RemoteNetworkProxyConfig;
    use codex_network_proxy::RemoteNetworkProxyLaunchConfig;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSandboxPolicy;
    pub use codex_protocol::permissions::NetworkSandboxPolicy;
    pub use codex_protocol::permissions::RawFileSystemSandboxPolicy;
    pub use codex_utils_absolute_path::AbsolutePathBuf;
    use std::os::fd::RawFd;
    use std::process::Command;
    use std::sync::Arc;

    pub struct Prepared {
        pub command: Option<Command>,
        pub setup: TargetSetup,
        proxy: Option<NetworkProxyHandle>,
    }

    impl Prepared {
        pub async fn shutdown(self) -> Result<()> {
            if let Some(handle) = self.proxy {
                handle.shutdown().await?;
            }
            Ok(())
        }
    }

    pub async fn prepare(
        request: Bootstrap,
        signals: SignalState,
        storage: Option<&Storage>,
        setup_fd: RawFd,
    ) -> Result<Prepared> {
        ensure!(
            cfg!(target_os = "macos") || request.macos_seatbelt_profile_extension.is_none(),
            "macos_seatbelt_profile_extension is supported only on macOS"
        );
        let mut filesystem = FileSystemSandboxPolicy::try_from(request.filesystem)
            .map_err(anyhow::Error::msg)
            .context("invalid filesystem policy")?;
        let mut environment = request.environment;
        if let Some(storage) = storage {
            #[cfg(target_os = "macos")]
            let access = FileSystemAccessMode::Read;
            #[cfg(target_os = "linux")]
            let access = FileSystemAccessMode::Write;
            // On Linux bind the containing directory, so disposable `data`
            // is not itself a mountpoint and can be replaced by the workload.
            filesystem.entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::from(AbsolutePathBuf::try_from(storage.root.clone())?),
                access,
            ));
            for name in &request
                .lifecycle
                .private_tmp
                .as_ref()
                .context("private storage configuration")?
                .environment
            {
                environment.insert(
                    name.clone(),
                    storage
                        .data
                        .to_str()
                        .context("private storage path must be UTF-8")?
                        .to_owned(),
                );
            }
        }
        #[cfg(target_os = "linux")]
        ensure!(
            request.linux_backend == Some(crate::config::LinuxBackend::Landlock)
                || !filesystem.has_full_disk_write_access(),
            "supervised Linux execution requires a restricted filesystem policy"
        );
        #[cfg(target_os = "linux")]
        if request.linux_backend == Some(crate::config::LinuxBackend::Landlock)
            && !filesystem.has_full_disk_write_access()
        {
            // Native best-effort Landlock on ABI 1/2 does not restrict truncate.
            // Do not present those capabilities as a read-only filesystem.
            let abi = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<u8>(),
                    0,
                    1,
                )
            };
            ensure!(
                abi >= 3,
                "Landlock filesystem policy requires truncate enforcement (ABI 3 or later)"
            );
        }
        let proxy = if let Some(config) = request.proxy {
            Some(
                NetworkProxy::builder()
                    .state(Arc::new(NetworkProxyState::from_remote_launch_config(
                        RemoteNetworkProxyLaunchConfig::new(config),
                    )?))
                    .build()
                    .await?,
            )
        } else {
            None
        };
        let (managed_network, mut proxy_environment) = if let Some(proxy) = &proxy {
            let prepared = proxy.prepare_for_optional_environment(
                Default::default(),
                /*environment_id*/ None,
            )?;
            (Some(prepared.sandbox_context), prepared.env)
        } else {
            (None, Default::default())
        };
        // Native proxy overrides take precedence over ordinary target inputs.
        environment.extend(proxy_environment.clone());
        for name in &request.excluded_environment {
            environment.remove(name);
            proxy_environment.remove(name);
        }
        let setup = TargetSetup {
            signals,
            environment,
            proxy_environment: proxy_environment.keys().cloned().collect(),
            excluded_environment: request.excluded_environment,
            command: request.command,
            seatbelt: None,
        };

        #[cfg(target_os = "macos")]
        let (mut command, setup) = {
            let mut setup = setup;
            use codex_sandboxing::seatbelt::CreateSeatbeltCommandArgsParams;
            use codex_sandboxing::seatbelt::create_seatbelt_profile;
            let mut profile = create_seatbelt_profile(CreateSeatbeltCommandArgsParams {
                command: Vec::new(),
                file_system_sandbox_policy: &filesystem,
                network_sandbox_policy: request.network,
                sandbox_policy_cwd: request.cwd.as_path(),
                enforce_managed_network: proxy.is_some(),
                managed_network: managed_network.as_ref(),
                environment_id: None,
                network: proxy.as_ref(),
                extra_allow_unix_sockets: &[],
            })
            .map_err(anyhow::Error::msg)?;
            if let Some(extension) = request.macos_seatbelt_profile_extension {
                profile.policy.push('\n');
                profile.policy.push_str(&extension);
            }
            // KERN_PROCARGS2 can expose the host's launch environment despite
            // deny-default and the ordinary same-sandbox process-info allowance.
            // Explicitly deny outside-sandbox reads while retaining peer queries.
            profile
                .policy
                .push_str("\n(deny process-info-pidinfo (require-not (target same-sandbox)))\n");
            if let Some(storage) = storage {
                // Disposable data may replace its own root; unlike an upstream
                // writable authority, this path is never reused for another job.
                profile
                    .parameters
                    .push(("RUNNER_PRIVATE_DATA".to_owned(), storage.data.clone()));
                profile
                    .parameters
                    .push(("RUNNER_PRIVATE_ROOT".to_owned(), storage.root.clone()));
                profile.policy.push_str("\n(allow file-write* (subpath (param \"RUNNER_PRIVATE_DATA\")))\n(deny file-write* (literal (param \"RUNNER_PRIVATE_ROOT\")))\n");
            }
            setup.seatbelt = Some(crate::native::Seatbelt {
                policy: profile.policy,
                parameters: profile
                    .parameters
                    .into_iter()
                    .map(|(key, path)| {
                        Ok((
                            key,
                            path.into_os_string()
                                .into_string()
                                .map_err(|_| anyhow::anyhow!("Seatbelt parameter must be UTF-8"))?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            });
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args(["--native-macos", &setup_fd.to_string()])
                .env_clear();
            (command, setup)
        };
        #[cfg(target_os = "linux")]
        let mut command = {
            use codex_protocol::config_types::WindowsSandboxLevel;
            use codex_protocol::models::PermissionProfile;
            use codex_sandboxing::SandboxCommand;
            use codex_sandboxing::SandboxManager;
            use codex_sandboxing::SandboxTransformRequest;
            use codex_sandboxing::SandboxType;
            use codex_utils_path_uri::PathUri;
            let permissions =
                PermissionProfile::from_runtime_permissions(&filesystem, request.network);
            let cwd = PathUri::from(request.cwd.clone());
            let executable = std::env::current_exe()?;
            // Target PATH, TMPDIR, loader, proxy and control-looking variables
            // must not select or configure host helpers. Only the trusted launch
            // environment and the prepared proxy context reach native setup.
            let mut helper_env = std::collections::HashMap::new();
            for (key, value) in std::env::vars_os() {
                let key = key
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("host environment key must be UTF-8"))?;
                if key.starts_with("LD_")
                    || key.starts_with("DYLD_")
                    || setup.excluded_environment.contains(&key)
                {
                    continue;
                }
                helper_env.insert(
                    key,
                    value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("host environment value must be UTF-8"))?,
                );
            }
            helper_env.extend(proxy_environment);
            let native = SandboxManager::default().transform(SandboxTransformRequest {
                command: SandboxCommand {
                    program: setup.command[0].clone().into(),
                    args: setup.command[1..].to_vec(),
                    cwd: cwd.clone(),
                    env: helper_env,
                    managed_network,
                    additional_permissions: None,
                },
                permissions: &permissions,
                sandbox: SandboxType::LinuxSeccomp,
                enforce_managed_network: proxy.is_some(),
                environment_id: None,
                network: proxy.as_ref(),
                sandbox_policy_cwd: &cwd,
                codex_linux_sandbox_exe: Some(&executable),
                use_legacy_landlock: request.linux_backend
                    == Some(crate::config::LinuxBackend::Landlock),
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_private_desktop: false,
            })?;
            let mut command = Command::new(&native.command[0]);
            command
                .args(["--target-setup-fd", &setup_fd.to_string()])
                .args(&native.command[1..])
                .env_clear()
                .envs(native.env);
            command
        };
        command.current_dir(request.cwd.as_path());
        for name in &setup.excluded_environment {
            command.env_remove(name);
        }
        let handle = if let Some(proxy) = proxy {
            Some(proxy.run().await?)
        } else {
            None
        };
        Ok(Prepared {
            command: Some(command),
            setup,
            proxy: handle,
        })
    }

    #[cfg(target_os = "linux")]
    pub fn linux_sandbox_main() -> ! {
        codex_linux_sandbox::run_main_with_target_setup(crate::native::linux_target_setup)
    }
}
