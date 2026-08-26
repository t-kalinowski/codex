use codex_sandbox_api::BackendPreference;
use codex_sandbox_api::CommandSpec;
use codex_sandbox_api::FileSystemBase;
use codex_sandbox_api::FileSystemPolicy;
#[cfg(target_os = "linux")]
use codex_sandbox_api::LinuxHelper;
use codex_sandbox_api::MissingPathBehavior;
use codex_sandbox_api::NetworkPolicy;
use codex_sandbox_api::PathAccess;
use codex_sandbox_api::PathRule;
use codex_sandbox_api::SANDBOX_API_VERSION;
use codex_sandbox_api::SandboxBackend;
use codex_sandbox_api::SandboxCapabilities;
use codex_sandbox_api::SandboxError;
use codex_sandbox_api::SandboxExitStatus;
use codex_sandbox_api::SandboxFeature;
use codex_sandbox_api::SandboxLifetime;
use codex_sandbox_api::SandboxPolicy;
use codex_sandbox_api::SandboxRequest;
use codex_sandbox_api::SandboxRuntime;
use codex_sandbox_api::SandboxRuntimeConfig;
use codex_sandbox_api::SandboxStdio;
use codex_sandbox_api::SandboxStdioMode;
use codex_sandbox_api::SandboxedChild;
use codex_sandbox_api::SandboxedOutput;
use codex_sandbox_api::SandboxedProcess;
use codex_sandbox_api::SandboxedStdin;
use codex_sandbox_api::TerminalPolicy;
#[cfg(target_os = "windows")]
use codex_sandbox_api::WindowsOptions;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

#[test]
fn constructs_the_documented_public_request() {
    let state_dir = PathBuf::from("/application/state");
    let runtime_root = PathBuf::from("/runtime");
    let cache_root = PathBuf::from("/cache");
    let home = PathBuf::from("/home/example");
    let cwd = PathBuf::from("/workspace");
    let mut env = BTreeMap::new();
    env.insert(OsString::from("ONLY"), OsString::from("this"));

    let command = CommandSpec::new("worker", cwd.clone(), env.clone())
        .arg("--stdio")
        .args(["-v"]);
    let policy = SandboxPolicy::platform_minimal()
        .read_only(runtime_root.clone())
        .read_write(cache_root.clone())
        .deny(home.clone())
        .network_denied()
        .terminal_inherited_or_created();
    let request = SandboxRequest::new(command, policy)
        .stdin(SandboxStdioMode::Pipe)
        .stdout(SandboxStdioMode::Inherit)
        .stderr(SandboxStdioMode::Null)
        .lifetime(SandboxLifetime::SupervisedProcessTree);
    let config = SandboxRuntimeConfig::new(state_dir.clone());

    assert_eq!(config.state_dir, state_dir);
    assert_eq!(config.backend, BackendPreference::PlatformDefault);
    assert_eq!(request.command.program, OsString::from("worker"));
    assert_eq!(request.command.args, ["--stdio", "-v"]);
    assert_eq!(request.command.cwd, cwd);
    assert_eq!(request.command.env, env);
    assert_eq!(
        request.stdio,
        SandboxStdio {
            stdin: SandboxStdioMode::Pipe,
            stdout: SandboxStdioMode::Inherit,
            stderr: SandboxStdioMode::Null,
        }
    );
    assert_eq!(request.lifetime, SandboxLifetime::SupervisedProcessTree);
    assert_eq!(
        request.policy.filesystem.base,
        FileSystemBase::PlatformMinimal
    );
    assert_eq!(
        request.policy.filesystem,
        FileSystemPolicy {
            base: FileSystemBase::PlatformMinimal,
            rules: vec![
                PathRule {
                    path: runtime_root,
                    access: PathAccess::Read,
                    missing: MissingPathBehavior::Error,
                },
                PathRule {
                    path: cache_root,
                    access: PathAccess::Write,
                    missing: MissingPathBehavior::Error,
                },
                PathRule {
                    path: home,
                    access: PathAccess::Deny,
                    missing: MissingPathBehavior::Error,
                },
            ],
        }
    );
    assert_eq!(request.policy.network, NetworkPolicy::Denied);
    assert_eq!(
        request.policy.terminal,
        TerminalPolicy::InheritedAndCreatedOnly
    );
}

#[test]
fn request_defaults_preserve_version_one_streams_without_requesting_supervision() {
    let request = SandboxRequest::new(
        CommandSpec::new("worker", "/workspace", BTreeMap::new()),
        SandboxPolicy::host_read_only(),
    );

    assert_eq!(
        request.stdio,
        SandboxStdio {
            stdin: SandboxStdioMode::Null,
            stdout: SandboxStdioMode::Pipe,
            stderr: SandboxStdioMode::Pipe,
        }
    );
    assert_eq!(request.stdio, SandboxStdio::default());
    assert_eq!(request.lifetime, SandboxLifetime::BackendDefault);
    assert_eq!(request.policy.terminal, TerminalPolicy::BackendDefault);
}

#[test]
fn constructs_host_read_only_policy_with_missing_path_ignore() {
    let optional_root = PathBuf::from("/optional/root");
    let policy = SandboxPolicy::host_read_only()
        .rule(PathRule::new(optional_root.clone(), PathAccess::Read).ignore_if_missing())
        .network_unrestricted();

    assert_eq!(policy.filesystem.base, FileSystemBase::HostReadOnly);
    assert_eq!(
        policy.filesystem.rules,
        [PathRule {
            path: optional_root,
            access: PathAccess::Read,
            missing: MissingPathBehavior::Ignore,
        }]
    );
    assert_eq!(policy.network, NetworkPolicy::Unrestricted);
}

#[test]
fn preserves_nested_rule_order() {
    let parent = PathBuf::from("/workspace");
    let child = parent.join("private");
    let policy = SandboxPolicy::host_read_only()
        .read_write(parent.clone())
        .deny(child.clone());

    assert_eq!(
        policy.filesystem.rules,
        [
            PathRule::new(parent, PathAccess::Write),
            PathRule::new(child, PathAccess::Deny),
        ]
    );
}

#[test]
fn exposes_the_versioned_runtime_and_process_contract() {
    let _: u32 = SANDBOX_API_VERSION;
    assert_eq!(SANDBOX_API_VERSION, 2);
    assert_send_sync::<SandboxRuntime>();
    assert_send::<SandboxedChild>();
    assert_send_sync::<SandboxedProcess>();
    assert_send::<SandboxedStdin>();
    assert_send::<SandboxedOutput>();
    let _ = [
        SandboxBackend::MacosSeatbelt,
        SandboxBackend::LinuxBubblewrap,
        SandboxBackend::LinuxLandlock,
        SandboxBackend::WindowsRestrictedToken,
        SandboxBackend::WindowsElevated,
    ];
    let _ = [
        SandboxFeature::MinimalReadPolicy,
        SandboxFeature::DeniedReadPaths,
        SandboxFeature::DeniedWritePaths,
        SandboxFeature::NestedAllowUnderDeny,
        SandboxFeature::NetworkDenial,
        SandboxFeature::NetworkUnrestricted,
        SandboxFeature::Interrupt,
        SandboxFeature::ProcessTreeTermination,
        SandboxFeature::CurrentProcessGroupTermination,
        SandboxFeature::TerminalIsolation,
    ];
    let _: fn(SandboxExitStatus) -> Option<i32> = SandboxExitStatus::code;
    let _: fn(SandboxExitStatus) -> Option<i32> = SandboxExitStatus::signal;
    let _: fn(SandboxExitStatus) -> bool = SandboxExitStatus::success;

    let config = SandboxRuntimeConfig::new(PathBuf::from("/application/state"));
    #[cfg(target_os = "linux")]
    let config = {
        let mut config = config;
        config.linux.helper = LinuxHelper::External(PathBuf::from("/packaged/helper"));
        let _ = LinuxHelper::CurrentExecutable;
        let _: fn() = codex_sandbox_api::dispatch_embedded_helper;
        config
    };
    #[cfg(target_os = "windows")]
    let config = {
        let mut config = config;
        config.windows = WindowsOptions::default();
        config
    };
    let _: SandboxRuntimeConfig = config;
    let _: fn(SandboxRuntimeConfig) -> Result<SandboxRuntime, SandboxError> = SandboxRuntime::new;

    let _ = child_contract as fn(&mut SandboxedChild);
    let _ = process_contract as fn(&SandboxedProcess);
    let _ = runtime_contract as fn(&SandboxRuntime, SandboxRequest);
    let _ = capabilities_contract as fn(SandboxCapabilities);
    let _ = error_contract as fn(SandboxError);
    let _ = stdin_contract;
    let _ = output_contract;
    let _: fn() -> Result<(), SandboxError> =
        codex_sandbox_api::terminate_current_process_group_members;
}

fn assert_send<T: Send>() {}

fn assert_send_sync<T: Send + Sync>() {}

fn child_contract(child: &mut SandboxedChild) {
    let _: Option<SandboxedStdin> = child.take_stdin();
    let _: Option<SandboxedOutput> = child.take_stdout();
    let _: Option<SandboxedOutput> = child.take_stderr();
    let _: SandboxedProcess = child.process();
}

fn process_contract(process: &SandboxedProcess) {
    drop(process.wait_root());
    let _ = process.try_root_status();
    let _ = process.interrupt();
    let _ = process.terminate();
    drop(process.retire());
    let _ = process.backend();
    let _ = process.lifetime();
}

fn runtime_contract(runtime: &SandboxRuntime, request: SandboxRequest) {
    let _: SandboxCapabilities = runtime.capabilities();
    drop(runtime.spawn(request));
}

fn capabilities_contract(capabilities: SandboxCapabilities) {
    let SandboxCapabilities {
        backend,
        minimal_read_policy,
        denied_read_paths,
        denied_write_paths,
        network_denial,
        network_unrestricted,
        interrupt,
        process_tree_termination,
        terminal_isolation,
    } = capabilities;
    let _ = (
        backend,
        minimal_read_policy,
        denied_read_paths,
        denied_write_paths,
        network_denial,
        network_unrestricted,
        interrupt,
        process_tree_termination,
        terminal_isolation,
    );
}

fn error_contract(error: SandboxError) {
    match error {
        SandboxError::UnsupportedPlatform { platform } => drop(platform),
        SandboxError::BackendUnavailable { backend, message } => drop((backend, message)),
        SandboxError::UnsupportedPolicy {
            backend,
            feature,
            message,
        } => drop((backend, feature, message)),
        SandboxError::InvalidCommand { message } => drop(message),
        SandboxError::InvalidOperation { message } => drop(message),
        SandboxError::InvalidPath { path, message } => drop((path, message)),
        SandboxError::Preparation {
            backend,
            message,
            source,
        }
        | SandboxError::Spawn {
            backend,
            message,
            source,
        } => drop((backend, message, source)),
        SandboxError::Io { operation, source } => drop((operation, source)),
        _ => {}
    }
}

async fn stdin_contract(mut stdin: SandboxedStdin) -> Result<(), SandboxError> {
    stdin.write_all(b"raw\0bytes").await?;
    stdin.close().await
}

async fn output_contract(mut output: SandboxedOutput) -> Result<(), SandboxError> {
    let _ = output.read_chunk().await?;
    Ok(())
}
