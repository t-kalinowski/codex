use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::raw::c_char;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use crate::bazel_bwrap;
use crate::exec_util::argv_to_cstrings;
use crate::exec_util::make_files_inheritable;
use codex_install_context::InstallContext;
use codex_utils_absolute_path::AbsolutePathBuf;
use sha2::Digest as _;
use sha2::Sha256;

const SHA256_HEX_LEN: usize = 64;
const NULL_SHA256_DIGEST: [u8; 32] = [0; 32];
const COMPATIBILITY_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_COMPATIBILITY_OUTPUT_BYTES: usize = 1024;
pub(crate) const COMPATIBILITY_QUERY: &str = "--codex-mcp-console-sandbox-bwrap-compatibility-v2";
pub(crate) const COMPATIBILITY_RESPONSE: &str = concat!(
    "mcp-console-sandbox-bwrap/2 codex/",
    env!("CARGO_PKG_VERSION"),
    "\n"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundledBwrapLauncher {
    program: AbsolutePathBuf,
}

#[derive(Clone, Copy)]
pub(crate) enum ExecVerification {
    DigestOnly,
    DigestAndPrivateCompatibility,
}

pub(crate) fn launcher() -> Option<BundledBwrapLauncher> {
    let current_exe = std::env::current_exe().ok()?;
    find_for_install_context(InstallContext::current())
        .or_else(|| find_legacy_for_exe(&current_exe))
        .map(|program| BundledBwrapLauncher { program })
}

pub(crate) fn launcher_for_program(program: &Path) -> Option<BundledBwrapLauncher> {
    if !is_executable_file(program) {
        return None;
    }
    AbsolutePathBuf::from_absolute_path(program)
        .ok()
        .map(|program| BundledBwrapLauncher { program })
}

impl BundledBwrapLauncher {
    pub(crate) fn program(&self) -> &Path {
        self.program.as_path()
    }

    pub(crate) fn verify(&self) -> Result<(), String> {
        let file = File::open(self.program.as_path()).map_err(|err| {
            format!(
                "failed to open packaged bubblewrap {}: {err}",
                self.program.as_path().display()
            )
        })?;
        verify_digest(&file, expected_sha256(), self.program.as_path())?;
        verify_compatibility(self.program.as_path())
    }

    pub(crate) fn exec(
        &self,
        verification: ExecVerification,
        argv: Vec<OsString>,
        preserved_files: Vec<File>,
    ) -> ! {
        let bwrap_file = File::open(self.program.as_path()).unwrap_or_else(|err| {
            panic!(
                "failed to open bundled bubblewrap {}: {err}",
                self.program.as_path().display()
            )
        });
        if let Err(err) = verify_digest(&bwrap_file, expected_sha256(), self.program.as_path()) {
            eprintln!("{err}");
            std::process::exit(crate::BUNDLED_BWRAP_DIGEST_VERIFICATION_FAILURE_EXIT_CODE);
        }
        if matches!(
            verification,
            ExecVerification::DigestAndPrivateCompatibility
        ) && let Err(err) = verify_compatibility(self.program.as_path())
        {
            eprintln!("{err}");
            std::process::exit(crate::BUNDLED_BWRAP_DIGEST_VERIFICATION_FAILURE_EXIT_CODE);
        }

        make_files_inheritable(&preserved_files);

        let fd_path = format!("/proc/self/fd/{}", bwrap_file.as_raw_fd());
        let program_cstring = CString::new(fd_path.as_str())
            .unwrap_or_else(|err| panic!("invalid bundled bubblewrap fd path: {err}"));
        let cstrings = argv_to_cstrings(&argv);
        let mut argv_ptrs: Vec<*const c_char> = cstrings
            .iter()
            .map(CString::as_c_str)
            .map(CStr::as_ptr)
            .collect();
        argv_ptrs.push(std::ptr::null());

        // SAFETY: `program_cstring` and every entry in `argv_ptrs` are valid C
        // strings for the duration of the call. On success `execv` does not return.
        unsafe {
            libc::execv(program_cstring.as_ptr(), argv_ptrs.as_ptr());
        }
        let err = std::io::Error::last_os_error();
        panic!(
            "failed to exec bundled bubblewrap {} via {fd_path}: {err}",
            self.program.as_path().display()
        );
    }
}

fn verify_compatibility(program: &Path) -> Result<(), String> {
    let mut command = Command::new(program);
    command
        .arg(COMPATIBILITY_QUERY)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(|err| {
        format!(
            "failed to query packaged bubblewrap compatibility for {}: {err}",
            program.display()
        )
    })?;
    let Some(mut stdout) = child.stdout.take() else {
        terminate_query(&mut child);
        return Err("packaged bubblewrap compatibility stdout was unavailable".to_string());
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_query(&mut child);
        return Err("packaged bubblewrap compatibility stderr was unavailable".to_string());
    };
    if let Err(err) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        terminate_query(&mut child);
        return Err(err);
    }

    let deadline = Instant::now() + COMPATIBILITY_QUERY_TIMEOUT;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut status = None;
    loop {
        if let Err(err) = read_bounded(&mut stdout, &mut stdout_bytes, &mut stdout_eof)
            .and_then(|()| read_bounded(&mut stderr, &mut stderr_bytes, &mut stderr_eof))
        {
            terminate_query(&mut child);
            return Err(err);
        }
        if stdout_bytes.len() > MAX_COMPATIBILITY_OUTPUT_BYTES
            || stderr_bytes.len() > MAX_COMPATIBILITY_OUTPUT_BYTES
        {
            terminate_query(&mut child);
            return Err(format!(
                "packaged bubblewrap compatibility output exceeded {MAX_COMPATIBILITY_OUTPUT_BYTES} bytes"
            ));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(err) => {
                    terminate_query(&mut child);
                    return Err(format!(
                        "failed to inspect packaged bubblewrap compatibility query: {err}"
                    ));
                }
            }
        }
        if status.is_some() && stdout_eof && stderr_eof {
            break;
        }
        if Instant::now() >= deadline {
            terminate_query(&mut child);
            return Err("packaged bubblewrap compatibility query timed out".to_string());
        }
        thread::sleep(Duration::from_millis(5));
    }

    let Some(status) = status else {
        return Err("completed compatibility query has no exit status".to_string());
    };
    if !status.success()
        || stdout_bytes.as_slice() != COMPATIBILITY_RESPONSE.as_bytes()
        || !stderr_bytes.is_empty()
    {
        return Err(format!(
            "packaged bubblewrap is incompatible with private companion protocol 2: {}",
            program.display()
        ));
    }
    Ok(())
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> Result<(), String> {
    // SAFETY: `fd` is an open pipe owned by this process.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(format!(
            "failed to inspect packaged bubblewrap compatibility pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `fd` remains open and `flags | O_NONBLOCK` is a valid file status value.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(format!(
            "failed to configure packaged bubblewrap compatibility pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn read_bounded(pipe: &mut impl Read, output: &mut Vec<u8>, eof: &mut bool) -> Result<(), String> {
    if *eof {
        return Ok(());
    }
    let mut buffer = [0_u8; 256];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => {
                *eof = true;
                return Ok(());
            }
            Ok(read) => {
                output.extend_from_slice(&buffer[..read]);
                if output.len() > MAX_COMPATIBILITY_OUTPUT_BYTES {
                    return Ok(());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => {
                return Err(format!(
                    "failed to read packaged bubblewrap compatibility output: {err}"
                ));
            }
        }
    }
}

fn terminate_query(child: &mut Child) {
    let process_group = -(child.id() as i32);
    // SAFETY: the compatibility child was placed in a process group whose ID is its PID.
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn find_for_install_context(context: &InstallContext) -> Option<AbsolutePathBuf> {
    context
        .bundled_resource("bwrap")
        .filter(|path| is_executable_file(path))
}

fn find_legacy_for_exe(exe: &Path) -> Option<AbsolutePathBuf> {
    legacy_candidates_for_exe(exe)
        .into_iter()
        .find(|candidate| is_executable_file(candidate))
        .map(|path| {
            AbsolutePathBuf::from_absolute_path(&path).unwrap_or_else(|err| {
                panic!(
                    "failed to normalize bundled bubblewrap path {}: {err}",
                    path.display()
                )
            })
        })
}

fn legacy_candidates_for_exe(exe: &Path) -> Vec<PathBuf> {
    let Some(exe_dir) = exe.parent() else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    candidates.push(exe_dir.join("codex-resources").join("bwrap"));
    if let Some(package_target_dir) = exe_dir.parent() {
        candidates.push(package_target_dir.join("codex-resources").join("bwrap"));
    }
    candidates.push(exe_dir.join("bwrap"));
    if let Some(path) = bazel_bwrap::candidate() {
        candidates.push(path);
    }
    candidates
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

fn expected_sha256() -> Option<[u8; 32]> {
    static EXPECTED: OnceLock<Option<[u8; 32]>> = OnceLock::new();
    *EXPECTED.get_or_init(|| {
        let raw_digest = option_env!("CODEX_BWRAP_SHA256")?;
        let digest = parse_sha256_hex(raw_digest)
            .unwrap_or_else(|err| panic!("invalid CODEX_BWRAP_SHA256 value: {err}"));
        (digest != NULL_SHA256_DIGEST).then_some(digest)
    })
}

fn verify_digest(file: &File, expected: Option<[u8; 32]>, path: &Path) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };

    let mut file = file
        .try_clone()
        .map_err(|err| format!("failed to clone bundled bubblewrap fd: {err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|err| {
            format!(
                "failed to read bundled bubblewrap {} for digest verification: {err}",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let actual: [u8; 32] = hasher.finalize().into();
    if actual == expected {
        return Ok(());
    }

    Err(format!(
        "bundled bubblewrap digest mismatch for {}: expected sha256:{}, got sha256:{}",
        path.display(),
        bytes_to_hex(&expected),
        bytes_to_hex(&actual),
    ))
}

fn parse_sha256_hex(raw: &str) -> Result<[u8; 32], String> {
    if raw.len() != SHA256_HEX_LEN {
        return Err(format!(
            "expected {SHA256_HEX_LEN} hex characters, got {}",
            raw.len()
        ));
    }

    let mut digest = [0_u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&raw[start..start + 2], 16)
            .map_err(|err| format!("invalid hex byte at offset {start}: {err}"))?;
    }
    Ok(digest)
}

fn bytes_to_hex(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(SHA256_HEX_LEN);
    for byte in bytes {
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0x0f) as usize] as char);
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_install_context::CodexPackageLayout;
    use codex_install_context::InstallContext;
    use codex_install_context::InstallMethod;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::NamedTempFile;
    use tempfile::tempdir;

    #[test]
    fn finds_package_layout_bwrap_from_install_context() {
        let temp_dir = tempdir().expect("temp dir");
        let package_dir = temp_dir.path();
        let bin_dir = package_dir.join("bin");
        let resources_dir = package_dir.join("codex-resources");
        let expected_bwrap = resources_dir.join("bwrap");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        write_executable(&expected_bwrap);

        let context = InstallContext {
            method: InstallMethod::Other,
            package_layout: Some(CodexPackageLayout {
                package_dir: AbsolutePathBuf::from_absolute_path(package_dir).expect("absolute"),
                bin_dir: AbsolutePathBuf::from_absolute_path(&bin_dir).expect("absolute"),
                resources_dir: Some(
                    AbsolutePathBuf::from_absolute_path(&resources_dir).expect("absolute"),
                ),
                path_dir: None,
            }),
        };

        assert_eq!(
            find_for_install_context(&context),
            Some(AbsolutePathBuf::from_absolute_path(&expected_bwrap).expect("absolute"))
        );
    }

    #[test]
    fn finds_legacy_standalone_bundled_bwrap_next_to_exe_resources() {
        let temp_dir = tempdir().expect("temp dir");
        let exe = temp_dir.path().join("codex");
        let expected_bwrap = temp_dir.path().join("codex-resources").join("bwrap");
        write_executable(&exe);
        write_executable(&expected_bwrap);

        assert_eq!(
            find_legacy_for_exe(&exe),
            Some(AbsolutePathBuf::from_absolute_path(&expected_bwrap).expect("absolute"))
        );
    }

    #[test]
    fn finds_npm_bundled_bwrap_next_to_target_vendor_dir() {
        let temp_dir = tempdir().expect("temp dir");
        let target_dir = temp_dir.path().join("vendor/x86_64-unknown-linux-musl");
        let exe = target_dir.join("codex").join("codex");
        let expected_bwrap = target_dir.join("codex-resources").join("bwrap");
        write_executable(&exe);
        write_executable(&expected_bwrap);

        assert_eq!(
            find_legacy_for_exe(&exe),
            Some(AbsolutePathBuf::from_absolute_path(&expected_bwrap).expect("absolute"))
        );
    }

    #[test]
    fn finds_adjacent_dev_bwrap() {
        let temp_dir = tempdir().expect("temp dir");
        let exe = temp_dir.path().join("codex");
        let expected_bwrap = temp_dir.path().join("bwrap");
        write_executable(&exe);
        write_executable(&expected_bwrap);

        assert_eq!(
            find_legacy_for_exe(&exe),
            Some(AbsolutePathBuf::from_absolute_path(&expected_bwrap).expect("absolute"))
        );
    }

    #[test]
    fn digest_verification_skips_missing_expected_digest() {
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), b"contents").expect("write file");

        verify_digest(file.as_file(), /*expected*/ None, file.path())
            .expect("missing digest should skip verification");
    }

    #[test]
    fn digest_verification_accepts_matching_digest() {
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), b"contents").expect("write file");
        let expected: [u8; 32] = Sha256::digest(b"contents").into();

        verify_digest(file.as_file(), Some(expected), file.path())
            .expect("matching digest should verify");
    }

    #[test]
    fn digest_verification_rejects_mismatched_digest() {
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), b"contents").expect("write file");

        let err = verify_digest(file.as_file(), Some([0xab; 32]), file.path())
            .expect_err("mismatched digest should fail");
        assert!(err.contains("bundled bubblewrap digest mismatch"));
    }

    #[test]
    fn parses_sha256_hex_digest() {
        assert_eq!(parse_sha256_hex(&"ab".repeat(32)), Ok([0xab; 32]));
        assert_eq!(parse_sha256_hex(&"00".repeat(32)), Ok(NULL_SHA256_DIGEST));
        assert!(parse_sha256_hex("ab").is_err());
        assert!(parse_sha256_hex(&format!("{}xx", "00".repeat(31))).is_err());
    }

    fn write_executable(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, b"").expect("write executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("set executable permissions");
    }
}
