use std::ffi::CString;
use std::ffi::OsString;
use std::io;
use std::mem::MaybeUninit;

pub(super) fn cpu_count() -> Result<(), String> {
    let count = sysctl_value::<libc::c_int>("hw.logicalcpu")?;
    if count <= 0 {
        return Err(format!("invalid logical CPU count {count}"));
    }
    Ok(())
}

pub(super) fn boottime() -> Result<(), String> {
    let boottime = sysctl_value::<libc::timeval>("kern.boottime")?;
    if boottime.tv_sec <= 0 {
        return Err(format!("invalid boot time {}", boottime.tv_sec));
    }
    Ok(())
}

pub(super) fn posix_semaphore() -> Result<(), String> {
    let name = CString::new(format!("/codex-sandbox-api-{}", std::process::id()))
        .map_err(|error| error.to_string())?;
    let semaphore = unsafe {
        libc::sem_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL,
            /*mode*/ 0o600,
            /*value*/ 1,
        )
    };
    if semaphore == libc::SEM_FAILED {
        return Err(io::Error::last_os_error().to_string());
    }
    let close_result = unsafe { libc::sem_close(semaphore) };
    let unlink_result = unsafe { libc::sem_unlink(name.as_ptr()) };
    if close_result != 0 || unlink_result != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

pub(super) fn pty_created() -> Result<(), String> {
    let mut master = -1;
    let mut slave = -1;
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().to_string());
    }
    let result = exchange_pty_bytes(master, slave);
    unsafe {
        libc::close(master);
        libc::close(slave);
    }
    result
}

pub(super) fn terminal_reopen_denied(
    args: &mut impl Iterator<Item = OsString>,
) -> Result<(), String> {
    let path = super::next_path(args, "terminal path")?;
    let path =
        CString::new(path.as_os_str().as_encoded_bytes()).map_err(|error| error.to_string())?;
    for flags in [libc::O_RDONLY, libc::O_WRONLY] {
        let fd = unsafe { libc::open(path.as_ptr(), flags) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            return Err(format!("reopened pre-existing terminal with flags {flags}"));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EPERM) {
            return Err(format!(
                "pre-existing terminal open failed unexpectedly with flags {flags}: {error}"
            ));
        }
    }
    Ok(())
}

fn sysctl_value<T>(name: &str) -> Result<T, String> {
    let name = CString::new(name).map_err(|error| error.to_string())?;
    let mut value = MaybeUninit::<T>::uninit();
    let mut length = std::mem::size_of::<T>();
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            /*newlen*/ 0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().to_string());
    }
    if length != std::mem::size_of::<T>() {
        return Err(format!("unexpected sysctl value size {length}"));
    }
    Ok(unsafe { value.assume_init() })
}

fn exchange_pty_bytes(master: libc::c_int, slave: libc::c_int) -> Result<(), String> {
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(slave, termios.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let mut termios = unsafe { termios.assume_init() };
    unsafe { libc::cfmakeraw(&mut termios) };
    if unsafe { libc::tcsetattr(slave, libc::TCSANOW, &termios) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }

    let expected = b"pty-bytes\0\x80\xff";
    if unsafe { libc::write(master, expected.as_ptr().cast(), expected.len()) }
        != expected.len() as isize
    {
        return Err(io::Error::last_os_error().to_string());
    }
    let mut actual = [0; 12];
    if unsafe { libc::read(slave, actual.as_mut_ptr().cast(), actual.len()) }
        != actual.len() as isize
    {
        return Err(io::Error::last_os_error().to_string());
    }
    if actual != *expected {
        return Err(format!("PTY bytes changed: {actual:?}"));
    }
    Ok(())
}
