use codex_sandbox_api::CommandSpec;
use codex_sandbox_api::SandboxPolicy;
use codex_sandbox_api::SandboxRequest;
use codex_sandbox_api::SandboxRuntime;
use codex_sandbox_api::SandboxRuntimeConfig;
use codex_sandbox_api::SandboxStdioMode;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::io::Write;

pub(super) fn inherit_driver(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let state_dir = super::next_path(args, "state directory")?;
    let cwd = super::next_path(args, "working directory")?;
    let terminal_policy = super::next_string(args, "terminal policy")?;
    let target_mode = super::next_os(args, "target mode")?;
    fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let target_env = env::vars_os()
        .filter(|(key, _)| {
            !cfg!(target_os = "linux") || !key.as_encoded_bytes().starts_with(b"LD_")
        })
        .filter(|(key, _)| {
            !cfg!(target_os = "macos") || !key.as_encoded_bytes().starts_with(b"DYLD_")
        })
        .collect::<BTreeMap<_, _>>();
    let command = CommandSpec::new(executable, &cwd, target_env)
        .arg(target_mode)
        .args(args);
    let mut policy = SandboxPolicy::host_read_only()
        .read_write(&cwd)
        .network_unrestricted();
    match terminal_policy.as_str() {
        "default" => {}
        "isolated" => policy = policy.terminal_inherited_or_created(),
        _ => return Err(format!("unknown terminal policy `{terminal_policy}`")),
    }
    let request = SandboxRequest::new(command, policy)
        .stdin(SandboxStdioMode::Inherit)
        .stdout(SandboxStdioMode::Inherit)
        .stderr(SandboxStdioMode::Inherit);
    let runtime = SandboxRuntime::new(SandboxRuntimeConfig::new(state_dir))
        .map_err(|error| error.to_string())?;
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    async_runtime.block_on(async move {
        let child = runtime
            .spawn(request)
            .await
            .map_err(|error| error.to_string())?;
        let status = child
            .process()
            .wait_root()
            .await
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!("inherited-stdio target failed: {status:?}"));
        }
        Ok(())
    })
}

pub(super) fn inherited_regular() -> Result<(), String> {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(format!(
                "fd {fd} is not a regular file: {:#o}",
                stat.st_mode
            ));
        }
    }

    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .lock()
        .write_all(b"native-stdout\0\x80\xff")
        .map_err(|error| error.to_string())?;
    std::io::stderr()
        .lock()
        .write_all(b"native-stderr\0\x81\xfe")
        .map_err(|error| error.to_string())?;
    if input != b"native-stdin\0\x82\xfd" {
        return Err(format!("stdin bytes changed: {input:?}"));
    }
    Ok(())
}

pub(super) fn null_device() -> Result<(), String> {
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| error.to_string())?;
    if !input.is_empty() {
        return Err(format!("null stdin returned bytes: {input:?}"));
    }
    std::io::stdout()
        .lock()
        .write_all(b"discarded stdout")
        .map_err(|error| error.to_string())?;
    std::io::stderr()
        .lock()
        .write_all(b"discarded stderr")
        .map_err(|error| error.to_string())
}

pub(super) fn inherited_pty(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let marker = super::next_path(args, "marker path")?;
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(format!("fd {fd} is not an inherited terminal"));
        }
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, attributes.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    fs::write(marker, b"inherited-pty-ok").map_err(|error| error.to_string())
}
