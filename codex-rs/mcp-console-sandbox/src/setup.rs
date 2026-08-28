use crate::protocol::SetupCompletedOperation;
use crate::protocol::SetupOperation;
use crate::protocol::SetupRequest;
use crate::protocol::SetupStatus;
use anyhow::Result;
use std::path::Path;

#[derive(Default)]
pub struct SetupSession {
    #[cfg(windows)]
    prepared: Option<PreparedWindowsSetup>,
}

impl SetupSession {
    pub async fn inspect(
        &mut self,
        request: SetupRequest,
        state_directory: &Path,
    ) -> Result<SetupStatus> {
        #[cfg(not(windows))]
        {
            let _ = request;
            Ok(crate::capabilities::setup_status(state_directory))
        }
        #[cfg(windows)]
        {
            self.prepare_matching(request, state_directory).await?;
            self.prepared
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("matching Windows setup was not retained"))?
                .inspect()
                .await
        }
    }

    pub async fn apply(
        &mut self,
        operation: SetupOperation,
        request: SetupRequest,
        state_directory: &Path,
    ) -> Result<SetupCompletedOperation> {
        #[cfg(not(windows))]
        {
            let _ = (operation, request, state_directory);
            anyhow::bail!("this platform does not require or support an explicit setup operation")
        }
        #[cfg(windows)]
        {
            self.prepare_matching(request, state_directory).await?;
            let prepared = self
                .prepared
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("matching Windows setup was not retained"))?;
            prepared.apply(operation).await
        }
    }

    #[cfg(windows)]
    pub async fn take_for_launch(
        &mut self,
        request: SetupRequest,
        policy: &crate::policy::ValidatedPolicy,
        state_directory: &Path,
    ) -> Result<PreparedWindowsSetup> {
        self.prepare_matching(request, state_directory).await?;
        let prepared = self
            .prepared
            .take()
            .ok_or_else(|| anyhow::anyhow!("matching Windows setup was not retained"))?;
        // Matching JSON can compile to different authority after an ignored
        // missing path appears, so only the proxy and environment are reusable.
        prepared.rebuild_for_launch(policy, state_directory).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        #[cfg(windows)]
        if let Some(prepared) = self.prepared.take() {
            prepared.shutdown().await?;
        }
        Ok(())
    }

    #[cfg(windows)]
    async fn prepare_matching(
        &mut self,
        request: SetupRequest,
        state_directory: &Path,
    ) -> Result<()> {
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.request == request)
        {
            return Ok(());
        }
        self.shutdown().await?;
        self.prepared = Some(PreparedWindowsSetup::prepare(request, state_directory).await?);
        Ok(())
    }
}

#[cfg(windows)]
pub struct PreparedWindowsSetup {
    pub request: SetupRequest,
    pub native: codex_windows_sandbox::WindowsSandboxStandaloneSetupRequest,
    pub environment: crate::environment::NativeEnvironment,
    pub network: crate::network::PreparedNetwork,
}

#[cfg(windows)]
impl PreparedWindowsSetup {
    pub async fn prepare(request: SetupRequest, state_directory: &Path) -> Result<Self> {
        let policy = crate::policy::validate_setup(&request, state_directory)?;
        let mut environment = crate::environment::target_environment()?;
        let mut network =
            crate::network::prepare_network(&request.network, &mut environment).await?;
        let native = match crate::platform::prepare_windows_setup_request(
            &policy,
            &network,
            &environment,
            state_directory,
        ) {
            Ok(native) => native,
            Err(error) => {
                if let Some(handle) = network.handle.take() {
                    let _ = handle.shutdown().await;
                }
                return Err(error);
            }
        };
        Ok(Self {
            request,
            native,
            environment,
            network,
        })
    }

    async fn rebuild_for_launch(
        mut self,
        policy: &crate::policy::ValidatedPolicy,
        state_directory: &Path,
    ) -> Result<Self> {
        match crate::platform::prepare_windows_setup_request(
            policy,
            &self.network,
            &self.environment,
            state_directory,
        ) {
            Ok(native) => {
                match codex_windows_sandbox::windows_sandbox_standalone_setup_status(&native) {
                    codex_windows_sandbox::WindowsSandboxStandaloneSetupState::Ready
                    | codex_windows_sandbox::WindowsSandboxStandaloneSetupState::RefreshRequired {
                        ..
                    } => {}
                    state => {
                        let source = anyhow::anyhow!(
                            "standalone Windows setup is not ready for launch: {state:?}"
                        );
                        if let Err(cleanup_error) = self.shutdown().await {
                            return Err(source.context(format!(
                                "managed network cleanup after Windows setup rejection failed: {cleanup_error:#}"
                            )));
                        }
                        return Err(source);
                    }
                }
                self.native = native;
                crate::environment::remove_windows_setup_only_environment(&mut self.environment);
                Ok(self)
            }
            Err(source) => {
                if let Err(cleanup_error) = self.shutdown().await {
                    return Err(source.context(format!(
                        "managed network cleanup after Windows policy rebuild failed: {cleanup_error:#}"
                    )));
                }
                Err(source)
            }
        }
    }

    pub async fn inspect(&self) -> Result<SetupStatus> {
        use crate::protocol::SetupState;
        use codex_windows_sandbox::WindowsSandboxStandaloneSetupState;

        let native = self.native.clone();
        let native_status = tokio::task::spawn_blocking(move || {
            codex_windows_sandbox::windows_sandbox_standalone_verified_setup_status(&native)
        })
        .await?;
        Ok(match native_status {
            WindowsSandboxStandaloneSetupState::Ready => SetupStatus {
                state: SetupState::Ready,
                detail: None,
            },
            WindowsSandboxStandaloneSetupState::RefreshRequired { reason } => SetupStatus {
                state: SetupState::RefreshRequired,
                detail: Some(reason),
            },
            WindowsSandboxStandaloneSetupState::AdministrativeActionRequired { reason } => {
                SetupStatus {
                    state: SetupState::AdministrativeActionRequired,
                    detail: Some(reason),
                }
            }
            WindowsSandboxStandaloneSetupState::Unavailable { reason } => SetupStatus {
                state: SetupState::Unavailable,
                detail: Some(reason),
            },
        })
    }

    pub async fn apply(&self, operation: SetupOperation) -> Result<SetupCompletedOperation> {
        use codex_windows_sandbox::WindowsSandboxStandaloneSetupOperation;

        let native = self.native.clone();
        let result = tokio::task::spawn_blocking(move || match operation {
            SetupOperation::Prepare => {
                codex_windows_sandbox::prepare_windows_sandbox_standalone(&native)
            }
            SetupOperation::Refresh => {
                codex_windows_sandbox::refresh_windows_sandbox_standalone(&native)
            }
        })
        .await?;
        match result? {
            WindowsSandboxStandaloneSetupOperation::AlreadyReady => {
                Ok(SetupCompletedOperation::AlreadyReady)
            }
            WindowsSandboxStandaloneSetupOperation::Prepared => {
                Ok(SetupCompletedOperation::Prepared)
            }
            WindowsSandboxStandaloneSetupOperation::Refreshed => {
                Ok(SetupCompletedOperation::Refreshed)
            }
        }
    }

    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(handle) = self.network.handle.take() {
            handle.shutdown().await?;
        }
        Ok(())
    }
}
