#![cfg(target_os = "macos")]

mod support;

use codex_sandbox_api::SandboxLifetime;
use codex_sandbox_api::SandboxPolicy;
use codex_sandbox_api::SandboxRequest;
use codex_sandbox_api::SandboxRuntime;
use codex_sandbox_api::SandboxedChild;
use pretty_assertions::assert_eq;
use std::ffi::CStr;
use std::fs;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use support::TestResult;
use support::collect;
use support::command;
use support::fixture;
use support::runtime;
use support::writable_policy;
use tokio::time::sleep;
use tokio::time::timeout;

const PROCESS_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TreePids {
    root: u32,
    child: u32,
    grandchild: u32,
}

#[tokio::test]
async fn terminates_and_retires_a_live_supervised_root() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let marker = temp.path().join("tree-ready");
    let mut child = launch_tree(&runtime, temp.path(), &marker, "wait").await?;
    let pids = read_tree_pids(&marker).await?;
    let process = child.process();
    let stdout = child.take_stdout().expect("stdout should be piped");
    let stderr = child.take_stderr().expect("stderr should be piped");
    let stdout_task = tokio::spawn(collect(stdout));
    let stderr_task = tokio::spawn(collect(stderr));

    assert_eq!(process.try_root_status()?, None);
    process.terminate()?;
    process.terminate()?;
    let status = process.retire().await?;

    assert_eq!(status.code(), None);
    assert!(status.signal().is_some());
    assert_eq!(process.retire().await?, status);
    assert_eq!(
        timeout(PROCESS_DEADLINE, stdout_task).await???,
        Vec::<u8>::new()
    );
    assert_eq!(
        timeout(PROCESS_DEADLINE, stderr_task).await???,
        Vec::<u8>::new()
    );
    wait_until_gone([pids.root, pids.child, pids.grandchild]).await?;
    Ok(())
}

#[tokio::test]
async fn observes_root_without_reaping_then_retires_descendants_once() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let marker = temp.path().join("tree-ready");
    let mut child = launch_tree(&runtime, temp.path(), &marker, "exit").await?;
    let pids = read_tree_pids(&marker).await?;
    let process = child.process();
    let stdout = child.take_stdout().expect("stdout should be piped");
    let stderr = child.take_stderr().expect("stderr should be piped");
    let stdout_task = tokio::spawn(collect(stdout));
    let stderr_task = tokio::spawn(collect(stderr));

    let try_deadline = Instant::now() + PROCESS_DEADLINE;
    let observed = loop {
        if let Some(status) = process.try_root_status()? {
            break status;
        }
        if Instant::now() >= try_deadline {
            return Err(io::Error::other("try_root_status did not observe the exited root").into());
        }
        sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(observed.code(), Some(0));
    let first_process = process.clone();
    let second_process = process.clone();
    let (first, second) = tokio::join!(first_process.wait_root(), second_process.wait_root());
    let first = first?;
    assert_eq!(second?, first);
    assert_eq!(first.code(), Some(0));
    assert_eq!(process.try_root_status()?, Some(first));
    assert_waitable_without_reaping(pids.root)?;
    assert!(pid_exists(pids.child));
    assert!(pid_exists(pids.grandchild));
    assert!(!stdout_task.is_finished());
    assert!(!stderr_task.is_finished());

    process.terminate()?;
    process.terminate()?;
    let first_retire = process.clone();
    let second_retire = process.clone();
    let (first_retired, second_retired) =
        tokio::join!(first_retire.retire(), second_retire.retire());
    assert_eq!(first_retired?, first);
    assert_eq!(second_retired?, first);
    assert_eq!(process.retire().await?, first);
    assert_eq!(process.wait_root().await?, first);
    assert_eq!(
        timeout(PROCESS_DEADLINE, stdout_task).await???,
        Vec::<u8>::new()
    );
    assert_eq!(
        timeout(PROCESS_DEADLINE, stderr_task).await???,
        Vec::<u8>::new()
    );
    wait_until_gone([pids.root, pids.child, pids.grandchild]).await?;
    assert_already_reaped(pids.root)?;
    Ok(())
}

#[tokio::test]
async fn dropping_the_last_supervisor_cleans_up_the_tree() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let marker = temp.path().join("tree-ready");
    let mut child = launch_tree(&runtime, temp.path(), &marker, "wait").await?;
    let pids = read_tree_pids(&marker).await?;
    let process = child.process();
    let stdin = child.take_stdin();
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();

    drop(stdin);
    drop(stdout);
    drop(stderr);
    drop(child);
    drop(process);

    wait_until_gone([pids.root, pids.child, pids.grandchild]).await?;
    Ok(())
}

#[tokio::test]
async fn in_sandbox_supervisor_stops_other_members_and_remains_alive() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let marker = temp.path().join("tree-ready");
    let request = SandboxRequest::new(
        command(temp.path(), "supervised-except-self").arg(&marker),
        writable_policy(temp.path()),
    )
    .lifetime(SandboxLifetime::SupervisedProcessTree);
    let mut child = runtime.spawn(request).await?;
    let process = child.process();
    let stdout_task = tokio::spawn(collect(
        child.take_stdout().expect("stdout should be piped"),
    ));
    let stderr_task = tokio::spawn(collect(
        child.take_stderr().expect("stderr should be piped"),
    ));
    let pids = read_tree_pids(&marker).await?;

    let status = process.wait_root().await?;
    let stdout = timeout(PROCESS_DEADLINE, stdout_task).await???;
    let stderr = timeout(PROCESS_DEADLINE, stderr_task).await???;
    assert!(status.success(), "except-self fixture failed: {stderr:?}");
    assert_eq!(stdout, b"except-self-caller-alive\n");
    assert_eq!(process.retire().await?, status);
    wait_until_gone([pids.root, pids.child, pids.grandchild]).await?;
    Ok(())
}

#[tokio::test]
async fn except_self_refuses_an_inherited_process_group() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let request = SandboxRequest::new(
        command(temp.path(), "supervised-except-self-refused"),
        writable_policy(temp.path()),
    );
    let mut child = runtime.spawn(request).await?;
    let process = child.process();
    let stdout_task = tokio::spawn(collect(
        child.take_stdout().expect("stdout should be piped"),
    ));
    let stderr_task = tokio::spawn(collect(
        child.take_stderr().expect("stderr should be piped"),
    ));

    let status = process.wait_root().await?;
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    assert!(status.success(), "refusal fixture failed: {stderr:?}");
    assert_eq!(stdout, Vec::<u8>::new());
    Ok(())
}

#[tokio::test]
async fn allows_cpu_count_and_boot_time_queries() -> TestResult {
    run_successful_fixture("macos-cpu-count", SandboxPolicy::host_read_only()).await?;
    run_successful_fixture("macos-boottime", SandboxPolicy::host_read_only()).await
}

#[tokio::test]
async fn allows_posix_semaphores() -> TestResult {
    run_successful_fixture("macos-posix-semaphore", SandboxPolicy::host_read_only()).await
}

#[tokio::test]
async fn allows_created_ptys_with_terminal_isolation() -> TestResult {
    run_successful_fixture(
        "macos-pty-created",
        SandboxPolicy::host_read_only().terminal_inherited_or_created(),
    )
    .await
}

#[tokio::test]
async fn terminal_isolation_denies_reopening_a_preexisting_pty() -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let (master, slave, terminal_path) = open_pty()?;
    let request = SandboxRequest::new(
        command(temp.path(), "macos-terminal-reopen-denied").arg(&terminal_path),
        writable_policy(temp.path()).terminal_inherited_or_created(),
    );
    let mut child = runtime.spawn(request).await?;
    let process = child.process();
    let stdout_task = tokio::spawn(collect(
        child.take_stdout().expect("stdout should be piped"),
    ));
    let stderr_task = tokio::spawn(collect(
        child.take_stderr().expect("stderr should be piped"),
    ));
    let status = process.wait_root().await?;
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    close_pty(master, slave);

    assert!(
        status.success(),
        "terminal reopen fixture failed: {stderr:?}"
    );
    assert_eq!(stdout, Vec::<u8>::new());
    Ok(())
}

#[tokio::test]
async fn terminal_isolation_denies_reopening_a_legacy_terminal_device() -> TestResult {
    let terminal_path = Path::new("/dev/ttysa");
    if !terminal_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("expected macOS terminal device {}", terminal_path.display()),
        )
        .into());
    }
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let request = SandboxRequest::new(
        command(temp.path(), "macos-terminal-reopen-denied").arg(terminal_path),
        writable_policy(temp.path()).terminal_inherited_or_created(),
    );
    let mut child = runtime.spawn(request).await?;
    let process = child.process();
    let stdout_task = tokio::spawn(collect(
        child.take_stdout().expect("stdout should be piped"),
    ));
    let stderr_task = tokio::spawn(collect(
        child.take_stderr().expect("stderr should be piped"),
    ));
    let status = process.wait_root().await?;
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;

    assert!(
        status.success(),
        "legacy terminal reopen fixture failed: {stderr:?}"
    );
    assert_eq!(stdout, Vec::<u8>::new());
    Ok(())
}

#[test]
fn terminal_isolation_preserves_inherited_ptys() -> TestResult {
    let temp = tempfile::tempdir()?;
    let state_dir = temp.path().join("state");
    let marker = temp.path().join("inherited-pty-marker");
    let (master, slave, _terminal_path) = open_pty()?;
    let stdin = duplicate_file(slave)?;
    let stdout = duplicate_file(slave)?;
    let stderr = duplicate_file(slave)?;

    let status = Command::new(fixture())
        .arg("stdio-inherit-driver")
        .arg(state_dir)
        .arg(temp.path())
        .arg("isolated")
        .arg("stdio-inherited-pty")
        .arg(&marker)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()?;
    close_pty(master, slave);

    assert!(status.success());
    assert_eq!(fs::read(marker)?, b"inherited-pty-ok");
    Ok(())
}

async fn launch_tree(
    runtime: &SandboxRuntime,
    cwd: &Path,
    marker: &Path,
    behavior: &str,
) -> TestResult<SandboxedChild> {
    let request = SandboxRequest::new(
        command(cwd, "supervised-tree").arg(marker).arg(behavior),
        writable_policy(cwd),
    )
    .lifetime(SandboxLifetime::SupervisedProcessTree);
    Ok(runtime.spawn(request).await?)
}

async fn read_tree_pids(path: &Path) -> TestResult<TreePids> {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            let pids = contents
                .split_whitespace()
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(pids.len(), 3);
            return Ok(TreePids {
                root: pids[0],
                child: pids[1],
                grandchild: pids[2],
            });
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other(format!("timed out waiting for {}", path.display())).into(),
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn assert_waitable_without_reaping(pid: u32) -> TestResult {
    let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    if unsafe { info.si_pid() } != pid as libc::pid_t {
        return Err(io::Error::other("root was not left waitable").into());
    }
    Ok(())
}

fn assert_already_reaped(pid: u32) -> TestResult {
    let mut status = 0;
    let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if result != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD) {
        return Err(io::Error::other(format!(
            "root wait status was available more than once: result={result}, status={status}"
        ))
        .into());
    }
    Ok(())
}

fn pid_exists(pid: u32) -> bool {
    let result = unsafe {
        libc::kill(pid as libc::pid_t, /*signal*/ 0)
    };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

async fn wait_until_gone<const N: usize>(pids: [u32; N]) -> TestResult {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    loop {
        let remaining = pids
            .iter()
            .copied()
            .filter(|pid| pid_exists(*pid))
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "processes remained after cleanup: {remaining:?}"
            ))
            .into());
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn run_successful_fixture(mode: &str, policy: SandboxPolicy) -> TestResult {
    let temp = tempfile::tempdir()?;
    let runtime = runtime()?;
    let request = SandboxRequest::new(command(temp.path(), mode), policy);
    let mut child = runtime.spawn(request).await?;
    let process = child.process();
    let stdout = child
        .take_stdout()
        .ok_or_else(|| io::Error::other("stdout should be piped"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| io::Error::other("stderr should be piped"))?;
    let stdout_task = tokio::spawn(collect(stdout));
    let stderr_task = tokio::spawn(collect(stderr));
    let status = process.wait_root().await?;
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    assert!(
        status.success(),
        "fixture {mode} failed; stdout={stdout:?}; stderr={stderr:?}"
    );
    Ok(())
}

fn open_pty() -> TestResult<(libc::c_int, libc::c_int, PathBuf)> {
    let mut master = -1;
    let mut slave = -1;
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let mut terminal_path = [0_i8; 1024];
    if unsafe { libc::ttyname_r(slave, terminal_path.as_mut_ptr(), terminal_path.len()) } != 0 {
        close_pty(master, slave);
        return Err(io::Error::last_os_error().into());
    }
    let terminal_path = unsafe { CStr::from_ptr(terminal_path.as_ptr()) }
        .to_str()?
        .into();
    Ok((master, slave, terminal_path))
}

fn duplicate_file(fd: libc::c_int) -> TestResult<File> {
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

fn close_pty(master: libc::c_int, slave: libc::c_int) {
    unsafe {
        libc::close(master);
        libc::close(slave);
    }
}
