#[cfg(any(target_os = "linux", target_os = "macos"))]
mod bootstrap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod codex;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod launch;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--sandbox-policy-cwd")) {
        // SandboxManager emits this leading option for the native helper.
        // Dispatch by its arguments so argv[0] can remain an executable path
        // when a host bwrap lacks --argv0. Helper re-execs must also retain the
        // descriptors Codex needs temporarily during setup.
        crate::codex::linux_sandbox_main();
    }
    let result = run();
    let code = match result {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            // Match the native CLI's exit_status::handle_exit_status mapping.
            status
                .code()
                .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal))
        }
        Err(error) => {
            eprintln!("mcp-console-sandbox: {error:#}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run() -> anyhow::Result<std::process::ExitStatus> {
    use std::fs::File;
    use std::os::fd::FromRawFd;

    let bootstrap = bootstrap::take_inherited()?;
    // Enumerate before creating the runtime or invoking native setup. This
    // also prevents a caller's accidentally inherited control pipe from
    // reaching the target. New Rust/Tokio descriptors are close-on-exec.
    #[cfg(target_os = "linux")]
    let fd_directory = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    let fd_directory = "/dev/fd";
    for entry in std::fs::read_dir(fd_directory)? {
        let name = entry?.file_name();
        let Some(fd) = name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        if fd > libc::STDERR_FILENO {
            // SAFETY: fcntl only changes this process's descriptor flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    let request = bootstrap::read(bootstrap)?;
    // SAFETY: this executable owns fd 0 and adopts it exactly once, without
    // reading it. Rust startup supplies /dev/null if it was closed at invocation.
    let stdin = unsafe { File::from_raw_fd(libc::STDIN_FILENO) };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(launch::run(request, stdin))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!("mcp-console-sandbox: native runner is supported only on Linux and macOS");
    std::process::exit(1);
}
