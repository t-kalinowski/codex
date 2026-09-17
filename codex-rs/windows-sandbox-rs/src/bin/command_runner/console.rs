#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    codex_windows_sandbox::WindowsSandboxProduct::Console.initialize()?;
    win::main()
}

#[cfg(not(windows))]
fn main() {
    panic!("mcp-console-sandbox-runner is Windows-only");
}
