mod server;

use clap::Parser;
use codex_mcp_console_sandbox::cleanup::CleanupDirectory;
use codex_mcp_console_sandbox::stdio::PassedStreamEndpoints;
#[cfg(unix)]
use codex_mcp_console_sandbox::supervisor::Supervisor;
use std::ffi::OsString;
use std::path::PathBuf;

const SOURCE_REVISION: &str = match option_env!("STABLE_GIT_COMMIT") {
    Some(revision) => revision,
    None => match option_env!("MCP_CONSOLE_SANDBOX_SOURCE_REVISION") {
        Some(revision) => revision,
        None => "",
    },
};
const _: () = {
    let revision = SOURCE_REVISION.as_bytes();
    assert!(
        revision.len() == 40,
        "Codex source revision must be a full Git SHA"
    );
    let mut index = 0;
    while index < revision.len() {
        assert!(
            revision[index].is_ascii_hexdigit(),
            "Codex source revision must contain only hexadecimal digits"
        );
        index += 1;
    }
};

#[derive(Debug, Parser)]
#[command(name = "mcp-console-sandbox")]
struct Args {
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    cleanup_dir: PathBuf,
    #[cfg(unix)]
    #[arg(long)]
    control_fd: i32,
    #[cfg(unix)]
    #[arg(long = "stream-fd")]
    stream_fds: Vec<u64>,
    #[cfg(windows)]
    #[arg(long)]
    control_handle: u64,
    #[cfg(windows)]
    #[arg(long = "stream-handle")]
    stream_handles: Vec<u64>,
    #[arg(last = true)]
    target: Vec<OsString>,
}

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(|arg0| std::path::Path::new(arg0).file_name())
        == Some(std::ffi::OsStr::new("codex-linux-sandbox"))
    {
        codex_linux_sandbox::run_main();
    }
    #[cfg(target_os = "macos")]
    if codex_mcp_console_sandbox::lifetime::dispatch_if_requested() {
        return;
    }
    #[cfg(unix)]
    if codex_mcp_console_sandbox::launch_bridge::dispatch_if_requested() {
        return;
    }
    let args = Args::parse();
    #[cfg(unix)]
    let endpoints = PassedStreamEndpoints::claim(&args.stream_fds, args.control_fd);
    #[cfg(windows)]
    let endpoints = PassedStreamEndpoints::claim(&args.stream_handles, args.control_handle);
    let endpoints = match endpoints {
        Ok(endpoints) => endpoints,
        Err(error) => {
            eprintln!(
                "mcp-console-sandbox infrastructure error: invalid native bootstrap endpoint: {error}"
            );
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("mcp-console-sandbox infrastructure error: could not build runtime: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = runtime.block_on(run(args, endpoints)) {
        eprintln!("mcp-console-sandbox infrastructure error: {error}");
        std::process::exit(2);
    }
}

async fn run(args: Args, mut endpoints: PassedStreamEndpoints) -> anyhow::Result<()> {
    let runner_executable = std::env::current_exe()
        .map_err(|error| anyhow::anyhow!("could not resolve runner executable: {error}"))?;
    anyhow::ensure!(
        runner_executable.to_str().is_some(),
        "runner executable path must be valid Unicode"
    );
    anyhow::ensure!(
        args.state_dir.is_absolute(),
        "application state directory must be absolute"
    );
    anyhow::ensure!(
        args.state_dir.to_str().is_some(),
        "application state directory must be valid Unicode"
    );
    std::fs::create_dir_all(&args.state_dir)?;
    let state_dir = args.state_dir.canonicalize()?;
    anyhow::ensure!(
        state_dir.to_str().is_some(),
        "canonical application state directory must be valid Unicode"
    );
    let cleanup_directory = CleanupDirectory::claim(&args.cleanup_dir)?;
    let cleanup_path = cleanup_directory.path();
    anyhow::ensure!(
        !cleanup_path.starts_with(&state_dir) && !state_dir.starts_with(cleanup_path),
        "runner state and target cleanup directories must be separate"
    );
    let mut cleanup_directory = Some(cleanup_directory);
    let mut control = control_channel(&args)?;
    #[cfg(unix)]
    let mut supervisor = None;
    #[cfg(windows)]
    let mut supervisor: Option<()> = None;
    let loop_result = server::control_loop(
        &args,
        &state_dir,
        &mut control,
        &mut supervisor,
        &mut endpoints,
        &mut cleanup_directory,
    )
    .await;
    #[cfg(unix)]
    if let Some(supervisor) = supervisor {
        let outcome = supervisor.retire_on_control_loss().await?;
        anyhow::ensure!(
            outcome.retirement.complete,
            "target retirement failed after control loss{}",
            outcome
                .retirement
                .error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        );
        anyhow::ensure!(
            outcome.infrastructure.error.is_none(),
            "target infrastructure failed after control loss: {}",
            outcome.infrastructure.error.as_deref().unwrap_or_default()
        );
        anyhow::ensure!(
            outcome.infrastructure.cleanup_error.is_none(),
            "target cleanup failed after control loss: {}",
            outcome
                .infrastructure
                .cleanup_error
                .as_deref()
                .unwrap_or_default()
        );
    }
    loop_result
}

#[cfg(unix)]
type ActiveSupervisor = Supervisor;
#[cfg(windows)]
type ActiveSupervisor = ();

#[cfg(unix)]
fn control_channel(args: &Args) -> std::io::Result<tokio::fs::File> {
    use std::os::fd::FromRawFd;

    let file = unsafe { std::fs::File::from_raw_fd(args.control_fd) };
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(windows)]
fn control_channel(args: &Args) -> std::io::Result<tokio::fs::File> {
    use std::os::windows::io::FromRawHandle;

    let handle_value = usize::try_from(args.control_handle).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private control handle exceeds the native handle width",
        )
    })?;
    let file = unsafe { std::fs::File::from_raw_handle(handle_value as *mut std::ffi::c_void) };
    Ok(tokio::fs::File::from_std(file))
}
