use super::*;
use pretty_assertions::assert_eq;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn packaged_bwrap_uses_only_the_runner_relative_resource() {
    let temp_dir = tempdir().expect("temp directory");
    let runner = temp_dir.path().join("mcp-console-sandbox");
    let resources = temp_dir.path().join("codex-resources");
    let bwrap = resources.join("bwrap");
    let state = temp_dir.path().join("state");
    write_executable(&runner).expect("write runner executable");
    write_compatible_bwrap(&bwrap).expect("write packaged bubblewrap");
    std::fs::create_dir(&state).expect("create application state");

    let embedding =
        prepare_packaged_bwrap(&runner, &state).expect("prepare packaged bubblewrap input");

    assert_eq!(embedding.program(), bwrap);
    assert_eq!(embedding.application_state_dir(), state);
    assert_eq!(
        embedding.helper_args(),
        vec![
            OsString::from("--embedding-bwrap"),
            bwrap.clone().into_os_string(),
            OsString::from("--embedding-state-dir"),
            state.clone().into_os_string(),
            OsString::from("--embedding-command-as-pid-1"),
        ]
    );
    assert_eq!(
        embedding.helper_args_with_info_fd(197),
        vec![
            OsString::from("--embedding-bwrap"),
            bwrap.into_os_string(),
            OsString::from("--embedding-state-dir"),
            state.into_os_string(),
            OsString::from("--embedding-command-as-pid-1"),
            OsString::from("--embedding-bwrap-info-fd"),
            OsString::from("197"),
        ]
    );
}

#[test]
fn packaged_bwrap_does_not_search_an_incidental_path() {
    let temp_dir = tempdir().expect("temp directory");
    let runner = temp_dir.path().join("mcp-console-sandbox");
    let state = temp_dir.path().join("state");
    let incidental_bwrap = temp_dir.path().join("bin").join("bwrap");
    write_executable(&runner).expect("write runner executable");
    write_executable(&incidental_bwrap).expect("write incidental bubblewrap");
    std::fs::create_dir(&state).expect("create application state");

    let error = prepare_packaged_bwrap(&runner, &state)
        .expect_err("missing runner-relative resource must fail");

    assert!(error.contains("codex-resources/bwrap"));
}

#[test]
fn packaged_bwrap_rejects_a_non_unicode_canonical_path() {
    let temp_dir = tempdir().expect("temp directory");
    let runner = temp_dir.path().join("mcp-console-sandbox");
    let resources = temp_dir.path().join("codex-resources");
    let packaged_bwrap = resources.join("bwrap");
    let native_parent = temp_dir.path().join(OsString::from_vec(vec![
        b'n', b'a', b't', b'i', b'v', b'e', 0xff,
    ]));
    let native_bwrap = native_parent.join("codex-resources").join("bwrap");
    let state = temp_dir.path().join("state");
    write_executable(&runner).expect("write runner executable");
    write_executable(&native_bwrap).expect("write native-path bubblewrap");
    std::fs::create_dir_all(&resources).expect("create packaged resource directory");
    symlink(&native_bwrap, &packaged_bwrap).expect("link packaged bubblewrap");
    std::fs::create_dir(&state).expect("create application state");

    let error = prepare_packaged_bwrap(&runner, &state)
        .expect_err("non-Unicode canonical companion path must fail");

    assert_eq!(
        error,
        "packaged bubblewrap path must be valid UTF-8 for embedding"
    );
}

fn write_executable(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"#!/bin/sh\nexit 0\n")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn write_compatible_bwrap(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\nif [ \"$#\" -eq 1 ] && [ \"$1\" = '{}' ]; then\n  printf '%s' '{}'\n  exit 0\nfi\nexit 2\n",
            crate::bundled_bwrap::COMPATIBILITY_QUERY,
            crate::bundled_bwrap::COMPATIBILITY_RESPONSE,
        ),
    )?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
