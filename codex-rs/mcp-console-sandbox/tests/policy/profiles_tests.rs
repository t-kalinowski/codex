use super::*;
use pretty_assertions::assert_eq;

fn selected(workspace: &Path, name: &str, command: &[&str]) -> Value {
    let mut value = request(command);
    let fields = value.as_object_mut().unwrap();
    fields.remove("filesystem");
    fields.remove("network");
    value["extends"] = json!(name);
    value["workspace"] = json!(workspace.canonicalize().unwrap());
    value["cwd"] = value["workspace"].clone();
    value
}

#[test]
fn native_workspace_selector_enforces_metadata_through_both_transports() {
    for descriptor in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        for name in [".git", ".agents", ".codex"] {
            std::fs::create_dir(workspace.path().join(name)).unwrap();
            std::fs::write(workspace.path().join(name).join("keep"), "readable").unwrap();
        }
        let script = r#"
set -eu
denied() { if "$@" 2>/dev/null; then echo "unexpected success: $*"; exit 1; fi; }
printf old > ordinary
printf new >> ordinary
test "$(cat ordinary)" = oldnew
rm ordinary
for name in .git .agents .codex; do
    test "$(cat "$name/keep")" = readable
    denied /bin/sh -c 'echo changed > "$1/keep"' sh "$name"
    denied /bin/sh -c 'echo created > "$1/new"' sh "$name"
    denied rm "$name/keep"
    denied mv "$name" "$name-moved"
    mkdir replacement
    denied mv replacement "$name/replacement"
    rmdir replacement
    denied rm -rf "$name"
done
printf 'native workspace\n'
"#;
        let mut value = selected(workspace.path(), ":workspace", &["/bin/sh", "-c", script]);
        value["workspace_options"] =
            json!({"exclude_tmpdir_env_var":true,"exclude_slash_tmp":true});
        // A broader write must not swallow the materialized metadata entries.
        value["filesystem"] = json!({"kind":"restricted","entries":[{
            "path":{"type":"path","path":workspace.path().parent().unwrap().canonicalize().unwrap()},"access":"write"
        }]});
        let output = if descriptor {
            run(frame(&value), &[])
        } else {
            for field in ["command", "cwd", "environment"] {
                value.as_object_mut().unwrap().remove(field);
            }
            let staging = tempfile::tempdir().unwrap();
            runner(staging.path())
                .current_dir(workspace.path())
                .args(["--config-env", "POLICY", "--", "/bin/sh", "-c", script])
                .env("POLICY", value.to_string())
                .output()
                .unwrap()
        };
        assert_eq!(
            (output.status.code(), output.stdout, output.stderr),
            (Some(0), b"native workspace\n".to_vec(), vec![])
        );
    }
}

#[test]
fn native_selection_preserves_network_and_complete_policy_contracts() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let connect = fixture("connect", &[&address])["command"].clone();
    for name in [":workspace", ":read-only"] {
        let mut value = selected(workspace.path(), name, &["/bin/echo", "ready"]);
        value["command"] = connect.clone();
        assert!(!run(frame(&value), &[]).status.success());
        value["network"] = json!("enabled");
        let output = run(frame(&value), &[]);
        assert!(output.status.success(), "{output:?}");
        for kind in ["unrestricted", "external-sandbox"] {
            let path = outside.path().join("written");
            value["filesystem"] = json!({"kind":kind});
            value["command"] = fixture("write", &[path.to_str().unwrap()])["command"].clone();
            let output = run(frame(&value), &[]);
            assert!(output.status.success(), "{name}/{kind}: {output:?}");
            assert_eq!(std::fs::read(&path).unwrap(), b"created");
            std::fs::remove_file(path).unwrap();
        }
    }
    let path = workspace.path().join("written");
    let command = fixture("probe-write", &[path.to_str().unwrap()])["command"].clone();
    let mut raw = request(&[]);
    raw["command"] = command.clone();
    let mut read_only = selected(workspace.path(), ":read-only", &[]);
    read_only["command"] = command;
    for value in [raw, read_only] {
        let output = run(frame(&value), &[]);
        assert_eq!(
            (output.status.code(), output.stdout, output.stderr),
            (Some(0), b"write denied\n".to_vec(), vec![])
        );
    }
}

#[test]
fn native_selection_reports_unsupported_names_and_requires_raw_policies() {
    let workspace = tempfile::tempdir().unwrap();
    let value = selected(workspace.path(), ":unknown", &["/bin/echo", "must not run"]);
    let output = run(frame(&value), &[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "unsupported built-in profile \":unknown\"; expected \":workspace\" or \":read-only\""
        ),
        "{output:?}"
    );
    for field in ["filesystem", "network"] {
        let mut value = request(&["/bin/echo", "must not run"]);
        value.as_object_mut().unwrap().remove(field);
        let output = run(frame(&value), &[]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn workspace_options_keep_native_temp_defaults_and_private_storage() {
    let workspace = tempfile::tempdir().unwrap();
    let inherited = tempfile::tempdir().unwrap();
    let shared = tempfile::Builder::new()
        .prefix("profile-shared-")
        .tempdir_in("/tmp")
        .unwrap();
    for excluded in [false, true] {
        let mut value = selected(
            workspace.path(),
            ":workspace",
            &[
                "/bin/sh",
                "-c",
                r#"
set -eu
printf private > "$TMPDIR/private"
if test "$EXCLUDED" = true; then
    if touch "$INHERITED/new" 2>/dev/null; then exit 1; fi
    if touch "$SHARED/new" 2>/dev/null; then exit 1; fi
else
    touch "$INHERITED/new" "$SHARED/new"
fi
printf 'temp policy applied\n'
"#,
            ],
        );
        if excluded {
            value["workspace_options"] =
                json!({"exclude_tmpdir_env_var":true,"exclude_slash_tmp":true});
        }
        value["environment"] = json!({"INHERITED":inherited.path(),"SHARED":shared.path(),"EXCLUDED":excluded.to_string()});
        value["lifecycle"] = json!({"private_tmp":{"environment":["TMPDIR"]}});
        let staging = tempfile::tempdir().unwrap();
        let mut command = runner(staging.path());
        command.env("TMPDIR", inherited.path());
        let output = run_command(command, frame(&value), &[]);
        assert_eq!(
            (output.status.code(), output.stdout, output.stderr),
            (Some(0), b"temp policy applied\n".to_vec(), vec![])
        );
    }
}

#[test]
fn native_profile_composition_preserves_specificity_and_equal_path_precedence() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir(root.join(".git/allowed")).unwrap();
    for path in ["ordinary", ".git/keep", ".git/secret"] {
        std::fs::write(root.join(path), "readable").unwrap();
    }
    let entry = |path: &Path, access| json!({"path":{"type":"path","path":path},"access":access});
    let mut entries = vec![
        entry(&root.join(".git/secret"), "deny"),
        entry(&root.join(".git/allowed"), "write"),
    ];
    for reverse in [false, true] {
        if reverse {
            entries.reverse();
        }
        let mut value = selected(
            &root,
            ":workspace",
            &[
                "/bin/sh",
                "-c",
                r#"
set -eu
test "$(cat .git/keep)" = readable
test "$(cat ordinary)" = readable
if cat .git/secret 2>/dev/null; then exit 1; fi
printf allowed > .git/allowed/result
if touch .git/blocked 2>/dev/null; then exit 1; fi
"#,
            ],
        );
        value["filesystem"] = json!({"kind":"restricted","entries":entries});
        let output = run(frame(&value), &[]);
        assert!(output.status.success(), "{output:?}");
    }
    for access in ["write", "deny"] {
        let mut value = selected(&root, ":workspace", &[]);
        value["command"] =
            fixture("probe-write", &[root.join(".git/keep").to_str().unwrap()])["command"].clone();
        value["filesystem"] = json!({"kind":"restricted","entries":[entry(&root.join(".git"), "write"), entry(&root.join(".git"), access)]});
        let output = run(frame(&value), &[]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            if access == "write" {
                "write allowed\n"
            } else {
                "write denied\n"
            }
        );
    }
}

#[test]
fn materialized_workspace_stays_fixed_and_protects_missing_metadata() {
    let workspace = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let mut value = selected(
        &root,
        ":workspace",
        &[
            "/bin/sh",
            "-c",
            r#"
set -eu
printf created > "$WORKSPACE/ordinary"
if touch elsewhere 2>/dev/null; then exit 1; fi
for name in .git .agents .codex; do
    if mkdir "$WORKSPACE/$name" 2>/dev/null; then exit 1; fi
done
"#,
        ],
    );
    value["cwd"] = json!(elsewhere.path());
    value["environment"]["WORKSPACE"] = json!(root);
    value["workspace_options"] = json!({"exclude_tmpdir_env_var":true,"exclude_slash_tmp":true});
    let output = run(frame(&value), &[]);
    assert!(output.status.success(), "{output:?}");
    for name in [".git", ".agents", ".codex"] {
        assert!(!root.join(name).exists(), "native setup retained {name}");
    }
}

#[test]
fn workspace_profile_keeps_native_git_indirection_and_symlink_protection() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("metadata")).unwrap();
    std::fs::write(root.join("metadata/keep"), "readable").unwrap();
    std::fs::write(root.join(".git"), "gitdir: metadata\n").unwrap();
    std::os::unix::fs::symlink(root.join("metadata"), root.join("alias")).unwrap();
    let value = selected(
        &root,
        ":workspace",
        &[
            "/bin/sh",
            "-c",
            r#"
set -eu
test "$(cat alias/keep)" = readable
if touch metadata/keep 2>/dev/null; then echo "allowed: touch metadata/keep"; exit 1; fi
if touch alias/keep 2>/dev/null; then echo "allowed: touch alias/keep"; exit 1; fi
if rm .git 2>/dev/null; then echo "allowed: rm .git"; exit 1; fi
if mv metadata relocated 2>/dev/null; then echo "allowed: mv metadata relocated"; exit 1; fi
"#,
        ],
    );
    let output = run(frame(&value), &[]);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn read_grants_beneath_denials_retain_native_backend_behavior_for_raw_and_selected_policy() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/keep"), "readable").unwrap();
    std::fs::write(root.join(".git/secret"), "secret").unwrap();
    for builtin in [false, true] {
        for nested_deny in [false, true] {
            let mut value = selected(&root, ":workspace", &["/bin/cat", ".git/keep"]);
            value["filesystem"] = json!({"kind":"restricted","entries":[
                {"path":{"type":"special","value":{"kind":"root"}},"access":"read"},
                {"path":{"type":"path","path":root},"access":"write"},
                {"path":{"type":"path","path":root},"access":"deny"},
                {"path":{"type":"path","path":root.join(".git")},"access":"read"}
            ]});
            if nested_deny {
                value["filesystem"]["entries"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({
                        "path":{"type":"path","path":root.join(".git/secret")},"access":"deny"
                    }));
            }
            if !builtin {
                value.as_object_mut().unwrap().remove("extends");
                value.as_object_mut().unwrap().remove("workspace");
                value["network"] = json!("restricted");
            }
            let output = run(frame(&value), &[]);
            #[cfg(target_os = "macos")]
            assert_eq!(
                (output.status.code(), output.stdout, output.stderr),
                (Some(0), b"readable".to_vec(), vec![])
            );
            #[cfg(target_os = "linux")]
            {
                // Native mount masks do not reopen descendant read grants.
                // A further deny tries to mount inside that hidden, read-only tree.
                assert_eq!(output.status.code(), Some(1), "{output:?}");
                assert!(output.stdout.is_empty(), "{output:?}");
                let stderr = String::from_utf8(output.stderr).unwrap();
                if nested_deny {
                    assert!(
                        stderr.starts_with("bwrap: Can't ")
                            && stderr.contains("/.git/secret: Read-only file system"),
                        "{stderr}"
                    );
                } else {
                    assert_eq!(stderr, "/bin/cat: .git/keep: No such file or directory\n");
                }
            }
        }
    }
}
