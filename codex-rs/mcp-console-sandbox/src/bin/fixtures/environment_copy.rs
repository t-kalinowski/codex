use anyhow::Context;
use anyhow::Result;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::process::Stdio;

pub fn run(runner: String, library: String, forbidden: String) -> Result<()> {
    let (mut events, event_writer) = std::io::pipe()?;
    let (release, mut release_writer) = std::io::pipe()?;
    let mut command = Command::new(runner);
    command
        .args(["--config-env", "SANDBOX_TEST_CONFIG", "--"])
        .arg(std::env::current_exe()?)
        .args(["probe-write", &forbidden])
        .env("SANDBOX_TEST_BEFORE_PARSE", "1")
        .env(
            "SANDBOX_TEST_EVENT_FD",
            event_writer.as_raw_fd().to_string(),
        )
        .env("SANDBOX_TEST_RELEASE_FD", release.as_raw_fd().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "macos")]
    command.env("DYLD_INSERT_LIBRARIES", library);
    #[cfg(target_os = "linux")]
    command.env("LD_PRELOAD", library);
    let descriptors = [event_writer.as_raw_fd(), release.as_raw_fd()];
    unsafe {
        command.pre_exec(move || {
            for fd in descriptors {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop((command, event_writer, release));
    let mut event = [0; 4];
    events.read_exact(&mut event)?;
    assert_eq!(i32::from_ne_bytes(event), child.id() as i32);
    let mut value: serde_json::Value =
        serde_json::from_str(&std::env::var("SANDBOX_TEST_CONFIG")?)?;
    let directory = std::path::Path::new(&forbidden)
        .parent()
        .context("fixture write probe must have a parent directory")?;
    value["filesystem"]["entries"]
        .as_array_mut()
        .context("fixture policy must contain filesystem entries")?
        .push(serde_json::json!({
            "path": {"type": "path", "path": directory},
            "access": "write"
        }));
    value["network"] = serde_json::json!("enabled");
    // This purpose-built owner fixture is single-threaded. The runner has
    // successfully execed and is blocked in its constructor, before parsing.
    unsafe {
        std::env::set_var("SANDBOX_TEST_CONFIG", value.to_string());
    }
    release_writer.write_all(b"x")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout, b"write denied\n",
        "parent mutation changed copied policy"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!std::path::Path::new(&forbidden).exists());
    println!("launch environment remained fixed before parsing");
    Ok(())
}
