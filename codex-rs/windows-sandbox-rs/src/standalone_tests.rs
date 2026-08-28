use crate::WindowsSandboxStandaloneFilesystemOverrides;
use crate::WindowsSandboxStandaloneNetworkIdentity;
use crate::WindowsSandboxStandaloneNetworkSetup;
use crate::WindowsSandboxStandalonePolicyRequest;
use crate::WindowsSandboxStandaloneResources;
use crate::WindowsSandboxStandaloneSetupRequest;
use crate::WindowsSandboxStandaloneSetupState;
use crate::is_windows_sandbox_standalone_helper_invocation;
use crate::windows_sandbox_standalone_setup_request_from_permission_profile;
use crate::windows_sandbox_standalone_setup_status;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn write_resources(root: &TempDir) -> Result<WindowsSandboxStandaloneResources> {
    let setup_executable = root.path().join("codex-windows-sandbox-setup.exe");
    let command_runner_executable = root.path().join("codex-command-runner.exe");
    fs::write(&setup_executable, b"setup")?;
    fs::write(&command_runner_executable, b"runner")?;
    Ok(WindowsSandboxStandaloneResources {
        setup_executable,
        command_runner_executable,
    })
}

#[test]
fn standalone_helper_dispatch_requires_the_exact_private_shape() {
    let valid = vec![
        OsString::from("--standalone-sandbox-runner"),
        OsString::from(r"--pipe-in=\\.\pipe\private-in"),
        OsString::from(r"--pipe-out=\\.\pipe\private-out"),
    ];
    let mut extra = valid.clone();
    extra.push(OsString::from("target-argument"));
    let ordinary = vec![
        OsString::from("normal-command"),
        OsString::from("--standalone-sandbox-runner"),
    ];

    assert_eq!(
        [
            is_windows_sandbox_standalone_helper_invocation(&valid),
            is_windows_sandbox_standalone_helper_invocation(&extra),
            is_windows_sandbox_standalone_helper_invocation(&ordinary),
        ],
        [true, false, false]
    );
}

fn setup_request(
    root: &TempDir,
    resources: WindowsSandboxStandaloneResources,
) -> WindowsSandboxStandaloneSetupRequest {
    WindowsSandboxStandaloneSetupRequest {
        state_dir: root.path().join("state"),
        resources,
        command_cwd: root.path().to_path_buf(),
        read_roots: vec![root.path().to_path_buf()],
        read_roots_include_platform_defaults: true,
        write_roots: Vec::new(),
        deny_read_paths: Vec::new(),
        deny_write_paths: Vec::new(),
        network: WindowsSandboxStandaloneNetworkSetup {
            identity: WindowsSandboxStandaloneNetworkIdentity::Online,
            proxy_ports: Vec::new(),
            allow_local_binding: false,
        },
    }
}

#[test]
fn standalone_policy_constructor_reuses_native_root_and_deny_translation() -> Result<()> {
    let root = TempDir::new()?;
    let resources = write_resources(&root)?;
    let state_dir = root.path().join("state");
    let workspace = root.path().join("workspace");
    let readable = root.path().join("readable");
    let denied_read = readable.join("private");
    let denied_write = workspace.join("protected");
    for path in [&workspace, &readable, &denied_read, &denied_write] {
        fs::create_dir_all(path)?;
    }
    let workspace_roots = vec![AbsolutePathBuf::from_absolute_path(&workspace)?];
    let request = windows_sandbox_standalone_setup_request_from_permission_profile(
        WindowsSandboxStandalonePolicyRequest {
            permission_profile: &PermissionProfile::read_only(),
            workspace_roots: &workspace_roots,
            command_cwd: &workspace,
            environment: &HashMap::new(),
            state_dir: state_dir.clone(),
            resources: resources.clone(),
            filesystem_overrides: WindowsSandboxStandaloneFilesystemOverrides {
                read_roots: Some(vec![readable.clone()]),
                read_roots_include_platform_defaults: false,
                write_roots: Some(vec![workspace.clone()]),
                additional_deny_read_paths: vec![denied_read.clone()],
                additional_deny_write_paths: vec![denied_write.clone()],
            },
            network: WindowsSandboxStandaloneNetworkSetup {
                identity: WindowsSandboxStandaloneNetworkIdentity::Offline,
                proxy_ports: Vec::new(),
                allow_local_binding: false,
            },
        },
    )?;

    assert_eq!(
        request,
        WindowsSandboxStandaloneSetupRequest {
            state_dir: state_dir.clone(),
            resources,
            command_cwd: workspace.clone(),
            read_roots: vec![
                dunce::canonicalize(state_dir.join(".sandbox-bin"))?,
                dunce::canonicalize(readable)?,
            ],
            read_roots_include_platform_defaults: false,
            write_roots: vec![dunce::canonicalize(&workspace)?],
            deny_read_paths: vec![denied_read],
            deny_write_paths: vec![dunce::canonicalize(denied_write)?],
            network: WindowsSandboxStandaloneNetworkSetup {
                identity: WindowsSandboxStandaloneNetworkIdentity::Offline,
                proxy_ports: Vec::new(),
                allow_local_binding: false,
            },
        }
    );
    Ok(())
}

#[test]
fn setup_status_requires_explicit_existing_resources() -> Result<()> {
    let root = TempDir::new()?;
    let request = setup_request(
        &root,
        WindowsSandboxStandaloneResources {
            setup_executable: root.path().join("codex-windows-sandbox-setup.exe"),
            command_runner_executable: root.path().join("codex-command-runner.exe"),
        },
    );

    assert!(matches!(
        windows_sandbox_standalone_setup_status(&request),
        WindowsSandboxStandaloneSetupState::Unavailable { .. }
    ));
    Ok(())
}

#[test]
fn setup_status_requires_a_fixed_sibling_companion_layout() -> Result<()> {
    let root = TempDir::new()?;
    let other = TempDir::new()?;
    let setup_executable = root.path().join("codex-windows-sandbox-setup.exe");
    let command_runner_executable = other.path().join("codex-command-runner.exe");
    fs::write(&setup_executable, b"setup")?;
    fs::write(&command_runner_executable, b"runner")?;
    let request = setup_request(
        &root,
        WindowsSandboxStandaloneResources {
            setup_executable,
            command_runner_executable,
        },
    );

    assert!(matches!(
        windows_sandbox_standalone_setup_status(&request),
        WindowsSandboxStandaloneSetupState::Unavailable { .. }
    ));
    Ok(())
}

#[test]
fn setup_status_reports_administrative_action_for_unprepared_state() -> Result<()> {
    let root = TempDir::new()?;
    let resources = write_resources(&root)?;
    let request = setup_request(&root, resources);

    assert!(matches!(
        windows_sandbox_standalone_setup_status(&request),
        WindowsSandboxStandaloneSetupState::AdministrativeActionRequired { .. }
    ));
    assert!(!request.state_dir.exists());
    Ok(())
}

#[test]
fn setup_status_rejects_proxy_exceptions_for_online_identity() -> Result<()> {
    let root = TempDir::new()?;
    let resources = write_resources(&root)?;
    let mut request = setup_request(&root, resources);
    request.network.proxy_ports = vec![8080];

    assert!(matches!(
        windows_sandbox_standalone_setup_status(&request),
        WindowsSandboxStandaloneSetupState::Unavailable { .. }
    ));
    Ok(())
}

#[test]
fn setup_status_rejects_relative_policy_paths() -> Result<()> {
    let root = TempDir::new()?;
    let resources = write_resources(&root)?;
    let mut request = setup_request(&root, resources);
    request.read_roots = vec![PathBuf::from("relative")];

    assert!(matches!(
        windows_sandbox_standalone_setup_status(&request),
        WindowsSandboxStandaloneSetupState::Unavailable { .. }
    ));
    Ok(())
}

#[test]
fn native_command_and_environment_values_preserve_unpaired_surrogates() -> Result<()> {
    use crate::WindowsSandboxStandaloneCommand;
    use std::os::windows::ffi::OsStringExt;

    let native_arg = OsString::from_wide(&[b'a' as u16, 0xd800, b'z' as u16]);
    let native_value = OsString::from_wide(&[0xdc00, b'=' as u16]);
    let root = TempDir::new()?;
    let program = root.path().join("program.exe");
    fs::write(&program, b"program")?;
    let command = WindowsSandboxStandaloneCommand {
        program,
        args: vec![native_arg.clone()],
        environment: vec![(OsString::from("VALUE"), native_value.clone())],
        cwd: root.path().to_path_buf(),
    };

    command.validate()?;
    assert_eq!(command.args, vec![native_arg]);
    assert_eq!(
        command.environment,
        vec![(OsString::from("VALUE"), native_value)]
    );
    Ok(())
}

#[test]
fn native_environment_rejects_non_unicode_and_case_duplicate_names() -> Result<()> {
    use crate::WindowsSandboxStandaloneCommand;
    use std::os::windows::ffi::OsStringExt;

    let root = TempDir::new()?;
    let program = root.path().join("program.exe");
    fs::write(&program, b"program")?;
    let mut command = WindowsSandboxStandaloneCommand {
        program,
        args: Vec::new(),
        environment: vec![(
            OsString::from_wide(&[b'N' as u16, 0xd800]),
            OsString::from("value"),
        )],
        cwd: root.path().to_path_buf(),
    };

    assert!(command.validate().is_err());
    command.environment = vec![
        (OsString::from("ä"), OsString::from("first")),
        (OsString::from("Ä"), OsString::from("second")),
    ];
    assert!(command.validate().is_err());
    Ok(())
}
