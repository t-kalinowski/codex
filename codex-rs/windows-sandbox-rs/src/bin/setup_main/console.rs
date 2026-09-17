#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    codex_windows_sandbox::WindowsSandboxProduct::Console.initialize()?;
    win::main()
}

#[cfg(not(windows))]
fn main() {
    panic!("mcp-console-sandbox-setup is Windows-only");
}
