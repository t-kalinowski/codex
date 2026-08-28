use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::bundled_bwrap;
use crate::bundled_bwrap::BundledBwrapLauncher;
use crate::bundled_bwrap::ExecVerification;
use crate::exec_util::argv_to_cstrings;
use crate::exec_util::make_files_inheritable;
use codex_sandboxing::find_system_bwrap_in_path;
use codex_utils_absolute_path::AbsolutePathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
enum BubblewrapLauncher {
    System(SystemBwrapLauncher),
    Bundled(BundledBwrapLauncher),
    Embedding(BundledBwrapLauncher),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemBwrapLauncher {
    program: AbsolutePathBuf,
    supports_argv0: bool,
    supports_ro_bind_fd: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SystemBwrapCapabilities {
    supports_argv0: bool,
    supports_perms: bool,
    supports_ro_bind_fd: bool,
}

const CLOSE_MONITOR_STANDARD_STREAMS_ARG: &str = "--codex-close-monitor-standard-streams";

#[derive(Clone, Copy)]
enum MonitorStandardStreams {
    Preserve,
    Release,
}

pub(crate) fn exec_bwrap(mut argv: Vec<OsString>, preserved_files: Vec<File>) -> ! {
    let launcher = preferred_bwrap_launcher();
    let monitor_standard_streams = if crate::embedding::command_as_pid_1() {
        if !matches!(
            &launcher,
            BubblewrapLauncher::Bundled(_) | BubblewrapLauncher::Embedding(_)
        ) {
            panic!("PID 1 embedding requires packaged bubblewrap")
        }
        MonitorStandardStreams::Release
    } else {
        MonitorStandardStreams::Preserve
    };
    apply_process_options(&mut argv, monitor_standard_streams);

    match launcher {
        BubblewrapLauncher::System(launcher) => {
            if !launcher.supports_ro_bind_fd {
                translate_legacy_bwrap_fd_mounts(&mut argv)
                    .unwrap_or_else(|error| panic!("invalid legacy bubblewrap fd mount: {error}"));
            }
            exec_system_bwrap(&launcher.program, argv, preserved_files)
        }
        BubblewrapLauncher::Bundled(launcher) => {
            launcher.exec(ExecVerification::DigestOnly, argv, preserved_files)
        }
        BubblewrapLauncher::Embedding(launcher) => launcher.exec(
            ExecVerification::DigestAndPrivateCompatibility,
            argv,
            preserved_files,
        ),
        BubblewrapLauncher::Unavailable => {
            panic!(
                "bubblewrap is unavailable: no system bwrap was found on PATH and no bundled \
                 codex-resources/bwrap binary was found next to the Codex executable"
            )
        }
    }
}

fn apply_process_options(
    argv: &mut Vec<OsString>,
    monitor_standard_streams: MonitorStandardStreams,
) {
    argv.insert(1, OsString::from("--as-pid-1"));
    if matches!(monitor_standard_streams, MonitorStandardStreams::Release) {
        argv.insert(2, OsString::from(CLOSE_MONITOR_STANDARD_STREAMS_ARG));
    }
}

fn translate_legacy_bwrap_fd_mounts(argv: &mut Vec<OsString>) -> Result<(), String> {
    let command_separator = argv
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| "bubblewrap argv is missing the command separator '--'".to_string())?;
    let mut verification_args = Vec::new();
    let mut verified_fds = Vec::new();
    let mut argument_index = 0;

    while argument_index < command_separator {
        if argv[argument_index] != "--ro-bind-fd" {
            argument_index += 1;
            continue;
        }

        let fd_argument = argv
            .get(argument_index + 1)
            .filter(|_| argument_index + 1 < command_separator)
            .ok_or_else(|| "--ro-bind-fd is missing its file descriptor".to_string())?;
        let fd = fd_argument
            .to_str()
            .and_then(|argument| argument.parse::<libc::c_int>().ok())
            .ok_or_else(|| {
                format!(
                    "invalid --ro-bind-fd file descriptor: {}",
                    fd_argument.to_string_lossy()
                )
            })?;
        if fd <= libc::STDERR_FILENO {
            return Err(format!(
                "--ro-bind-fd file descriptor must not use standard descriptors: {fd}"
            ));
        }
        if verified_fds.contains(&fd) {
            return Err(format!("duplicate --ro-bind-fd file descriptor: {fd}"));
        }

        let destination = argv
            .get(argument_index + 2)
            .filter(|_| argument_index + 2 < command_separator)
            .ok_or_else(|| "--ro-bind-fd is missing its mount destination".to_string())?;
        if !Path::new(destination).is_absolute() {
            return Err(format!(
                "--ro-bind-fd mount destination must be absolute: {}",
                destination.to_string_lossy()
            ));
        }

        verification_args.push(OsString::from("--verify-fd-mount"));
        verification_args.push(OsString::from(format!(
            "{fd}:{}",
            destination.to_string_lossy()
        )));
        verified_fds.push(fd);
        argv[argument_index] = OsString::from("--ro-bind");
        argv[argument_index + 1] = OsString::from(format!("/proc/self/fd/{fd}"));
        argument_index += 3;
    }

    if verification_args.is_empty() {
        return Ok(());
    }
    let inner_command = command_separator + 1;
    if argv.get(inner_command).is_none() {
        return Err("bubblewrap argv is missing the inner command after '--'".to_string());
    }
    if !argv[inner_command + 1..]
        .iter()
        .take_while(|argument| argument.as_os_str() != OsStr::new("--"))
        .any(|argument| argument == "--apply-seccomp-then-exec")
    {
        return Err("descriptor-backed mounts require the trusted inner sandbox stage".to_string());
    }
    argv.splice(inner_command + 1..inner_command + 1, verification_args);
    Ok(())
}

fn preferred_bwrap_launcher() -> BubblewrapLauncher {
    if let Some(launcher) = crate::embedding::selected_bwrap_launcher() {
        return BubblewrapLauncher::Embedding(launcher);
    }

    static LAUNCHER: OnceLock<BubblewrapLauncher> = OnceLock::new();
    LAUNCHER
        .get_or_init(|| {
            if let Some(path) = find_system_bwrap_in_path()
                && let Some(launcher) = system_bwrap_launcher_for_path(&path)
            {
                return BubblewrapLauncher::System(launcher);
            }

            match bundled_bwrap::launcher() {
                Some(launcher) => BubblewrapLauncher::Bundled(launcher),
                None => BubblewrapLauncher::Unavailable,
            }
        })
        .clone()
}

fn system_bwrap_launcher_for_path(system_bwrap_path: &Path) -> Option<SystemBwrapLauncher> {
    system_bwrap_launcher_for_path_with_probe(system_bwrap_path, system_bwrap_capabilities)
}

fn system_bwrap_launcher_for_path_with_probe(
    system_bwrap_path: &Path,
    system_bwrap_capabilities: impl FnOnce(&Path) -> Option<SystemBwrapCapabilities>,
) -> Option<SystemBwrapLauncher> {
    if !system_bwrap_path.is_file() {
        return None;
    }

    let Some(SystemBwrapCapabilities {
        supports_argv0,
        supports_perms: true,
        supports_ro_bind_fd,
    }) = system_bwrap_capabilities(system_bwrap_path)
    else {
        return None;
    };
    let system_bwrap_path = match AbsolutePathBuf::from_absolute_path(system_bwrap_path) {
        Ok(path) => path,
        Err(err) => panic!(
            "failed to normalize system bubblewrap path {}: {err}",
            system_bwrap_path.display()
        ),
    };
    Some(SystemBwrapLauncher {
        program: system_bwrap_path,
        supports_argv0,
        supports_ro_bind_fd,
    })
}

pub(crate) fn preferred_bwrap_supports_argv0() -> bool {
    match preferred_bwrap_launcher() {
        BubblewrapLauncher::System(launcher) => launcher.supports_argv0,
        BubblewrapLauncher::Bundled(_)
        | BubblewrapLauncher::Embedding(_)
        | BubblewrapLauncher::Unavailable => true,
    }
}

fn system_bwrap_capabilities(system_bwrap_path: &Path) -> Option<SystemBwrapCapabilities> {
    // bubblewrap added `--argv0` in v0.9.0:
    // https://github.com/containers/bubblewrap/releases/tag/v0.9.0
    // Older distro packages (for example Ubuntu 20.04/22.04) ship builds that
    // reject `--argv0`, so use the system binary's no-argv0 compatibility path
    // in that case.
    let output = match Command::new(system_bwrap_path).arg("--help").output() {
        Ok(output) => output,
        Err(_) => return None,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.contains("--as-pid-1") && !stderr.contains("--as-pid-1") {
        return None;
    }
    Some(SystemBwrapCapabilities {
        supports_argv0: stdout.contains("--argv0") || stderr.contains("--argv0"),
        supports_perms: stdout.contains("--perms") || stderr.contains("--perms"),
        supports_ro_bind_fd: stdout.contains("--ro-bind-fd") || stderr.contains("--ro-bind-fd"),
    })
}

fn exec_system_bwrap(
    program: &AbsolutePathBuf,
    argv: Vec<OsString>,
    preserved_files: Vec<File>,
) -> ! {
    // System bwrap runs across an exec boundary, so preserved fds must survive exec.
    make_files_inheritable(&preserved_files);

    let program_path = program.as_path().display().to_string();
    let program = CString::new(program.as_path().as_os_str().as_bytes())
        .unwrap_or_else(|err| panic!("invalid system bubblewrap path: {err}"));
    let cstrings = argv_to_cstrings(&argv);
    let mut argv_ptrs: Vec<*const c_char> = cstrings
        .iter()
        .map(CString::as_c_str)
        .map(CStr::as_ptr)
        .collect();
    argv_ptrs.push(std::ptr::null());

    // SAFETY: `program` and every entry in `argv_ptrs` are valid C strings for
    // the duration of the call. On success `execv` does not return.
    unsafe {
        libc::execv(program.as_ptr(), argv_ptrs.as_ptr());
    }
    let err = std::io::Error::last_os_error();
    panic!("failed to exec system bubblewrap {program_path}: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    #[test]
    fn default_process_options_preserve_monitor_standard_streams() {
        let mut argv = vec![OsString::from("bwrap"), OsString::from("--")];

        apply_process_options(&mut argv, MonitorStandardStreams::Preserve);

        assert_eq!(
            argv,
            vec![
                OsString::from("bwrap"),
                OsString::from("--as-pid-1"),
                OsString::from("--"),
            ]
        );
    }

    #[test]
    fn embedding_process_options_release_monitor_standard_streams() {
        let mut argv = vec![OsString::from("bwrap"), OsString::from("--")];

        apply_process_options(&mut argv, MonitorStandardStreams::Release);

        assert_eq!(
            argv,
            vec![
                OsString::from("bwrap"),
                OsString::from("--as-pid-1"),
                OsString::from(CLOSE_MONITOR_STANDARD_STREAMS_ARG),
                OsString::from("--"),
            ]
        );
    }

    #[test]
    fn prefers_system_bwrap_when_help_lists_argv0() {
        let fake_bwrap = NamedTempFile::new().expect("temp file");
        let fake_bwrap_path = fake_bwrap.path();
        let expected = AbsolutePathBuf::from_absolute_path(fake_bwrap_path).expect("absolute");

        assert_eq!(
            system_bwrap_launcher_for_path_with_probe(fake_bwrap_path, |_| {
                Some(SystemBwrapCapabilities {
                    supports_argv0: true,
                    supports_perms: true,
                    supports_ro_bind_fd: true,
                })
            }),
            Some(SystemBwrapLauncher {
                program: expected,
                supports_argv0: true,
                supports_ro_bind_fd: true,
            })
        );
    }

    #[test]
    fn prefers_system_bwrap_when_system_bwrap_lacks_argv0() {
        let fake_bwrap = NamedTempFile::new().expect("temp file");
        let fake_bwrap_path = fake_bwrap.path();

        assert_eq!(
            system_bwrap_launcher_for_path_with_probe(fake_bwrap_path, |_| {
                Some(SystemBwrapCapabilities {
                    supports_argv0: false,
                    supports_perms: true,
                    supports_ro_bind_fd: false,
                })
            }),
            Some(SystemBwrapLauncher {
                program: AbsolutePathBuf::from_absolute_path(fake_bwrap_path).expect("absolute"),
                supports_argv0: false,
                supports_ro_bind_fd: false,
            })
        );
    }

    #[test]
    fn ignores_system_bwrap_when_system_bwrap_lacks_perms() {
        let fake_bwrap = NamedTempFile::new().expect("temp file");

        assert_eq!(
            system_bwrap_launcher_for_path_with_probe(fake_bwrap.path(), |_| {
                Some(SystemBwrapCapabilities {
                    supports_argv0: false,
                    supports_perms: false,
                    supports_ro_bind_fd: false,
                })
            }),
            None
        );
    }

    #[test]
    fn detects_fd_backed_read_only_mount_support_in_system_bwrap_help() {
        let temp_dir = tempfile::tempdir().expect("temp directory");
        let fake_bwrap_path = temp_dir.path().join("bwrap");
        std::fs::write(
            &fake_bwrap_path,
            "#!/bin/sh\nprintf '%s\\n' '--as-pid-1' '--perms' '--argv0' '--ro-bind-fd'\n",
        )
        .expect("write fake bubblewrap");
        std::fs::set_permissions(&fake_bwrap_path, std::fs::Permissions::from_mode(0o755))
            .expect("make fake bubblewrap executable");

        assert_eq!(
            system_bwrap_capabilities(&fake_bwrap_path),
            Some(SystemBwrapCapabilities {
                supports_argv0: true,
                supports_perms: true,
                supports_ro_bind_fd: true,
            })
        );
    }

    #[test]
    fn translates_fd_mounts_for_legacy_system_bubblewrap() {
        let mut argv = vec![
            "bwrap".to_string(),
            "--ro-bind-fd".to_string(),
            "7".to_string(),
            "/tmp/socket-root".to_string(),
            "--".to_string(),
            "/usr/bin/codex-linux-sandbox".to_string(),
            "--apply-seccomp-then-exec".to_string(),
            "--".to_string(),
            "echo".to_string(),
            "--ro-bind-fd".to_string(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        translate_legacy_bwrap_fd_mounts(&mut argv).expect("fd mount should translate");

        assert_eq!(
            argv,
            vec![
                "bwrap".to_string(),
                "--ro-bind".to_string(),
                "/proc/self/fd/7".to_string(),
                "/tmp/socket-root".to_string(),
                "--".to_string(),
                "/usr/bin/codex-linux-sandbox".to_string(),
                "--verify-fd-mount".to_string(),
                "7:/tmp/socket-root".to_string(),
                "--apply-seccomp-then-exec".to_string(),
                "--".to_string(),
                "echo".to_string(),
                "--ro-bind-fd".to_string(),
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn translates_multiple_fd_mounts_for_legacy_system_bubblewrap() {
        let mut argv = vec![
            "bwrap".to_string(),
            "--ro-bind-fd".to_string(),
            "7".to_string(),
            "/tmp/first".to_string(),
            "--ro-bind-fd".to_string(),
            "9".to_string(),
            "/tmp/second:with-colon".to_string(),
            "--".to_string(),
            "codex-linux-sandbox".to_string(),
            "--apply-seccomp-then-exec".to_string(),
            "--".to_string(),
            "true".to_string(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        translate_legacy_bwrap_fd_mounts(&mut argv).expect("fd mounts should translate");

        assert_eq!(
            argv,
            vec![
                "bwrap".to_string(),
                "--ro-bind".to_string(),
                "/proc/self/fd/7".to_string(),
                "/tmp/first".to_string(),
                "--ro-bind".to_string(),
                "/proc/self/fd/9".to_string(),
                "/tmp/second:with-colon".to_string(),
                "--".to_string(),
                "codex-linux-sandbox".to_string(),
                "--verify-fd-mount".to_string(),
                "7:/tmp/first".to_string(),
                "--verify-fd-mount".to_string(),
                "9:/tmp/second:with-colon".to_string(),
                "--apply-seccomp-then-exec".to_string(),
                "--".to_string(),
                "true".to_string(),
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rejects_malformed_legacy_bubblewrap_fd_mounts() {
        for (fd, destination) in [
            ("invalid", "/tmp/socket-root"),
            ("2", "/tmp/socket-root"),
            ("7", "relative/socket-root"),
        ] {
            let mut argv = vec![
                "bwrap".to_string(),
                "--ro-bind-fd".to_string(),
                fd.to_string(),
                destination.to_string(),
                "--".to_string(),
                "codex-linux-sandbox".to_string(),
            ]
            .into_iter()
            .map(OsString::from)
            .collect();

            assert!(translate_legacy_bwrap_fd_mounts(&mut argv).is_err());
        }
    }

    #[test]
    fn rejects_fd_mounts_without_the_trusted_inner_sandbox_stage() {
        let mut argv = vec![
            "bwrap".to_string(),
            "--ro-bind-fd".to_string(),
            "7".to_string(),
            "/tmp/socket-root".to_string(),
            "--".to_string(),
            "/bin/true".to_string(),
            "--".to_string(),
            "--apply-seccomp-then-exec".to_string(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        assert!(translate_legacy_bwrap_fd_mounts(&mut argv).is_err());
    }

    #[test]
    fn rejects_duplicate_legacy_bubblewrap_fd_mounts() {
        let mut argv = vec![
            "bwrap".to_string(),
            "--ro-bind-fd".to_string(),
            "7".to_string(),
            "/tmp/first".to_string(),
            "--ro-bind-fd".to_string(),
            "7".to_string(),
            "/tmp/second".to_string(),
            "--".to_string(),
            "codex-linux-sandbox".to_string(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        assert!(translate_legacy_bwrap_fd_mounts(&mut argv).is_err());
    }

    #[test]
    fn ignores_system_bwrap_when_system_bwrap_is_missing() {
        assert_eq!(
            system_bwrap_launcher_for_path(Path::new("/definitely/not/a/bwrap")),
            None
        );
    }
}
