use anyhow::Context;
use anyhow::Result;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn run(runner: String) -> Result<()> {
    let mut request: serde_json::Value =
        serde_json::from_str(&std::env::var("SANDBOX_TEST_REQUEST")?)?;
    request["lifecycle"]["parent_pid"] = serde_json::json!(std::process::id());
    let (reader, mut writer) = std::io::pipe()?;
    let fd = reader.as_raw_fd();
    let mut command = Command::new(runner);
    command.args(["--bootstrap-fd", &fd.to_string()]);
    if let Some(library) = std::env::var_os("SANDBOX_TEST_RUNNER_PRELOAD") {
        #[cfg(target_os = "macos")]
        command.env("DYLD_INSERT_LIBRARIES", library);
        #[cfg(target_os = "linux")]
        command.env("LD_PRELOAD", library);
    }
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("start owned runner")?;
    drop(command);
    drop(reader);
    println!("{}", child.id());
    std::io::stdout().flush()?;
    let bytes = serde_json::to_vec(&request)?;
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    drop(writer);
    unsafe {
        drop(std::fs::File::from_raw_fd(0));
    }
    let status = child.wait()?;
    anyhow::ensure!(status.success(), "owned runner exited: {status}");
    Ok(())
}
