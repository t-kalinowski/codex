//! Private machine protocol and runtime for the MCP Console sandbox runner.

pub mod capabilities;
pub mod cleanup;
pub mod environment;
pub mod framing;
#[cfg(unix)]
pub mod launch;
#[cfg(unix)]
pub mod launch_bridge;
#[cfg(target_os = "macos")]
pub mod lifetime;
pub mod network;
#[cfg(unix)]
pub mod platform;
pub mod policy;
#[cfg(target_os = "macos")]
mod process;
#[cfg(target_os = "macos")]
mod process_tracker;
pub mod protocol;
pub mod stdio;
#[cfg(unix)]
pub mod supervisor;

/// Version of the executable control protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum encoded JSON payload accepted by one control frame.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Stable Codex release on which this rolling patch is based.
pub const RELEASE_TAG: &str = "rust-v0.150.1";
