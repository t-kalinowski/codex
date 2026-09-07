#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpStream;

    let mut args = std::env::args().skip(1);
    #[cfg(target_os = "linux")]
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(|path| std::path::Path::new(path).file_name())
        == Some(std::ffi::OsStr::new("bwrap"))
    {
        // Executable selection seam: advertise supported host options, then
        // return a distinctive exit code without entering a namespace.
        if args.next().as_deref() == Some("--help") {
            if std::env::var_os("TEST_BWRAP_UNSUITABLE").is_none() {
                println!("--as-pid-1 --perms --ro-bind-fd");
                if std::env::var_os("TEST_BWRAP_NO_ARGV0").is_none() {
                    println!("--argv0");
                }
            }
            return Ok(());
        }
        if let Some(program) = std::env::var_os("TEST_BWRAP_EXECUTABLE") {
            use std::os::unix::process::CommandExt;
            return Err(std::process::Command::new(program)
                .args(std::env::args_os().skip(1))
                .exec()
                .into());
        }
        std::process::exit(97);
    }
    let operation = args.next().context("fixture operation")?;
    match operation.as_str() {
        "stdout" | "stderr" => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input)?;
            if operation == "stdout" {
                std::io::stdout().write_all(&input)?;
            } else {
                std::io::stderr().write_all(&input)?;
            }
        }
        "context" => {
            let environment: std::collections::BTreeMap<_, _> = std::env::vars().collect();
            serde_json::to_writer(
                std::io::stdout(),
                &serde_json::json!({
                    "cwd": std::env::current_dir()?, "environment": environment
                }),
            )?;
        }
        "write" => std::fs::write(args.next().context("file path")?, b"created")?,
        "exit" => std::process::exit(args.next().context("exit code")?.parse()?),
        "signal" => {
            let signal: i32 = args.next().context("signal")?.parse()?;
            // SAFETY: the fixture intentionally signals itself.
            if unsafe { libc::raise(signal) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            anyhow::bail!("signal did not terminate the fixture");
        }
        "descriptors" => {
            #[cfg(target_os = "linux")]
            let directory = "/proc/self/fd";
            #[cfg(target_os = "macos")]
            let directory = "/dev/fd";
            let mut fds = std::fs::read_dir(directory)?
                .map(|entry| {
                    let entry = entry?;
                    entry
                        .file_name()
                        .to_str()
                        .context("descriptor name")?
                        .parse::<i32>()
                        .map_err(Into::into)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            // read_dir has closed its own fd; report all remaining descriptors.
            fds.retain(|fd| *fd > 2 && unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0);
            fds.sort_unstable();
            serde_json::to_writer(std::io::stdout(), &fds)?;
        }
        "connect" => {
            TcpStream::connect(args.next().context("address")?)?;
        }
        "proxy-get" => {
            let url = args.next().context("URL")?;
            let address = std::env::var("HTTP_PROXY")?;
            let mut stream = TcpStream::connect(address.trim_start_matches("http://"))?;
            let authority = url
                .strip_prefix("http://")
                .context("HTTP URL")?
                .split('/')
                .next()
                .context("authority")?;
            write!(
                stream,
                "GET {url} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
            )?;
            std::io::copy(&mut stream, &mut std::io::stdout())?;
        }
        #[cfg(target_os = "macos")]
        "pty" => {
            use std::fs::File;
            use std::os::fd::FromRawFd;
            use std::os::unix::fs::OpenOptionsExt;

            let path = args.next().context("host PTY path")?;
            let host_readable = match File::options()
                .read(true)
                .custom_flags(libc::O_NOCTTY)
                .open(path)
            {
                Ok(_) => true,
                Err(error) if error.raw_os_error() == Some(libc::EPERM) => false,
                Err(error) => return Err(error.into()),
            };
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: openpty initializes both descriptors; null uses default
            // terminal settings and does not request the slave pathname.
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
                return Err(std::io::Error::last_os_error().into());
            }
            let mut name = [0; libc::PATH_MAX as usize];
            // SAFETY: slave is an open PTY and name provides the stated space.
            let error = unsafe { libc::ttyname_r(slave, name.as_mut_ptr(), name.len()) };
            anyhow::ensure!(
                error == 0,
                "PTY name lookup: {}",
                std::io::Error::from_raw_os_error(error)
            );
            // SAFETY: the fixture owns the two newly opened descriptors.
            let (mut master, mut slave) =
                unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
            slave.write_all(b"output")?;
            let mut output = [0; 6];
            master.read_exact(&mut output)?;
            master.write_all(b"input\n")?;
            let mut input = [0; 6];
            slave.read_exact(&mut input)?;
            serde_json::to_writer(
                std::io::stdout(),
                &serde_json::json!({
                    "host_readable": host_readable,
                    "output": String::from_utf8(output.to_vec())?,
                    "input": String::from_utf8(input.to_vec())?,
                }),
            )?;
        }
        #[cfg(target_os = "macos")]
        "sysctl" => {
            let name = std::ffi::CString::new(args.next().context("sysctl name")?)?;
            let mut length = 0;
            // SAFETY: a null output requests the size of the named sysctl only.
            if unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        _ => anyhow::bail!("unknown fixture operation: {operation}"),
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {}
