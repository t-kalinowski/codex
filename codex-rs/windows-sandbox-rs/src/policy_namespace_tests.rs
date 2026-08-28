use crate::policy_namespace::WindowsSandboxPolicyNamespace;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

#[test]
fn ordinary_policy_identifiers_remain_unchanged() {
    let namespace = WindowsSandboxPolicyNamespace::Codex;

    assert_eq!(namespace.offline_username(), "CodexSandboxOffline");
    assert_eq!(namespace.online_username(), "CodexSandboxOnline");
    assert_eq!(namespace.users_group(), "CodexSandboxUsers");
    assert_eq!(
        namespace.read_acl_mutex_name(),
        r"Local\CodexSandboxReadAcl"
    );
}

#[test]
fn mcp_console_principals_and_mutex_are_disjoint_from_codex() {
    let codex = WindowsSandboxPolicyNamespace::Codex;
    let mcp_console = WindowsSandboxPolicyNamespace::McpConsole;
    let codex_principals = [
        codex.offline_username(),
        codex.online_username(),
        codex.users_group(),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let mcp_console_principals = [
        mcp_console.offline_username(),
        mcp_console.online_username(),
        mcp_console.users_group(),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();

    assert!(codex_principals.is_disjoint(&mcp_console_principals));
    assert_ne!(
        codex.read_acl_mutex_name(),
        mcp_console.read_acl_mutex_name()
    );
}
