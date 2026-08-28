#[cfg(target_os = "windows")]
mod win;

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if let Some(response) =
        codex_windows_sandbox::windows_sandbox_standalone_helper_compatibility_response(
            codex_windows_sandbox::WindowsSandboxStandaloneHelperKind::Setup,
            &arguments,
        )
    {
        print!("{response}");
        return Ok(());
    }
    win::main()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    panic!("codex-windows-sandbox-setup is Windows-only");
}
