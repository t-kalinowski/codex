use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=STABLE_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=CODEX_BWRAP_SHA256");
    require_linux_companion_digest();
    emit_git_rerun_paths();
    let revision = std::env::var("STABLE_GIT_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(git_revision)
        .unwrap_or_else(|| panic!("set STABLE_GIT_COMMIT when building outside a Git checkout"));
    assert!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Codex source revision must be a full 40-character Git SHA"
    );
    println!("cargo:rustc-env=MCP_CONSOLE_SANDBOX_SOURCE_REVISION={revision}");
}

fn require_linux_companion_digest() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let digest = std::env::var("CODEX_BWRAP_SHA256").unwrap_or_else(|_| {
        panic!(
            "build codex-bwrap first and set CODEX_BWRAP_SHA256 to the SHA-256 of the exact \
             companion bytes before building mcp-console-sandbox"
        )
    });
    assert!(
        digest.len() == 64
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && digest.bytes().any(|byte| byte != b'0'),
        "CODEX_BWRAP_SHA256 must contain a nonzero 64-digit hexadecimal digest"
    );
}

fn git_revision() -> Option<String> {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")?;
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?;
    let revision = revision.trim();
    (!revision.is_empty()).then(|| revision.to_string())
}

fn emit_git_rerun_paths() {
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    let git_path = |path: &str| {
        Command::new("git")
            .args(["rev-parse", "--git-path", path])
            .current_dir(&manifest_dir)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|path| path.trim().to_string())
    };
    if let Some(head) = git_path("HEAD") {
        println!("cargo:rerun-if-changed={head}");
    }
    let reference = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(&manifest_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok());
    if let Some(reference) = reference
        && let Some(reference_path) = git_path(reference.trim())
    {
        println!("cargo:rerun-if-changed={reference_path}");
    }
}
