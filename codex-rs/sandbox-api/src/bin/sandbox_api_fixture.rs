use codex_sandbox_api::CommandSpec;
use codex_sandbox_api::PathAccess;
use codex_sandbox_api::PathRule;
use codex_sandbox_api::SandboxError;
use codex_sandbox_api::SandboxFeature;
use codex_sandbox_api::SandboxPolicy;
use codex_sandbox_api::SandboxRequest;
use codex_sandbox_api::SandboxRuntime;
use codex_sandbox_api::SandboxRuntimeConfig;
use codex_sandbox_api::SandboxedOutput;
use codex_sandbox_api::dispatch_embedded_helper;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

fn main() -> ExitCode {
    // This must remain before runtime or thread creation. Linux re-enters this
    // binary through the reserved helper alias.
    dispatch_embedded_helper();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    let mode = next_string(&mut args, "mode")?;
    match mode.as_str() {
        "contract" => contract(&mut args),
        "current-executable" => current_executable(&mut args),
        "current-executable-state-write-rejected" => {
            current_executable_state_write_rejected(&mut args)
        }
        "exit" => exit_with(&mut args),
        #[cfg(unix)]
        "fd-unavailable" => fd_unavailable(&mut args),
        "fs-read" => fs_read(&mut args),
        "fs-read-denied" => fs_read_denied(&mut args),
        "fs-write" => fs_write(&mut args),
        "fs-write-denied" => fs_write_denied(&mut args),
        "large-output" => large_output(),
        "marker" => marker(&mut args),
        "network-connect" => network_connect(&mut args),
        "network-denied" => network_denied(&mut args),
        "wait" => wait_forever(),
        _ => Err(format!("unknown fixture mode `{mode}`")),
    }
}

fn next_os(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<OsString, String> {
    args.next().ok_or_else(|| format!("missing {name}"))
}

fn next_string(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<String, String> {
    next_os(args, name)?
        .into_string()
        .map_err(|_| format!("{name} is not UTF-8"))
}

fn next_path(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(next_os(args, name)?))
}

fn contract(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let expected_cwd = next_path(args, "expected cwd")?;
    let omitted_env = next_os(args, "omitted environment name")?;
    let remaining = args.collect::<Vec<_>>();
    if remaining != [OsString::from("--stdio"), OsString::from("-v")] {
        return Err(format!("arguments changed: {remaining:?}"));
    }
    let actual_cwd = env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|error| error.to_string())?;
    let expected_cwd = fs::canonicalize(expected_cwd).map_err(|error| error.to_string())?;
    if actual_cwd != expected_cwd {
        return Err(format!(
            "cwd changed: expected {}, got {}",
            expected_cwd.display(),
            actual_cwd.display()
        ));
    }
    if env::var_os("SANDBOX_API_EXACT_ENV") != Some(OsString::from("present")) {
        return Err("expected environment value is absent".to_string());
    }
    if env::var_os(&omitted_env).is_some() {
        return Err(format!(
            "omitted environment value {omitted_env:?} was inherited"
        ));
    }

    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| error.to_string())?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(b"stdin\0")
        .and_then(|()| stdout.write_all(&input))
        .and_then(|()| stdout.write_all(&[0x80, 0xff]))
        .map_err(|error| error.to_string())?;
    std::io::stderr()
        .lock()
        .write_all(b"stderr\0\x80\xff")
        .map_err(|error| error.to_string())
}

fn current_executable(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let state_dir = next_path(args, "state directory")?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let target_env = env::vars_os()
        .filter(|(key, _)| {
            !cfg!(target_os = "linux") || !key.as_encoded_bytes().starts_with(b"LD_")
        })
        .collect::<BTreeMap<_, _>>();
    let command = CommandSpec::new(executable, state_dir.clone(), target_env).args(["exit", "0"]);
    let request = SandboxRequest::new(
        command,
        SandboxPolicy::host_read_only().network_unrestricted(),
    );
    let runtime = SandboxRuntime::new(SandboxRuntimeConfig::new(state_dir))
        .map_err(|error| error.to_string())?;
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    async_runtime.block_on(async move {
        let mut child = runtime
            .spawn(request)
            .await
            .map_err(|error| error.to_string())?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| "missing stdout".to_string())?;
        let stderr = child
            .take_stderr()
            .ok_or_else(|| "missing stderr".to_string())?;
        let stdout_task = tokio::spawn(drain(stdout));
        let stderr_task = tokio::spawn(drain(stderr));
        let status = child.wait().await.map_err(|error| error.to_string())?;
        stdout_task.await.map_err(|error| error.to_string())??;
        stderr_task.await.map_err(|error| error.to_string())??;
        if !status.success() {
            return Err(format!("nested child failed: {status:?}"));
        }
        println!("current-executable-ok");
        Ok(())
    })
}

fn current_executable_state_write_rejected(
    args: &mut impl Iterator<Item = OsString>,
) -> Result<(), String> {
    let state_dir = next_path(args, "state directory")?;
    let runtime = SandboxRuntime::new(SandboxRuntimeConfig::new(&state_dir))
        .map_err(|error| error.to_string())?;
    let helper_dir = fs::read_dir(&state_dir)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b".sandbox-helper-"))
        })
        .ok_or_else(|| "private helper directory was not created".to_string())?;
    let registry_root = helper_dir.join("synthetic-mount-registry");
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let target_env = env::vars_os()
        .filter(|(key, _)| !key.as_encoded_bytes().starts_with(b"LD_"))
        .collect::<BTreeMap<_, _>>();
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    for access in [PathAccess::Write, PathAccess::Deny] {
        let nested = registry_root.join(format!("nested-{access:?}"));
        fs::create_dir(&nested).map_err(|error| error.to_string())?;
        let marker = state_dir.join(format!("target-ran-{access:?}"));
        let request = SandboxRequest::new(
            CommandSpec::new(executable.clone(), &state_dir, target_env.clone())
                .arg("marker")
                .arg(&marker),
            SandboxPolicy::host_read_only()
                .read_write(&state_dir)
                .read_only(&helper_dir)
                .rule(PathRule::new(&nested, access))
                .network_unrestricted(),
        );
        let error = match async_runtime.block_on(runtime.spawn(request)) {
            Ok(mut child) => {
                async_runtime
                    .block_on(child.wait())
                    .map_err(|error| error.to_string())?;
                return Err("sandbox unexpectedly spawned the target".to_string());
            }
            Err(error) => error,
        };
        let expected_feature = match access {
            PathAccess::Write => SandboxFeature::DeniedWritePaths,
            PathAccess::Deny => SandboxFeature::DeniedReadPaths,
            PathAccess::Read => unreachable!(),
        };
        if !matches!(
            error,
            SandboxError::UnsupportedPolicy { feature, .. } if feature == expected_feature
        ) {
            return Err(format!("unexpected sandbox error: {error:?}"));
        }
        if marker.exists() {
            return Err("target ran before policy rejection".to_string());
        }
    }
    println!("current-executable-state-write-rejected");
    Ok(())
}

async fn drain(mut output: SandboxedOutput) -> Result<(), String> {
    while output
        .read_chunk()
        .await
        .map_err(|error| error.to_string())?
        .is_some()
    {}
    Ok(())
}

fn exit_with(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let code = next_string(args, "exit code")?
        .parse::<i32>()
        .map_err(|error| error.to_string())?;
    std::process::exit(code);
}

#[cfg(unix)]
fn fd_unavailable(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let fd = next_string(args, "file descriptor")?
        .parse::<libc::c_int>()
        .map_err(|error| error.to_string())?;
    let expected = next_string(args, "inherited contents")?;
    let mut bytes = vec![0; expected.len()];
    let count = unsafe {
        libc::pread(
            fd,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            /*offset*/ 0,
        )
    };
    if count == expected.len() as isize && bytes == expected.as_bytes() {
        Err("inherited descriptor remained available inside the sandbox".to_string())
    } else {
        Ok(())
    }
}

fn fs_read(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let path = next_path(args, "read path")?;
    let expected = next_string(args, "expected contents")?;
    let actual = fs::read_to_string(path).map_err(|error| error.to_string())?;
    if actual != expected {
        return Err(format!("unexpected file contents: {actual:?}"));
    }
    Ok(())
}

fn fs_read_denied(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let path = next_path(args, "denied read path")?;
    match fs::read(path) {
        Ok(_) => Err("denied read succeeded".to_string()),
        Err(_) => Ok(()),
    }
}

fn fs_write(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let path = next_path(args, "write path")?;
    fs::write(path, b"written by sandbox fixture").map_err(|error| error.to_string())
}

fn fs_write_denied(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let path = next_path(args, "denied write path")?;
    match fs::write(path, b"sandbox escape") {
        Ok(()) => Err("denied write succeeded".to_string()),
        Err(_) => Ok(()),
    }
}

fn marker(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let path = next_path(args, "marker path")?;
    fs::write(path, b"target ran").map_err(|error| error.to_string())
}

fn large_output() -> Result<(), String> {
    const CHUNK_SIZE: usize = 32 * 1024;
    const CHUNKS: usize = 128;
    let stdout_thread = thread::spawn(|| -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        let chunk = vec![0xa5; CHUNK_SIZE];
        for _ in 0..CHUNKS {
            stdout.write_all(&chunk)?;
        }
        Ok(())
    });
    let stderr_thread = thread::spawn(|| -> std::io::Result<()> {
        let mut stderr = std::io::stderr().lock();
        let chunk = vec![0x5a; CHUNK_SIZE];
        for _ in 0..CHUNKS {
            stderr.write_all(&chunk)?;
        }
        Ok(())
    });
    stdout_thread
        .join()
        .map_err(|_| "stdout thread panicked".to_string())?
        .map_err(|error| error.to_string())?;
    stderr_thread
        .join()
        .map_err(|_| "stderr thread panicked".to_string())?
        .map_err(|error| error.to_string())
}

fn network_connect(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let address = next_string(args, "network address")?;
    let mut stream = TcpStream::connect(address).map_err(|error| error.to_string())?;
    stream
        .write_all(b"connected")
        .map_err(|error| error.to_string())
}

fn network_denied(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let address = next_string(args, "network address")?;
    match TcpStream::connect(address) {
        Ok(_) => Err("network connection unexpectedly succeeded".to_string()),
        Err(_) => Ok(()),
    }
}

fn wait_forever() -> Result<(), String> {
    std::io::stdout()
        .write_all(b"ready\n")
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
