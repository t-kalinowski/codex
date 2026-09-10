use anyhow::Context;
use anyhow::Result;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub struct Storage {
    pub root: PathBuf,
    pub data: PathBuf,
    directory: File,
}

impl Storage {
    pub fn create(config: &crate::config::PrivateTmp) -> Result<Self> {
        let parent = config
            .parent
            .clone()
            .unwrap_or_else(std::env::temp_dir)
            .canonicalize()?;
        let template = CString::new(parent.join("sandbox-XXXXXX").as_os_str().as_bytes())?;
        let mut bytes = template.into_bytes_with_nul();
        if unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) }.is_null() {
            return Err(std::io::Error::last_os_error()).context("create private storage");
        }
        bytes.pop();
        let root = PathBuf::from(std::ffi::OsStr::from_bytes(&bytes));
        let data = root.join("data");
        if let Err(error) = std::fs::create_dir(&data) {
            let cleanup = std::fs::remove_dir(&root);
            return Err(anyhow::anyhow!(
                "create private data: {error}; cleanup: {cleanup:?}"
            ));
        }
        let directory = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&root)?;
        Ok(Self {
            root,
            data,
            directory,
        })
    }

    /// Called only after retirement proves there is no executing descendant.
    pub fn remove(self) -> Result<()> {
        let result = (|| {
            if unsafe { libc::fchmod(self.directory.as_raw_fd(), 0o700) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            remove_contents(&self.directory)?;
            std::fs::remove_dir(&self.root)
        })();
        result.with_context(|| format!("remove private storage: {}", self.root.display()))
    }
}

fn remove_contents(directory: &File) -> std::io::Result<()> {
    let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let entries = unsafe { libc::fdopendir(fd) };
    if entries.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(std::io::Error::last_os_error());
    }
    let entries = Directory(entries);
    loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(entries.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error);
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let mut info: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                &mut info,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let is_directory = info.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if is_directory {
            // Retirement precedes traversal. Never follow a workload symlink
            // or change permissions on a regular-file hard link.
            #[cfg(target_os = "macos")]
            if unsafe {
                libc::fchmodat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    0o700,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "macos")]
            let mode = libc::O_RDONLY | libc::O_DIRECTORY;
            #[cfg(target_os = "linux")]
            let mode = libc::O_PATH;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    mode | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let child = unsafe { File::from_raw_fd(fd) };
            if !child.metadata()?.is_dir() {
                return Err(std::io::Error::other(
                    "private directory changed during cleanup",
                ));
            }
            #[cfg(target_os = "linux")]
            std::fs::set_permissions(
                format!("/proc/self/fd/{fd}"),
                std::fs::Permissions::from_mode(0o700),
            )?;
            let readable = unsafe {
                libc::openat(
                    fd,
                    c".".as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if readable < 0 {
                return Err(std::io::Error::last_os_error());
            }
            remove_contents(&unsafe { File::from_raw_fd(readable) })?;
        }
        if unsafe {
            libc::unlinkat(
                directory.as_raw_fd(),
                name.as_ptr(),
                if is_directory { libc::AT_REMOVEDIR } else { 0 },
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}
