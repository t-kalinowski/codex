use crate::bundled_bwrap;
use crate::bundled_bwrap::BundledBwrapLauncher;
use crate::launcher::BubblewrapLauncher;
use crate::launcher::SystemBwrapLauncher;
use crate::launcher::system_bwrap_launcher_for_path;
use clap::Args;
use codex_sandboxing::bwrap_has_namespace_access;
use codex_sandboxing::find_system_bwrap_in_search_path;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Bubblewrap package selected and validated by an embedding application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum EmbeddingBwrapKind {
    System,
    Bundled,
}

impl EmbeddingBwrapKind {
    /// Stable command-line representation accepted by the Linux helper.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Bundled => "bundled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EmbeddingBwrapImplementation {
    System(SystemBwrapLauncher),
    Bundled(BundledBwrapLauncher),
}

/// Opaque handle to the exact bubblewrap executable selected for an embedding runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingBwrapLauncher {
    implementation: EmbeddingBwrapImplementation,
}

impl EmbeddingBwrapLauncher {
    /// Exact executable path that must be passed to the embedding helper.
    pub fn program(&self) -> &Path {
        match &self.implementation {
            EmbeddingBwrapImplementation::System(launcher) => launcher.program.as_path(),
            EmbeddingBwrapImplementation::Bundled(launcher) => launcher.program(),
        }
    }

    /// Packaging kind needed to preserve the selected launch behavior in the helper.
    pub fn kind(&self) -> EmbeddingBwrapKind {
        match &self.implementation {
            EmbeddingBwrapImplementation::System(_) => EmbeddingBwrapKind::System,
            EmbeddingBwrapImplementation::Bundled(_) => EmbeddingBwrapKind::Bundled,
        }
    }

    /// Revalidates that the pinned executable can create the base sandbox namespaces.
    pub fn is_available(&self) -> bool {
        self.is_available_for(/*require_network_namespace*/ false)
    }

    /// Revalidates that the pinned executable can also create a network namespace.
    pub fn network_namespace_available(&self) -> bool {
        self.is_available_for(/*require_network_namespace*/ true)
    }

    fn is_available_for(&self, require_network_namespace: bool) -> bool {
        match &self.implementation {
            EmbeddingBwrapImplementation::System(launcher) => {
                system_bwrap_launcher_for_path(launcher.program.as_path()).is_some()
                    && bwrap_has_namespace_access(
                        launcher.program.as_path(),
                        require_network_namespace,
                    )
            }
            EmbeddingBwrapImplementation::Bundled(launcher) => {
                launcher.has_valid_digest()
                    && bwrap_has_namespace_access(launcher.program(), require_network_namespace)
            }
        }
    }

    fn from_pinned_program(program: &Path, kind: EmbeddingBwrapKind) -> Option<Self> {
        let implementation = match kind {
            EmbeddingBwrapKind::System => {
                EmbeddingBwrapImplementation::System(system_bwrap_launcher_for_path(program)?)
            }
            EmbeddingBwrapKind::Bundled => {
                EmbeddingBwrapImplementation::Bundled(bundled_bwrap::launcher_for_program(program)?)
            }
        };
        Some(Self { implementation })
    }

    fn as_launcher(&self) -> BubblewrapLauncher {
        match &self.implementation {
            EmbeddingBwrapImplementation::System(launcher) => {
                BubblewrapLauncher::System(launcher.clone())
            }
            EmbeddingBwrapImplementation::Bundled(launcher) => {
                BubblewrapLauncher::Bundled(launcher.clone())
            }
        }
    }
}

static EMBEDDING_LAUNCHER: OnceLock<EmbeddingBwrapLauncher> = OnceLock::new();

pub(crate) fn selected_bwrap_launcher() -> Option<BubblewrapLauncher> {
    EMBEDDING_LAUNCHER
        .get()
        .map(EmbeddingBwrapLauncher::as_launcher)
}

fn use_embedding_launcher(program: &Path, kind: EmbeddingBwrapKind) {
    let launcher =
        EmbeddingBwrapLauncher::from_pinned_program(program, kind).unwrap_or_else(|| {
            panic!(
                "the pinned embedding bubblewrap is unavailable: {}",
                program.display()
            )
        });
    if EMBEDDING_LAUNCHER.set(launcher).is_err() {
        panic!("the embedding bubblewrap launcher was configured more than once");
    }
}

/// Selects and validates one bubblewrap executable for an embedding runtime.
///
/// `search_path` and `cwd` must come from the embedding application itself,
/// not from a later child command request. The returned executable remains
/// pinned until the runtime is dropped.
pub fn prepare_embedding_bwrap(
    search_path: Option<&OsStr>,
    cwd: &Path,
    helper_executable: &Path,
) -> Option<EmbeddingBwrapLauncher> {
    if let Some(candidate) = bundled_bwrap::launcher_for_executable(helper_executable)
        && let Ok(program) = std::fs::canonicalize(candidate.program())
        && let Some(launcher) = bundled_bwrap::launcher_for_program(&program)
        && launcher.has_valid_digest()
        && bwrap_has_namespace_access(launcher.program(), /*require_network_namespace*/ false)
    {
        return Some(EmbeddingBwrapLauncher {
            implementation: EmbeddingBwrapImplementation::Bundled(launcher),
        });
    }

    if let Some(system_bwrap) = find_system_bwrap_in_search_path(search_path, cwd)
        && let Some(launcher) = system_bwrap_launcher_for_path(&system_bwrap)
        && bwrap_has_namespace_access(&system_bwrap, /*require_network_namespace*/ false)
    {
        return Some(EmbeddingBwrapLauncher {
            implementation: EmbeddingBwrapImplementation::System(launcher),
        });
    }
    None
}

/// Hidden helper arguments supplied only by an embedding application.
#[derive(Debug, Args)]
pub struct EmbeddingOptions {
    /// Avoid Codex installation discovery for an embedding caller.
    #[arg(
        long = "embedding",
        hide = true,
        default_value_t = false,
        requires_all = [
            "embedding_bwrap",
            "embedding_bwrap_kind",
            "embedding_registry_root"
        ]
    )]
    pub embedding: bool,

    /// Exact bubblewrap executable selected by the embedding runtime.
    #[arg(long = "embedding-bwrap", hide = true, requires = "embedding")]
    pub embedding_bwrap: Option<PathBuf>,

    /// Launch behavior for the selected embedding bubblewrap.
    #[arg(
        long = "embedding-bwrap-kind",
        hide = true,
        value_enum,
        requires = "embedding"
    )]
    pub embedding_bwrap_kind: Option<EmbeddingBwrapKind>,

    /// Application-owned state for embedding helper bookkeeping.
    #[arg(long = "embedding-registry-root", hide = true, requires = "embedding")]
    pub embedding_registry_root: Option<PathBuf>,
}

impl EmbeddingOptions {
    pub(crate) fn activate(self) {
        match (
            self.embedding,
            self.embedding_bwrap,
            self.embedding_bwrap_kind,
            self.embedding_registry_root,
        ) {
            (true, Some(program), Some(kind), Some(registry_root)) => {
                use_embedding_launcher(&program, kind);
                use_embedding_synthetic_mount_registry_root(&registry_root);
            }
            (false, None, None, None) => {}
            _ => panic!("embedding mode requires one pinned bubblewrap launcher and registry root"),
        }
    }
}

static SYNTHETIC_MOUNT_REGISTRY_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn synthetic_mount_registry_root() -> Option<&'static Path> {
    SYNTHETIC_MOUNT_REGISTRY_ROOT.get().map(PathBuf::as_path)
}

fn use_embedding_synthetic_mount_registry_root(registry_root: &Path) {
    if !registry_root.is_absolute() {
        panic!("embedding synthetic mount registry root must be absolute");
    }
    let registry_root = registry_root.canonicalize().unwrap_or_else(|err| {
        panic!(
            "failed to resolve embedding synthetic mount registry root {}: {err}",
            registry_root.display()
        )
    });
    if !registry_root.is_dir() {
        panic!(
            "embedding synthetic mount registry root is not a directory: {}",
            registry_root.display()
        );
    }
    if registry_root.to_str().is_none() {
        panic!("embedding synthetic mount registry root must be valid UTF-8");
    }
    SYNTHETIC_MOUNT_REGISTRY_ROOT
        .set(registry_root)
        .unwrap_or_else(|_| panic!("embedding synthetic mount registry root was already selected"));
}

#[cfg(test)]
#[path = "embedding_tests.rs"]
mod tests;
