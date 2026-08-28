//! Private machine protocol and runtime for the MCP Console sandbox runner.

pub mod capabilities;
pub mod environment;
pub mod framing;
pub mod launch;
#[cfg(unix)]
pub mod launch_bridge;
pub mod network;
pub mod platform;
pub mod policy;
pub mod protocol;
pub mod setup;
pub mod stdio;
pub mod supervisor;
pub mod watchdog;

/// Version of the executable control protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum encoded JSON payload accepted by one control frame.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Stable Codex release on which this rolling patch is based.
pub const RELEASE_TAG: &str = "rust-v0.150.1";
