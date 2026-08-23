use super::*;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

#[test]
fn preparation_uses_compatible_system_bwrap_from_supplied_path() {
    let temp_dir = tempdir().expect("temp directory");
    let cwd = temp_dir.path().join("cwd");
    let bin = temp_dir.path().join("bin");
    std::fs::create_dir(&cwd).expect("create cwd");
    std::fs::create_dir(&bin).expect("create bin");
    let bwrap = bin.join("bwrap");
    write_probe_bwrap(
        &bwrap, /*namespace_exit_code*/ 0, /*supports_perms*/ true,
    );
    let search_path = std::env::join_paths([bin]).expect("join search path");

    let launcher = prepare_embedding_bwrap(
        Some(&search_path),
        &cwd,
        &temp_dir.path().join("missing-helper"),
    )
    .expect("select system bubblewrap");

    assert_eq!(launcher.kind(), EmbeddingBwrapKind::System);
    assert_eq!(launcher.program(), bwrap);
}

#[test]
fn preparation_rejects_incompatible_or_unusable_system_bwrap() {
    for (supports_perms, namespace_exit_code) in [(false, 0), (true, 7)] {
        let temp_dir = tempdir().expect("temp directory");
        let cwd = temp_dir.path().join("cwd");
        let bin = temp_dir.path().join("bin");
        std::fs::create_dir(&cwd).expect("create cwd");
        std::fs::create_dir(&bin).expect("create bin");
        let bwrap = bin.join("bwrap");
        write_probe_bwrap(&bwrap, namespace_exit_code, supports_perms);
        let search_path = std::env::join_paths([bin]).expect("join search path");

        assert!(
            prepare_embedding_bwrap(
                Some(&search_path),
                &cwd,
                &temp_dir.path().join("missing-helper"),
            )
            .is_none()
        );
    }
}

#[test]
fn launcher_rejects_a_removed_pinned_bwrap() {
    let temp_dir = tempdir().expect("temp directory");
    let cwd = temp_dir.path().join("cwd");
    let bin = temp_dir.path().join("bin");
    std::fs::create_dir(&cwd).expect("create cwd");
    std::fs::create_dir(&bin).expect("create bin");
    let bwrap = bin.join("bwrap");
    write_probe_bwrap(
        &bwrap, /*namespace_exit_code*/ 0, /*supports_perms*/ true,
    );
    let search_path = std::env::join_paths([bin]).expect("join search path");
    let launcher = prepare_embedding_bwrap(
        Some(&search_path),
        &cwd,
        &temp_dir.path().join("missing-helper"),
    )
    .expect("select system bubblewrap");

    std::fs::remove_file(bwrap).expect("remove pinned bubblewrap");

    assert!(!launcher.is_available());
}

fn write_probe_bwrap(path: &Path, namespace_exit_code: i32, supports_perms: bool) {
    let permissions_help = if supports_perms {
        "--as-pid-1 --perms"
    } else {
        "--as-pid-1"
    };
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then\n  echo '{permissions_help}'\n  exit 0\nfi\nexit {namespace_exit_code}\n"
        ),
    )
    .expect("write fake bubblewrap");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make fake bubblewrap executable");
}
