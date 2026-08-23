use crate::FileSystemBase;
use crate::MissingPathBehavior;
use crate::NetworkPolicy;
use crate::PathAccess;
use crate::PathRule;
use crate::SandboxBackend;
use crate::SandboxError;
use crate::SandboxFeature;
use crate::SandboxPolicy;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::permissions::PROTECTED_METADATA_PATH_NAMES;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::path::PathBuf;

#[path = "policy/internal.rs"]
mod internal;
#[cfg(target_os = "linux")]
#[path = "policy/linux.rs"]
mod linux;

pub(crate) struct PreparedPolicy {
    pub(crate) file_system: FileSystemSandboxPolicy,
    pub(crate) network: NetworkSandboxPolicy,
}

pub(crate) fn prepare_policy(
    policy: &SandboxPolicy,
    cwd: &Path,
    backend: SandboxBackend,
    internal_read_roots: &[PathBuf],
) -> Result<PreparedPolicy, SandboxError> {
    validate_absolute_path(cwd, "the command working directory")?;
    let rules = existing_rules(&policy.filesystem.rules)?;
    let resolved_rules = rules
        .iter()
        .map(|rule| {
            rule.path
                .canonicalize()
                .map(|path| (rule, path))
                .map_err(|source| SandboxError::InvalidPath {
                    path: rule.path.clone(),
                    message: format!("failed to resolve a filesystem rule: {source}"),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_backend_policy(policy, &resolved_rules, backend)?;

    for internal_root in internal_read_roots {
        validate_absolute_path(internal_root, "an internal sandbox helper")?;
        validate_existing_path_encoding(internal_root, "an internal sandbox helper")?;
        match internal::explicit_access_for_internal(&resolved_rules, internal_root)? {
            Some(PathAccess::Deny) => {
                return Err(unsupported(
                    backend,
                    SandboxFeature::DeniedReadPaths,
                    "a deny rule covers a path required by the sandbox backend",
                ));
            }
            Some(PathAccess::Write) => {
                return Err(unsupported(
                    backend,
                    SandboxFeature::DeniedWritePaths,
                    "a write rule covers a path that the sandbox backend must keep read-only",
                ));
            }
            Some(PathAccess::Read) | None => {}
        }
    }

    let base_path = match policy.filesystem.base {
        FileSystemBase::PlatformMinimal => FileSystemSpecialPath::Minimal,
        FileSystemBase::HostReadOnly => FileSystemSpecialPath::Root,
    };
    let mut entries = vec![FileSystemSandboxEntry::new(
        FileSystemPath::Special { value: base_path },
        FileSystemAccessMode::Read,
    )];

    for rule in &rules {
        let path = absolute_path(&rule.path)?;
        let access = codex_access(rule.access);
        entries.push(FileSystemSandboxEntry::new(path.into(), access));

        // Seatbelt's platform-default allowances are independent of normal
        // readable-root exclusions. This final subtree deny keeps an explicit
        // deny authoritative even under one of those default roots.
        #[cfg(target_os = "macos")]
        if backend == SandboxBackend::MacosSeatbelt && rule.access == PathAccess::Deny {
            entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::GlobPattern {
                    pattern: escape_literal_glob(&rule.path)?,
                },
                FileSystemAccessMode::Deny,
            ));
        }
    }

    for internal_root in internal_read_roots {
        entries.push(FileSystemSandboxEntry::new(
            absolute_path(internal_root)?.into(),
            FileSystemAccessMode::Read,
        ));
    }

    // Codex normally keeps repository metadata read-only under writable roots.
    // The embedding contract grants the requested root, so restore exact writes
    // wherever the facade policy itself leaves that access writable.
    for (rule, resolved_rule) in resolved_rules
        .iter()
        .filter(|(rule, _)| rule.access == PathAccess::Write && rule.path.is_dir())
    {
        for name in PROTECTED_METADATA_PATH_NAMES {
            let metadata_path = rule.path.join(name);
            let resolved_metadata_path = match metadata_path.canonicalize() {
                Ok(path) => path,
                Err(source) => match metadata_path.try_exists() {
                    Ok(false) => resolved_rule.join(name),
                    Ok(true) => {
                        return Err(SandboxError::InvalidPath {
                            path: metadata_path,
                            message: format!("failed to resolve protected metadata path: {source}"),
                        });
                    }
                    Err(inspect_source) => {
                        return Err(SandboxError::InvalidPath {
                            path: metadata_path,
                            message: format!(
                                "failed to inspect protected metadata path: {inspect_source}"
                            ),
                        });
                    }
                },
            };
            let explicit_access = resolved_rules
                .iter()
                .filter(|(_, rule_path)| resolved_metadata_path.starts_with(rule_path))
                .max_by_key(|(rule, rule_path)| rule_precedence(rule_path, rule.access))
                .map(|(rule, _)| rule.access);
            let access = explicit_access.unwrap_or(match policy.filesystem.base {
                FileSystemBase::PlatformMinimal => PathAccess::Deny,
                FileSystemBase::HostReadOnly => PathAccess::Read,
            });
            if access == PathAccess::Write {
                entries.push(FileSystemSandboxEntry::new(
                    absolute_path(&metadata_path)?.into(),
                    FileSystemAccessMode::Write,
                ));
            }
        }
    }

    let file_system = FileSystemSandboxPolicy::restricted(entries);
    #[cfg(target_os = "linux")]
    if backend == SandboxBackend::LinuxBubblewrap {
        linux::validate_path_stability(&file_system, cwd, backend)?;
    }

    let network = match policy.network {
        NetworkPolicy::Denied => NetworkSandboxPolicy::Restricted,
        NetworkPolicy::Unrestricted => NetworkSandboxPolicy::Enabled,
    };
    Ok(PreparedPolicy {
        file_system,
        network,
    })
}

fn existing_rules(rules: &[PathRule]) -> Result<Vec<PathRule>, SandboxError> {
    let mut existing = Vec::with_capacity(rules.len());
    for rule in rules {
        validate_absolute_path(&rule.path, "a filesystem rule")?;
        match rule.path.try_exists() {
            Ok(true) => {
                validate_existing_path_encoding(&rule.path, "a filesystem rule")?;
                existing.push(rule.clone());
            }
            Ok(false) if rule.missing == MissingPathBehavior::Ignore => {}
            Ok(false) => {
                return Err(SandboxError::InvalidPath {
                    path: rule.path.clone(),
                    message: "the requested policy path does not exist".to_string(),
                });
            }
            Err(source) => {
                return Err(SandboxError::InvalidPath {
                    path: rule.path.clone(),
                    message: format!("failed to inspect the requested policy path: {source}"),
                });
            }
        }
    }
    Ok(existing)
}

fn validate_backend_policy(
    policy: &SandboxPolicy,
    resolved_rules: &[(&PathRule, PathBuf)],
    backend: SandboxBackend,
) -> Result<(), SandboxError> {
    if backend == SandboxBackend::WindowsRestrictedToken {
        if policy.filesystem.base == FileSystemBase::PlatformMinimal {
            return Err(unsupported(
                backend,
                SandboxFeature::MinimalReadPolicy,
                "the restricted-token backend requires host filesystem reads",
            ));
        }
        if resolved_rules
            .iter()
            .any(|(rule, _)| rule.access == PathAccess::Deny)
        {
            return Err(unsupported(
                backend,
                SandboxFeature::DeniedReadPaths,
                "the restricted-token backend cannot enforce read denials",
            ));
        }
        if policy.network == NetworkPolicy::Denied {
            return Err(unsupported(
                backend,
                SandboxFeature::NetworkDenial,
                "the restricted-token backend does not isolate direct network access",
            ));
        }

        let explicit_access_at = |path: &Path| {
            resolved_rules
                .iter()
                .filter(|(_, rule_path)| path.starts_with(rule_path))
                .max_by_key(|(rule, rule_path)| rule_precedence(rule_path, rule.access))
                .map(|(rule, _)| rule.access)
        };
        let explicit_lexical_access_at = |path: &Path| {
            resolved_rules
                .iter()
                .map(|(rule, _)| *rule)
                .filter(|rule| path.starts_with(&rule.path))
                .max_by_key(|rule| rule_precedence(&rule.path, rule.access))
                .map(|rule| rule.access)
        };
        for (read_only_rule, read_only_path) in resolved_rules
            .iter()
            .filter(|(rule, _)| rule.access == PathAccess::Read)
        {
            let carves_out_resolved_writable_parent =
                resolved_rules.iter().any(|(rule, rule_path)| {
                    rule.access == PathAccess::Write
                        && rule_path != read_only_path
                        && read_only_path.starts_with(rule_path)
                });
            let carves_out_lexical_writable_parent = resolved_rules.iter().any(|(rule, _)| {
                rule.access == PathAccess::Write
                    && rule.path != read_only_rule.path
                    && read_only_rule.path.starts_with(&rule.path)
            });
            let carves_out_writable_parent = explicit_access_at(read_only_path)
                == Some(PathAccess::Read)
                && explicit_lexical_access_at(&read_only_rule.path) == Some(PathAccess::Read)
                && (carves_out_resolved_writable_parent || carves_out_lexical_writable_parent);
            if carves_out_writable_parent
                && resolved_rules.iter().any(|(rule, rule_path)| {
                    rule.access == PathAccess::Write
                        && rule_path != read_only_path
                        && rule_path.starts_with(read_only_path)
                        && explicit_access_at(rule_path) == Some(PathAccess::Write)
                })
            {
                return Err(unsupported(
                    backend,
                    SandboxFeature::NestedAllowUnderDeny,
                    "restricted-token ACLs cannot reopen a writable root below a read-only carveout",
                ));
            }
        }
    }

    #[cfg(target_os = "linux")]
    if backend == SandboxBackend::LinuxBubblewrap {
        if resolved_rules
            .iter()
            .any(|(rule, _)| rule.access == PathAccess::Write && rule.path.parent().is_none())
        {
            return Err(unsupported(
                backend,
                SandboxFeature::DeniedWritePaths,
                "a writable filesystem root would bypass bubblewrap filesystem isolation",
            ));
        }
        if resolved_rules.iter().any(|(rule, rule_path)| {
            rule.access == PathAccess::Read
                && codex_linux_sandbox::read_rule_overlaps_implicit_writable_dev(rule_path)
        }) {
            return Err(unsupported(
                backend,
                SandboxFeature::DeniedWritePaths,
                "bubblewrap's implicit writable /dev mount overlaps a requested read-only rule",
            ));
        }
        let proc = Path::new("/proc");
        for (rule, resolved) in resolved_rules {
            if resolved.starts_with(proc) || proc.starts_with(resolved) {
                let feature = match rule.access {
                    PathAccess::Deny => SandboxFeature::DeniedReadPaths,
                    PathAccess::Read | PathAccess::Write => SandboxFeature::DeniedWritePaths,
                };
                return Err(unsupported(
                    backend,
                    feature,
                    "bubblewrap's required fresh procfs mount would replace this filesystem rule",
                ));
            }
        }
        for (_, deny_path) in resolved_rules
            .iter()
            .filter(|(rule, _)| rule.access == PathAccess::Deny)
        {
            if resolved_rules.iter().any(|(rule, rule_path)| {
                rule.access == PathAccess::Read
                    && rule_path != deny_path
                    && rule_path.starts_with(deny_path)
            }) {
                return Err(unsupported(
                    backend,
                    SandboxFeature::NestedAllowUnderDeny,
                    "bubblewrap cannot reopen a nested read-only allow under a deny",
                ));
            }
        }
    }

    #[cfg(target_os = "macos")]
    if backend == SandboxBackend::MacosSeatbelt {
        if resolved_rules
            .iter()
            .any(|(rule, path)| rule.access == PathAccess::Deny && path.parent().is_none())
        {
            return Err(unsupported(
                backend,
                SandboxFeature::DeniedReadPaths,
                "Seatbelt cannot express a root deny without leaving platform allowances readable",
            ));
        }
        let device_root = Path::new("/dev");
        if resolved_rules.iter().any(|(rule, rule_path)| {
            rule.access == PathAccess::Read
                && ((policy.filesystem.base == FileSystemBase::PlatformMinimal
                    && codex_sandboxing::seatbelt::restricted_platform_defaults_overlap_writes(
                        rule_path,
                    ))
                    || rule_path.starts_with(device_root)
                    || device_root.starts_with(rule_path))
        }) {
            return Err(unsupported(
                backend,
                SandboxFeature::DeniedWritePaths,
                "Seatbelt runtime allowances would reopen writes below this read-only rule",
            ));
        }
        for (_, deny_path) in resolved_rules
            .iter()
            .filter(|(rule, _)| rule.access == PathAccess::Deny)
        {
            if resolved_rules.iter().any(|(rule, rule_path)| {
                rule.access != PathAccess::Deny
                    && rule_path != deny_path
                    && rule_path.starts_with(deny_path)
            }) {
                return Err(unsupported(
                    backend,
                    SandboxFeature::NestedAllowUnderDeny,
                    "Seatbelt final deny rules cannot reopen a nested allow",
                ));
            }
        }
    }
    Ok(())
}

fn rule_precedence(path: &Path, access: PathAccess) -> (usize, u8) {
    let precedence = match access {
        PathAccess::Read => 0,
        PathAccess::Write => 1,
        PathAccess::Deny => 2,
    };
    (path.components().count(), precedence)
}

fn codex_access(access: PathAccess) -> FileSystemAccessMode {
    match access {
        PathAccess::Read => FileSystemAccessMode::Read,
        PathAccess::Write => FileSystemAccessMode::Write,
        PathAccess::Deny => FileSystemAccessMode::Deny,
    }
}

fn absolute_path(path: &Path) -> Result<AbsolutePathBuf, SandboxError> {
    if path.to_str().is_none() {
        return Err(SandboxError::InvalidPath {
            path: path.to_path_buf(),
            message: "sandbox policy paths must be valid UTF-8 for the selected backend"
                .to_string(),
        });
    }
    AbsolutePathBuf::from_absolute_path(path).map_err(|source| SandboxError::InvalidPath {
        path: path.to_path_buf(),
        message: source.to_string(),
    })
}

pub(crate) fn validate_absolute_path(
    path: &Path,
    description: &'static str,
) -> Result<(), SandboxError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(SandboxError::InvalidPath {
            path: path.to_path_buf(),
            message: format!("{description} must be absolute"),
        })
    }
}

pub(crate) fn validate_existing_path_encoding(
    path: &Path,
    description: &'static str,
) -> Result<(), SandboxError> {
    if path.to_str().is_none() {
        return Err(SandboxError::InvalidPath {
            path: path.to_path_buf(),
            message: format!("{description} must be valid UTF-8 for the selected backend"),
        });
    }
    let resolved = path
        .canonicalize()
        .map_err(|source| SandboxError::InvalidPath {
            path: path.to_path_buf(),
            message: format!("failed to resolve {description}: {source}"),
        })?;
    if resolved.to_str().is_none() {
        return Err(SandboxError::InvalidPath {
            path: path.to_path_buf(),
            message: format!(
                "{description} resolves to a path that is not valid UTF-8 for the selected backend"
            ),
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn escape_literal_glob(path: &Path) -> Result<String, SandboxError> {
    let path = path.to_str().ok_or_else(|| SandboxError::InvalidPath {
        path: path.to_path_buf(),
        message: "Seatbelt policy paths must be valid UTF-8".to_string(),
    })?;
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    Ok(escaped)
}

fn unsupported(
    backend: SandboxBackend,
    feature: SandboxFeature,
    message: impl Into<String>,
) -> SandboxError {
    SandboxError::UnsupportedPolicy {
        backend,
        feature,
        message: message.into(),
    }
}
