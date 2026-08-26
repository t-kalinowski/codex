use crate::SandboxBackend;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Capability or operation that a selected backend cannot provide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SandboxFeature {
    MinimalReadPolicy,
    DeniedReadPaths,
    DeniedWritePaths,
    NestedAllowUnderDeny,
    NetworkDenial,
    NetworkUnrestricted,
    Interrupt,
    ProcessTreeTermination,
    CurrentProcessGroupTermination,
    TerminalIsolation,
}

/// Error returned while preparing, launching, or controlling a sandbox.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SandboxError {
    #[error("sandboxing is unsupported on platform `{platform}`")]
    UnsupportedPlatform { platform: String },

    #[error("sandbox backend {backend:?} is unavailable: {message}")]
    BackendUnavailable {
        backend: Option<SandboxBackend>,
        message: String,
    },

    #[error("sandbox backend {backend:?} does not support {feature:?}: {message}")]
    UnsupportedPolicy {
        backend: SandboxBackend,
        feature: SandboxFeature,
        message: String,
    },

    #[error("invalid sandbox command: {message}")]
    InvalidCommand { message: String },

    #[error("invalid sandbox operation: {message}")]
    InvalidOperation { message: String },

    #[error("invalid sandbox path `{}`: {message}", path.display())]
    InvalidPath { path: PathBuf, message: String },

    #[error("failed to prepare sandbox backend {backend:?}: {message}")]
    Preparation {
        backend: SandboxBackend,
        message: String,
        #[source]
        source: Option<Box<dyn Error + Send + Sync>>,
    },

    #[error("failed to spawn sandbox backend {backend:?}: {message}")]
    Spawn {
        backend: SandboxBackend,
        message: String,
        #[source]
        source: Option<Box<dyn Error + Send + Sync>>,
    },

    #[error("sandbox I/O failed while {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

impl SandboxError {
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}
