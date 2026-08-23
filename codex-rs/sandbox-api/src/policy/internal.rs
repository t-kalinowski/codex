use super::*;

pub(super) fn explicit_access_for_internal(
    rules: &[(&PathRule, PathBuf)],
    internal_root: &Path,
) -> Result<Option<PathAccess>, SandboxError> {
    let resolved_internal =
        internal_root
            .canonicalize()
            .map_err(|source| SandboxError::InvalidPath {
                path: internal_root.to_path_buf(),
                message: format!("failed to resolve an internal sandbox path: {source}"),
            })?;
    let lexical_access = rules
        .iter()
        .filter(|(rule, _)| internal_root.starts_with(&rule.path))
        .max_by_key(|(rule, _)| rule_precedence(&rule.path, rule.access))
        .map(|(rule, _)| rule.access);
    let canonical_access = rules
        .iter()
        .filter(|(_, resolved_rule)| resolved_internal.starts_with(resolved_rule))
        .max_by_key(|(rule, resolved_rule)| rule_precedence(resolved_rule, rule.access))
        .map(|(rule, _)| rule.access);
    let mut conflict = most_restrictive_access(lexical_access, canonical_access)
        .filter(|access| *access != PathAccess::Read);

    for (rule, resolved_rule) in rules {
        let lexical_nested = rule.path.starts_with(internal_root).then(|| {
            rules
                .iter()
                .filter(|(candidate, _)| rule.path.starts_with(&candidate.path))
                .max_by_key(|(candidate, _)| rule_precedence(&candidate.path, candidate.access))
                .map(|(candidate, _)| candidate.access)
        });
        let canonical_nested = resolved_rule.starts_with(&resolved_internal).then(|| {
            rules
                .iter()
                .filter(|(_, candidate)| resolved_rule.starts_with(candidate))
                .max_by_key(|(candidate, path)| rule_precedence(path, candidate.access))
                .map(|(candidate, _)| candidate.access)
        });
        let nested_access =
            most_restrictive_access(lexical_nested.flatten(), canonical_nested.flatten());
        conflict = most_restrictive_access(conflict, nested_access)
            .filter(|access| *access != PathAccess::Read);
    }
    Ok(conflict)
}

fn most_restrictive_access(
    left: Option<PathAccess>,
    right: Option<PathAccess>,
) -> Option<PathAccess> {
    [left, right]
        .into_iter()
        .flatten()
        .max_by_key(|access| match access {
            PathAccess::Read => 0,
            PathAccess::Write => 1,
            PathAccess::Deny => 2,
        })
}
