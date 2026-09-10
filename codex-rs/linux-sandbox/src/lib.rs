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
#[cfg(target_os = "linux")]
mod target_control;

/// Exit status returned when bundled bubblewrap fails digest verification.
#[cfg(target_os = "linux")]
pub const BUNDLED_BWRAP_DIGEST_VERIFICATION_FAILURE_EXIT_CODE: i32 = 8;

#[cfg(target_os = "linux")]
pub fn run_main() -> ! {
    linux_run_main::run_main();
}

/// Whether setup precedes a namespace-init child or a direct restricted exec.
#[cfg(target_os = "linux")]
pub enum TargetSetupMode {
    Namespace,
    Direct,
}

/// Hook run after native enforcement to apply target state and retain init control.
#[cfg(target_os = "linux")]
pub type TargetSetupHook = fn(
    &mut std::process::Command,
    std::os::fd::OwnedFd,
    TargetSetupMode,
) -> std::io::Result<Option<std::os::fd::OwnedFd>>;

/// Run the native stages with a one-shot target setup hook. The hidden
/// `--target-setup-fd` option is carried through namespace creation. The hook
/// owns that descriptor and runs after enforcement, before spawning the target.
/// It returns the close-on-exec native control descriptor and establishes target
/// pre-exec state. The workload must never inherit this descriptor.
#[cfg(target_os = "linux")]
pub fn run_main_with_target_setup(setup: TargetSetupHook) -> ! {
    linux_run_main::run_main_with_target_setup(Some(setup));
}

#[cfg(not(target_os = "linux"))]
pub fn run_main() -> ! {
    panic!("codex-linux-sandbox is only supported on Linux");
}
