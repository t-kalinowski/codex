use std::ffi::OsString;
use std::path::Path;

pub const MACOS_SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

const MACOS_SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");

#[derive(Debug)]
pub struct CreateSeatbeltCommandPrefixParams<'a> {
    pub allow_read_all: bool,
    pub writable_roots: &'a [&'a Path],
    pub allow_network: bool,
    pub extra_policy_sections: &'a [&'a str],
}

/// Build the `sandbox-exec` arguments that precede a caller-owned command.
///
/// Paths are passed through `-D` parameters as `OsString` values rather than
/// being interpolated into the Seatbelt source. The returned prefix ends in
/// `--`; callers append their command and arguments without string conversion.
pub fn create_seatbelt_command_prefix(
    params: CreateSeatbeltCommandPrefixParams<'_>,
) -> Vec<OsString> {
    let mut policy_sections = vec![MACOS_SEATBELT_BASE_POLICY];
    if params.allow_read_all {
        policy_sections.push("(allow file-read*)");
    }

    let writable_policy = params
        .writable_roots
        .iter()
        .enumerate()
        .map(|(index, _)| {
            format!("(allow file-write* (subpath (param \"WRITABLE_ROOT_{index}\")))")
        })
        .collect::<Vec<_>>()
        .join("\n");
    policy_sections.push(&writable_policy);

    if params.allow_network {
        policy_sections.push("(allow network-outbound)\n(allow network-inbound)");
    }
    policy_sections.extend(params.extra_policy_sections.iter().copied());

    let mut arguments = vec![
        OsString::from("-p"),
        OsString::from(policy_sections.join("\n")),
    ];
    for (index, path) in params.writable_roots.iter().enumerate() {
        let mut definition = OsString::from(format!("-DWRITABLE_ROOT_{index}="));
        definition.push(path);
        arguments.push(definition);
    }
    arguments.push(OsString::from("--"));
    arguments
}
