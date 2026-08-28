use std::ffi::CString;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;

pub(crate) fn argv_to_cstrings(argv: &[OsString]) -> Vec<CString> {
    let mut cstrings: Vec<CString> = Vec::with_capacity(argv.len());
    for arg in argv {
        match CString::new(arg.as_os_str().as_bytes()) {
            Ok(value) => cstrings.push(value),
            Err(err) => panic!("failed to convert argv to CString: {err}"),
        }
    }
    cstrings
}

pub(crate) fn make_files_inheritable(files: &[File]) {
    for file in files {
        clear_cloexec(file.as_raw_fd());
    }
}

fn clear_cloexec(fd: libc::c_int) {
    // SAFETY: `fd` is an owned descriptor kept alive by `files`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        let err = std::io::Error::last_os_error();
        panic!("failed to read fd flags for preserved bubblewrap file descriptor {fd}: {err}");
    }
    let cleared_flags = flags & !libc::FD_CLOEXEC;
    if cleared_flags == flags {
        return;
    }

    // SAFETY: `fd` is valid and we are only clearing FD_CLOEXEC.
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, cleared_flags) };
    if result < 0 {
        let err = std::io::Error::last_os_error();
        panic!("failed to clear CLOEXEC for preserved bubblewrap file descriptor {fd}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::NamedTempFile;

    #[test]
    fn preserved_files_are_made_inheritable() {
        let file = NamedTempFile::new().expect("temp file");
        set_cloexec(file.as_file().as_raw_fd());

        make_files_inheritable(std::slice::from_ref(file.as_file()));

        assert_eq!(fd_flags(file.as_file().as_raw_fd()) & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn argv_conversion_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let native = OsString::from_vec(vec![b'a', b'r', b'g', 0x80]);
        let converted = argv_to_cstrings(std::slice::from_ref(&native));

        assert_eq!(converted[0].as_bytes(), native.as_encoded_bytes());
    }

    fn set_cloexec(fd: libc::c_int) {
        let flags = fd_flags(fd);
        // SAFETY: `fd` is valid for the duration of the test.
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            panic!("failed to set CLOEXEC for test fd {fd}: {err}");
        }
    }

    fn fd_flags(fd: libc::c_int) -> libc::c_int {
        // SAFETY: `fd` is valid for the duration of the test.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            let err = std::io::Error::last_os_error();
            panic!("failed to read fd flags for test fd {fd}: {err}");
        }
        flags
    }
}
