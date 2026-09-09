#[cfg(target_os = "macos")]
#[path = "platform_macos.rs"]
mod implementation;
#[cfg(target_os = "linux")]
#[path = "platform_linux.rs"]
mod implementation;
pub use implementation::*;
