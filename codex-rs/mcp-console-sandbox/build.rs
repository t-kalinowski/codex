use std::process::Command;

const BAZEL_BUILD_ENV: &str = "MCP_CONSOLE_SANDBOX_BAZEL_BUILD";
const BAZEL_REVISION_PLACEHOLDER: &str = "0000000000000000000000000000000000000000";

fn main() {
    println!("cargo:rerun-if-env-changed=STABLE_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed={BAZEL_BUILD_ENV}");
    emit_git_rerun_paths();
    let revision = std::env::var("STABLE_GIT_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(git_revision)
        .or_else(|| {
            (std::env::var(BAZEL_BUILD_ENV).as_deref() == Ok("1"))
                .then(|| BAZEL_REVISION_PLACEHOLDER.to_string())
        })
        .unwrap_or_else(|| panic!("set STABLE_GIT_COMMIT when building outside a Git checkout"));
    assert!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Codex source revision must be a full 40-character Git SHA"
    );
    println!("cargo:rustc-env=MCP_CONSOLE_SANDBOX_SOURCE_REVISION={revision}");
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
