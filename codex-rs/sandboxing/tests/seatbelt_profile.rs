#![cfg(target_os = "macos")]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use codex_sandboxing::seatbelt_profile::CreateSeatbeltCommandPrefixParams;
use codex_sandboxing::seatbelt_profile::MACOS_SANDBOX_EXEC_PATH;
use codex_sandboxing::seatbelt_profile::create_seatbelt_command_prefix;

#[test]
fn builds_a_closed_network_profile_with_lossless_path_parameters() {
    let writable_path = Path::new(OsStr::from_bytes(b"/tmp/private-\xFE"));
    let writable_roots = [writable_path];
    let arguments = create_seatbelt_command_prefix(CreateSeatbeltCommandPrefixParams {
        allow_read_all: true,
        writable_roots: &writable_roots,
        allow_network: false,
        extra_policy_sections: &["(deny file-read* (literal \"/dev/tty\"))"],
    });

    assert_eq!(MACOS_SANDBOX_EXEC_PATH, "/usr/bin/sandbox-exec");
    assert_eq!(
        arguments.first().map(std::ffi::OsString::as_os_str),
        Some(OsStr::new("-p"))
    );
    let policy = arguments[1].to_string_lossy();
    assert!(policy.contains("(deny default)"));
    assert!(policy.contains("(allow file-read*)"));
    assert!(policy.contains("(allow file-write* (subpath (param \"WRITABLE_ROOT_0\")))"));
    assert!(policy.contains("(deny file-read* (literal \"/dev/tty\"))"));
    assert!(!policy.contains("(allow network-outbound)"));
    assert_eq!(
        arguments[2].as_os_str().as_bytes(),
        b"-DWRITABLE_ROOT_0=/tmp/private-\xFE"
    );
    assert_eq!(
        arguments.last().map(std::ffi::OsString::as_os_str),
        Some(OsStr::new("--"))
    );
}
