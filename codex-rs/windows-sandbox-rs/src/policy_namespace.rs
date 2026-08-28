use serde::Deserialize;
use serde::Serialize;

const CODEX_OFFLINE_USERNAME: &str = "CodexSandboxOffline";
const CODEX_ONLINE_USERNAME: &str = "CodexSandboxOnline";
const CODEX_USERS_GROUP: &str = "CodexSandboxUsers";
const CODEX_READ_ACL_MUTEX_NAME: &str = r"Local\CodexSandboxReadAcl";

const MCP_CONSOLE_OFFLINE_USERNAME: &str = "McpConsoleSbxOffline";
const MCP_CONSOLE_ONLINE_USERNAME: &str = "McpConsoleSbxOnline";
const MCP_CONSOLE_USERS_GROUP: &str = "McpConsoleSbxUsers";
const MCP_CONSOLE_READ_ACL_MUTEX_NAME: &str = r"Local\McpConsoleSandboxReadAcl";

/// Selects one closed set of Windows sandbox identities and machine policy objects.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsSandboxPolicyNamespace {
    #[default]
    Codex,
    McpConsole,
}

impl WindowsSandboxPolicyNamespace {
    pub const fn offline_username(self) -> &'static str {
        match self {
            Self::Codex => CODEX_OFFLINE_USERNAME,
            Self::McpConsole => MCP_CONSOLE_OFFLINE_USERNAME,
        }
    }

    pub const fn online_username(self) -> &'static str {
        match self {
            Self::Codex => CODEX_ONLINE_USERNAME,
            Self::McpConsole => MCP_CONSOLE_ONLINE_USERNAME,
        }
    }

    pub const fn users_group(self) -> &'static str {
        match self {
            Self::Codex => CODEX_USERS_GROUP,
            Self::McpConsole => MCP_CONSOLE_USERS_GROUP,
        }
    }

    pub const fn read_acl_mutex_name(self) -> &'static str {
        match self {
            Self::Codex => CODEX_READ_ACL_MUTEX_NAME,
            Self::McpConsole => MCP_CONSOLE_READ_ACL_MUTEX_NAME,
        }
    }

    pub fn identities_match(self, offline_username: &str, online_username: &str) -> bool {
        matches!(
            (self, offline_username, online_username),
            (Self::Codex, CODEX_OFFLINE_USERNAME, CODEX_ONLINE_USERNAME)
                | (
                    Self::McpConsole,
                    MCP_CONSOLE_OFFLINE_USERNAME,
                    MCP_CONSOLE_ONLINE_USERNAME
                )
        )
    }

    pub const fn is_codex(&self) -> bool {
        matches!(self, Self::Codex)
    }
}
