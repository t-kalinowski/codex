use crate::WindowsSandboxStatsigMetricsSettings;
use crate::WindowsSandboxPolicyNamespace;
use crate::install_wfp_filters_for_account_in_namespace;
use std::path::Path;

pub fn install_wfp_filters<F>(
    _state_dir: &Path,
    offline_username: &str,
    _otel: Option<&WindowsSandboxStatsigMetricsSettings>,
    log: F,
) where
    F: FnMut(&str),
{
    install_wfp_filters_in_namespace(
        _state_dir,
        offline_username,
        WindowsSandboxPolicyNamespace::Codex,
        _otel,
        log,
    );
}

#[doc(hidden)]
pub fn install_wfp_filters_in_namespace<F>(
    _state_dir: &Path,
    offline_username: &str,
    namespace: WindowsSandboxPolicyNamespace,
    _otel: Option<&WindowsSandboxStatsigMetricsSettings>,
    mut log: F,
) where
    F: FnMut(&str),
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        install_wfp_filters_for_account_in_namespace(offline_username, namespace)
    })) {
        Ok(Ok(installed_filter_count)) => log(&format!(
            "WFP setup succeeded for {offline_username} with {installed_filter_count} installed filters"
        )),
        Ok(Err(error)) => log(&format!(
            "WFP setup failed for {offline_username}: {error}; continuing elevated setup"
        )),
        Err(panic_payload) => {
            let error = match panic_payload.downcast::<String>() {
                Ok(message) => *message,
                Err(panic_payload) => match panic_payload.downcast::<&'static str>() {
                    Ok(message) => (*message).to_string(),
                    Err(_) => "unknown panic payload".to_string(),
                },
            };
            log(&format!(
                "WFP setup panicked for {offline_username}: {error}; continuing elevated setup"
            ));
        }
    }
}
