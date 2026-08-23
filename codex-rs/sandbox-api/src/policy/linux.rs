use super::*;

pub(super) fn validate_path_stability(
    policy: &FileSystemSandboxPolicy,
    cwd: &Path,
    backend: SandboxBackend,
) -> Result<(), SandboxError> {
    let writable_roots = policy.get_writable_roots_with_cwd(cwd);
    let mut writable_paths = Vec::with_capacity(writable_roots.len() * 2);
    for writable_root in &writable_roots {
        let path = writable_root.root.as_path();
        writable_paths.push(path.to_path_buf());
        if let Ok(resolved) = path.canonicalize()
            && resolved != path
        {
            writable_paths.push(resolved);
        }
    }

    for path in writable_roots
        .iter()
        .flat_map(|root| root.read_only_subpaths.iter())
    {
        if let Some(symlink) = codex_linux_sandbox::first_writable_symlink_component_in_path(
            path.as_path(),
            &writable_paths,
        ) {
            return Err(unsupported(
                backend,
                SandboxFeature::DeniedWritePaths,
                format!(
                    "a read-only rule crosses writable symlink {}",
                    symlink.display()
                ),
            ));
        }
    }
    for path in policy.get_unreadable_roots_with_cwd(cwd) {
        if let Some(symlink) = codex_linux_sandbox::first_writable_symlink_component_in_path(
            path.as_path(),
            &writable_paths,
        ) {
            return Err(unsupported(
                backend,
                SandboxFeature::DeniedReadPaths,
                format!("a deny rule crosses writable symlink {}", symlink.display()),
            ));
        }
    }
    Ok(())
}
