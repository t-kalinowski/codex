use super::*;
use pretty_assertions::assert_eq;

#[test]
fn documented_payloads_execute_through_both_production_transports() {
    let reference =
        std::fs::read_to_string(codex_utils_cargo_bin::find_resource!("PROTOCOL.md").unwrap())
            .unwrap();
    let mut count = 0;
    for block in reference.split("```json\n").skip(1) {
        let payload = block.split("```").next().unwrap();
        let value: Value = serde_json::from_str(payload).unwrap();
        if cfg!(target_os = "macos") && value.get("linux_backend").is_some() {
            continue;
        }
        let output = if value.get("command").is_some() {
            run(frame(&value), &[])
        } else {
            let directory = tempfile::tempdir().unwrap();
            runner(directory.path())
                .args([
                    "--config-env",
                    "SANDBOX_REQUEST",
                    "--",
                    "/bin/echo",
                    "sandbox ready",
                ])
                .env("SANDBOX_REQUEST", payload)
                .output()
                .unwrap()
        };
        assert_eq!(
            (output.status.code(), output.stdout, output.stderr),
            (Some(0), b"sandbox ready\n".to_vec(), vec![]),
            "{payload}"
        );
        count += 1;
    }
    assert!(count >= 5, "complete transport examples disappeared");
}

#[test]
fn upstream_tagged_paths_aliases_and_ignored_fields_reach_execution() {
    for path in [
        json!({"type":"path", "path":"/var/tmp"}),
        json!({"type":"special", "value":{"kind":"minimal"}}),
        json!({"type":"special", "value":{"kind":"project_roots", "subpath":null}}),
        json!({"type":"special", "value":{"kind":"current_working_directory", "subpath":"child"}}),
        json!({"type":"special", "value":{"kind":"tmpdir"}}),
        json!({"type":"special", "value":{"kind":"slash_tmp"}}),
        json!({"type":"special", "value":{"kind":"unknown", "path":":future", "subpath":"child", "future":true}}),
        json!({"type":"glob_pattern", "pattern":"/var/tmp/reference-absent-*/**/*.secret"}),
    ] {
        let accesses: &[&str] = if path["type"] == "special" && path["value"]["kind"] != "unknown" {
            // Denying cwd or /tmp would also hide the staged native executable.
            &["read", "write"]
        } else {
            &["read", "write", "deny", "none"]
        };
        for access in accesses {
            let workspace = tempfile::tempdir().unwrap();
            let mut value = request(&["/bin/echo", "validated"]);
            value["cwd"] = json!(workspace.path().canonicalize().unwrap());
            // Keep runtime files readable while exercising every nested variant.
            value["filesystem"]["entries"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "path":path, "access":access, "missing_path_behavior":"skip", "future":true
                }));
            value["filesystem"]["future"] = json!(true);
            let output = run(frame(&value), &[]);
            assert!(output.status.success(), "{value}: {output:?}");
            assert_eq!(output.stdout, b"validated\n");
        }
    }
}

#[test]
fn omission_and_null_follow_the_production_field_contract() {
    let mut value = request(&["/bin/echo", "validated"]);
    value["filesystem"]["glob_scan_max_depth"] = Value::Null;
    value["lifecycle"] = json!({"parent_pid":null,"cleanup_timeout_ms":null,"private_tmp":null});
    value["linux_backend"] = Value::Null;
    value["macos_seatbelt_profile_extension"] = Value::Null;
    value["filesystem"]["entries"][0]["missing_path_behavior"] = Value::Null;
    let output = run(frame(&value), &[]);
    assert!(output.status.success(), "{output:?}");
    for mode in ["limited", "full"] {
        value["proxy"] = proxy_config();
        value["proxy"]["mode"] = json!(mode);
        for key in ["domains", "unixSockets"] {
            value["proxy"].as_object_mut().unwrap().remove(key);
        }
        value["proxy"]["future"] = json!(true);
        let output = run(frame(&value), &[]);
        assert!(output.status.success(), "{output:?}");
    }
    for field in [
        "enabled",
        "enableSocks5",
        "enableSocks5Udp",
        "allowUpstreamProxy",
        "dangerouslyAllowAllUnixSockets",
        "mode",
        "allowLocalBinding",
    ] {
        for null in [false, true] {
            value["proxy"] = proxy_config();
            if null {
                value["proxy"][field] = Value::Null;
            } else {
                value["proxy"].as_object_mut().unwrap().remove(field);
            }
            let output = run(frame(&value), &[]);
            assert!(!output.status.success(), "{field}: {output:?}");
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn invalid_nested_configuration_is_rejected_before_target_execution() {
    for (field, invalid) in [
        (
            "filesystem",
            json!({"kind":"unrestricted","entries":[{"path":{"type":"path","path":"relative"},"access":"read"}]}),
        ),
        (
            "filesystem",
            json!({"kind":"unrestricted","entries":[{"path":{"type":"path","path":"file:///tmp"},"access":"read"}]}),
        ),
        ("filesystem", json!({"kind":"unrestricted","entries":null})),
        (
            "filesystem",
            json!({"kind":"restricted","glob_scan_max_depth":-1}),
        ),
        (
            "filesystem",
            json!({"kind":"unrestricted","entries":[{"path":{"type":"special","value":{"kind":"future"}},"access":"read"}]}),
        ),
        (
            "filesystem",
            json!({"kind":"unrestricted","entries":[{"path":{"type":"special","value":{"kind":"root"}},"access":"execute"}]}),
        ),
        ("network", json!("allow")),
        ("lifecycle", Value::Null),
        ("lifecycle", json!({"sigterm":null})),
        ("lifecycle", json!({"sigterm":"kill"})),
        ("lifecycle", json!({"parent_pid":1})),
        ("lifecycle", json!({"cleanup_timeout_ms":0})),
        ("lifecycle", json!({"cleanup_timeout_ms":60001})),
        ("lifecycle", json!({"future":true})),
        ("lifecycle", json!({"private_tmp":{"environment":null}})),
        (
            "lifecycle",
            json!({"private_tmp":{"environment":[],"parent":"relative"}}),
        ),
        (
            "lifecycle",
            json!({"private_tmp":{"environment":[],"future":true}}),
        ),
        ("linux_backend", json!("automatic")),
        ("environment", Value::Null),
    ] {
        let mut value = request(&["/bin/echo", "must not execute"]);
        value[field] = invalid;
        let output = run(frame(&value), &[]);
        assert!(!output.status.success(), "{value}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    for (field, invalid) in [
        ("enabled", json!(false)),
        ("domains", json!({"*":"deny"})),
        ("domains", json!({"example.com":null})),
        ("unixSockets", json!({"relative":"allow"})),
        ("unixSockets", json!({"/tmp/socket":"none"})),
        ("mode", json!("read-only")),
    ] {
        let mut value = request(&["/bin/echo", "must not execute"]);
        value["proxy"] = proxy_config();
        value["proxy"][field] = invalid;
        let output = run(frame(&value), &[]);
        assert!(!output.status.success(), "{value}: {output:?}");
        assert!(output.stdout.is_empty());
    }
    for (field, invalid) in [
        ("linux_backend", json!("landlock")),
        ("macos_seatbelt_profile_extension", json!("")),
    ] {
        let mut value = request(&["/bin/echo", "must not execute"]);
        value["filesystem"] = json!({"kind":"external-sandbox"});
        value[field] = invalid;
        let output = run(frame(&value), &[]);
        assert!(!output.status.success(), "{value}: {output:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn duplicate_struct_fields_fail_while_environment_map_keys_keep_the_last_value() {
    let base = serde_json::to_string(&fixture("context", &[])).unwrap();
    for (before, after) in [
        ("\"version\":2", "\"version\":2,\"version\":2"),
        (
            "\"kind\":\"restricted\"",
            "\"kind\":\"restricted\",\"kind\":\"restricted\"",
        ),
        (
            "\"access\":\"read\"",
            "\"access\":\"read\",\"access\":\"write\"",
        ),
        (
            "\"type\":\"special\"",
            "\"type\":\"special\",\"type\":\"special\"",
        ),
    ] {
        let payload = base.replace(before, after);
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend(payload.bytes());
        let output = run(bytes, &[]);
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
    }
    let payload = base.replace(
        "\"environment\":{}",
        "\"environment\":{\"DUPLICATE\":\"first\",\"DUPLICATE\":\"last\"}",
    );
    let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
    bytes.extend(payload.bytes());
    let output = run(bytes, &[]);
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["environment"]["DUPLICATE"], "last");
}
