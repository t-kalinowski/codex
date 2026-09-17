//! Process-wide product identity for the launcher and its dedicated helpers.
//! Select it at executable startup, before using any sandbox functionality.

use std::borrow::Cow;
use std::sync::OnceLock;

use anyhow::Result;
use anyhow::bail;
use windows_sys::core::GUID;

/// Selects independent accounts, helpers, and kernel object names for a host product.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowsSandboxProduct {
    #[default]
    Codex,
    Console,
}

static PRODUCT: OnceLock<WindowsSandboxProduct> = OnceLock::new();

impl WindowsSandboxProduct {
    /// Must be called before the first sandbox operation. The selection cannot change.
    pub fn initialize(self) -> Result<()> {
        if *PRODUCT.get_or_init(|| self) != self {
            bail!("Windows sandbox product was already initialized differently");
        }
        Ok(())
    }

    /// Existing callers default to Codex and retain their installed resource identities.
    pub fn current() -> Self {
        *PRODUCT.get_or_init(Self::default)
    }

    /// Maps a shared backend resource name into this product's stable namespace.
    pub fn name(self, name: &str) -> Cow<'_, str> {
        match self {
            Self::Codex => Cow::Borrowed(name),
            Self::Console => match name {
                // NetUserAdd limits account login names to 20 characters.
                "CodexSandboxOffline" => Cow::Borrowed("McpConsoleSandboxOff"),
                "CodexSandboxOnline" => Cow::Borrowed("McpConsoleSandboxOn"),
                "codex-windows-sandbox-setup.exe" => Cow::Borrowed("mcp-console-sandbox-setup.exe"),
                "codex-command-runner.exe" => Cow::Borrowed("mcp-console-sandbox-runner.exe"),
                _ => Cow::Owned(name.replace("Codex", "Console").replace("codex", "console")),
            },
        }
    }

    /// Keeps Console's persistent WFP objects separate from existing Codex filters.
    pub(crate) fn wfp_key(self, mut key: GUID) -> GUID {
        if self == Self::Console {
            // Fixed namespace transform. Never change it after installation.
            key.data1 ^= 0xa71c_093e;
        }
        key
    }
}

/// Resolves a backend resource name using the executable's immutable product selection.
pub fn sandbox_name(name: &str) -> Cow<'_, str> {
    WindowsSandboxProduct::current().name(name)
}
