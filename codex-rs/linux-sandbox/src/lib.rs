//! Linux sandbox helper entry point.
//!
//! On Linux, `codex-linux-sandbox` applies:
//! - in-process restrictions (`no_new_privs` + seccomp), and
//! - bubblewrap for filesystem isolation.
#[cfg(target_os = "linux")]
mod bazel_bwrap;
#[cfg(target_os = "linux")]
mod bundled_bwrap;
#[cfg(target_os = "linux")]
mod bwrap;
#[cfg(target_os = "linux")]
mod exec_util;
#[cfg(target_os = "linux")]
mod fd_mount;
#[cfg(target_os = "linux")]
mod landlock;
#[cfg(target_os = "linux")]
mod launcher;
#[cfg(target_os = "linux")]
mod linux_run_main;
#[cfg(target_os = "linux")]
mod proxy_lifecycle;
#[cfg(target_os = "linux")]
mod proxy_routing;

/// Exit status returned when bundled bubblewrap fails digest verification.
#[cfg(target_os = "linux")]
pub const BUNDLED_BWRAP_DIGEST_VERIFICATION_FAILURE_EXIT_CODE: i32 = 8;

#[cfg(target_os = "linux")]
pub fn run_main() -> ! {
    linux_run_main::run_main();
}

/// Run the native stages with a one-shot target setup hook. The hidden
/// `--target-setup-fd` option is carried through namespace creation. The hook
/// owns that descriptor and runs after enforcement, before spawning the target.
/// It must close the descriptor and establish any target pre-exec state.
#[cfg(target_os = "linux")]
pub fn run_main_with_target_setup(
    setup: fn(&mut std::process::Command, std::os::fd::OwnedFd) -> std::io::Result<()>,
) -> ! {
    linux_run_main::run_main_with_target_setup(Some(setup));
}

#[cfg(not(target_os = "linux"))]
pub fn run_main() -> ! {
    panic!("codex-linux-sandbox is only supported on Linux");
}
