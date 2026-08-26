use codex_sandbox_api::SandboxError;
use codex_sandbox_api::terminate_current_process_group_members;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const READY_DEADLINE: Duration = Duration::from_secs(10);

pub(super) fn supervised_tree(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let marker = super::next_path(args, "marker path")?;
    let behavior = super::next_string(args, "root behavior")?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    Command::new(executable)
        .arg("supervised-tree-child")
        .arg(&marker)
        .arg(std::process::id().to_string())
        .spawn()
        .map_err(|error| error.to_string())?;
    wait_for_marker(&marker)?;
    match behavior.as_str() {
        "exit" => Ok(()),
        "wait" => wait_forever(),
        _ => Err(format!("unknown supervised root behavior `{behavior}`")),
    }
}

pub(super) fn tree_child(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let marker = super::next_path(args, "marker path")?;
    let root_pid = super::next_string(args, "root pid")?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let grandchild = Command::new(executable)
        .arg("supervised-tree-grandchild")
        .spawn()
        .map_err(|error| error.to_string())?;
    let contents = format!("{root_pid} {} {}\n", std::process::id(), grandchild.id());
    let temporary_marker = marker.with_extension("tmp");
    fs::write(&temporary_marker, contents).map_err(|error| error.to_string())?;
    fs::rename(temporary_marker, marker).map_err(|error| error.to_string())?;
    wait_forever()
}

pub(super) fn tree_grandchild() -> Result<(), String> {
    wait_forever()
}

pub(super) fn except_self(args: &mut impl Iterator<Item = OsString>) -> Result<(), String> {
    let marker = super::next_path(args, "marker path")?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let mut child = Command::new(executable)
        .arg("supervised-tree-child")
        .arg(&marker)
        .arg(std::process::id().to_string())
        .spawn()
        .map_err(|error| error.to_string())?;
    wait_for_marker(&marker)?;
    terminate_current_process_group_members().map_err(|error| error.to_string())?;
    let status = child.wait().map_err(|error| error.to_string())?;
    if status.success() {
        return Err("except-self operation did not terminate the direct child".to_string());
    }
    println!("except-self-caller-alive");
    Ok(())
}

pub(super) fn except_self_refused() -> Result<(), String> {
    match terminate_current_process_group_members() {
        Err(SandboxError::InvalidOperation { .. }) => Ok(()),
        Err(error) => Err(format!("unexpected except-self error: {error}")),
        Ok(()) => Err("except-self operation accepted an inherited process group".to_string()),
    }
}

fn wait_for_marker(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + READY_DEADLINE;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {}", path.display()));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn wait_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
