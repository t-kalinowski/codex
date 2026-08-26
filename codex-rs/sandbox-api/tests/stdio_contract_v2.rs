mod support;

use codex_sandbox_api::SandboxRequest;
use codex_sandbox_api::SandboxStdioMode;
use pretty_assertions::assert_eq;
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::process::Command;
use support::TestResult;
use support::collect;
use support::command;
#[cfg(unix)]
use support::fixture;
use support::runtime;
use support::writable_policy;

#[tokio::test]
async fn exposes_handles_for_exactly_the_requested_pipe_modes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let modes = [
        SandboxStdioMode::Inherit,
        SandboxStdioMode::Pipe,
        SandboxStdioMode::Null,
    ];

    for stdin_mode in modes {
        for stdout_mode in modes {
            for stderr_mode in modes {
                let request = SandboxRequest::new(
                    command(temp.path(), "exit").arg("0"),
                    writable_policy(temp.path()),
                )
                .stdin(stdin_mode)
                .stdout(stdout_mode)
                .stderr(stderr_mode);
                let mut child = runtime.spawn(request).await?;
                let process = child.process();

                let stdin = child.take_stdin();
                let stdout = child.take_stdout();
                let stderr = child.take_stderr();
                assert_eq!(stdin.is_some(), stdin_mode == SandboxStdioMode::Pipe);
                assert_eq!(stdout.is_some(), stdout_mode == SandboxStdioMode::Pipe);
                assert_eq!(stderr.is_some(), stderr_mode == SandboxStdioMode::Pipe);

                if let Some(stdin) = stdin {
                    stdin.close().await?;
                }
                let stdout_task = stdout.map(|output| tokio::spawn(collect(output)));
                let stderr_task = stderr.map(|output| tokio::spawn(collect(output)));
                let status = process.wait_root().await?;
                assert!(
                    status.success(),
                    "stdio modes failed: {stdin_mode:?}/{stdout_mode:?}/{stderr_mode:?}"
                );
                if let Some(task) = stdout_task {
                    assert_eq!(task.await??, Vec::<u8>::new());
                }
                if let Some(task) = stderr_task {
                    assert_eq!(task.await??, Vec::<u8>::new());
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn null_streams_use_the_native_null_device() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let request = SandboxRequest::new(
        command(temp.path(), "stdio-null-device"),
        writable_policy(temp.path()),
    )
    .stdin(SandboxStdioMode::Null)
    .stdout(SandboxStdioMode::Null)
    .stderr(SandboxStdioMode::Null);
    let mut child = runtime.spawn(request).await?;
    let process = child.process();

    assert!(child.take_stdin().is_none());
    assert!(child.take_stdout().is_none());
    assert!(child.take_stderr().is_none());
    assert!(process.wait_root().await?.success());
    Ok(())
}

#[cfg(unix)]
#[test]
fn inherited_streams_remain_native_regular_file_descriptors() -> TestResult {
    let temp = tempfile::tempdir()?;
    let state_dir = temp.path().join("state");
    let input_path = temp.path().join("stdin");
    let stdout_path = temp.path().join("stdout");
    let stderr_path = temp.path().join("stderr");
    fs::write(&input_path, b"native-stdin\0\x82\xfd")?;

    let status = Command::new(fixture())
        .arg("stdio-inherit-driver")
        .arg(&state_dir)
        .arg(temp.path())
        .arg("default")
        .arg("stdio-inherited-regular")
        .stdin(File::open(&input_path)?)
        .stdout(File::create(&stdout_path)?)
        .stderr(File::create(&stderr_path)?)
        .status()?;

    assert!(
        status.success(),
        "inheritance driver failed; stderr={:?}",
        fs::read(&stderr_path)?
    );
    assert_eq!(fs::read(stdout_path)?, b"native-stdout\0\x80\xff");
    assert_eq!(fs::read(stderr_path)?, b"native-stderr\0\x81\xfe");
    Ok(())
}
