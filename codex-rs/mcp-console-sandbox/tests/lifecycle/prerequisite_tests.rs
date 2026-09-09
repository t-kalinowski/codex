use super::*;

#[test]
fn unsuitable_inherited_procfs_is_rejected_without_panic_or_target_execution() {
    let directory = tempfile::tempdir().unwrap();
    let native_runner = runner(directory.path());
    let library = directory.path().join("procfs.so");
    let source =
        codex_utils_cargo_bin::find_resource!("tests/lifecycle/procfs_interposer.c").unwrap();
    let compiler = Command::new("cc")
        .args(["-shared", "-fPIC"])
        .arg(source)
        .args(["-o"])
        .arg(&library)
        .arg("-ldl")
        .output()
        .unwrap();
    assert!(compiler.status.success(), "{compiler:?}");
    let mut config = request(&["/bin/echo", "must not run"]);
    for key in ["command", "cwd", "environment"] {
        config.as_object_mut().unwrap().remove(key);
    }
    // Supply a mismatched procfs identity at the real native execution hook.
    // This exercises the rejection even where host AppArmor blocks the nested
    // namespaces needed to reproduce an unsuitable inherited procfs directly.
    let output = Command::new(native_runner.get_program())
        .args([
            "--config-env",
            "SANDBOX_TEST_CONFIG",
            "--",
            "/bin/echo",
            "must not run",
        ])
        .env("SANDBOX_TEST_CONFIG", config.to_string())
        .env("RUST_BACKTRACE", "1")
        .env("LD_PRELOAD", library)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(stderr.contains("namespace-local procfs"), "{output:?}");
    assert!(
        !stderr.contains("panicked") && !stderr.contains("stack backtrace"),
        "{output:?}"
    );
}
