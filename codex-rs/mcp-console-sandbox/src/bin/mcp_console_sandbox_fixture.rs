#![allow(clippy::expect_used)]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "macos")]
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

fn main() {
    #[cfg(target_os = "linux")]
    if dispatch_path_bwrap_probe() {
        return;
    }

    let mut arguments = std::env::args_os().skip(1);
    let operation = arguments.next().expect("fixture operation");
    let result = match operation.to_str() {
        Some("argv") => write_native_values(arguments.collect()),
        Some("environment") => write_environment(arguments.collect()),
        Some("cwd") => write_native_values(vec![
            std::env::current_dir()
                .expect("current directory")
                .into_os_string(),
        ]),
        Some("copy") => copy_streams(),
        #[cfg(unix)]
        Some("close-stdin-then-ready-and-wait") => close_stdin_then_ready_and_wait(),
        #[cfg(target_os = "linux")]
        Some("connected-unix-stream-io") => connected_unix_stream_io(),
        Some("emit-large") => emit_large(parse_usize(arguments.next())),
        Some("write") => write_file(arguments.next(), arguments.next()),
        Some("connect") => connect(arguments.next(), arguments.next()),
        Some("bind") => bind(arguments.next(), arguments.next()),
        Some("http-get") => http_get(arguments.next(), arguments.next()),
        Some("http-request") => http_request(arguments.next(), arguments.next(), arguments.next()),
        Some("http-follow") => http_follow(arguments.next(), arguments.next()),
        Some("socks-connect") => socks_connect(arguments.next(), arguments.next()),
        #[cfg(unix)]
        Some("unix-connect") => unix_connect(arguments.next()),
        #[cfg(unix)]
        Some("tty-status") => tty_status(),
        #[cfg(unix)]
        Some("open-controlling-terminal") => open_controlling_terminal(),
        #[cfg(unix)]
        Some("create-pty") => create_pty(),
        #[cfg(target_os = "macos")]
        Some("macos-policy-probe") => macos_policy_probe(),
        #[cfg(target_os = "macos")]
        Some("pty-runner-launcher") => pty_runner_launcher(arguments),
        Some("reopen") => reopen(arguments.next()),
        Some("exit") => std::process::exit(parse_i32(arguments.next())),
        Some("sleep") => {
            std::thread::sleep(Duration::from_millis(parse_u64(arguments.next())));
            Ok(())
        }
        Some("spawn-descendant") => spawn_descendant(arguments.next()),
        Some("spawn-descendant-and-sleep") => {
            spawn_descendant_and_sleep(arguments.next(), arguments.next())
        }
        #[cfg(unix)]
        Some("spawn-session-escaping-descendant") => {
            spawn_session_escaping_descendant(arguments.next())
        }
        #[cfg(unix)]
        Some("spawn-session-escaping-descendant-and-wait") => {
            spawn_session_escaping_descendant_and_wait(arguments.next(), "sleep")
        }
        #[cfg(unix)]
        Some("spawn-session-escaping-stubborn-descendant-and-wait") => {
            spawn_session_escaping_descendant_and_wait(
                arguments.next(),
                "ignore-terminate-and-sleep",
            )
        }
        #[cfg(target_os = "macos")]
        Some("spawn-session-escaping-signal-aware-descendant") => {
            spawn_session_escaping_signal_aware_descendant(arguments.next())
        }
        #[cfg(unix)]
        Some("echo-then-sleep") => echo_then_sleep(parse_u64(arguments.next())),
        Some("ready-then-wait") => ready_then_wait(),
        #[cfg(unix)]
        Some("lock-child-and-exit") => lock_child_and_exit(parse_i32(arguments.next())),
        #[cfg(unix)]
        Some("ignore-terminate-and-sleep") => {
            if unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) } == libc::SIG_ERR {
                Err(std::io::Error::last_os_error().to_string())
            } else {
                let ready = std::io::stdout()
                    .write_all(b"R")
                    .and_then(|()| std::io::stdout().flush());
                std::thread::sleep(Duration::from_millis(parse_u64(arguments.next())));
                ready.map_err(|error| error.to_string())
            }
        }
        #[cfg(unix)]
        Some("signal") => signal(parse_i32(arguments.next())),
        #[cfg(target_os = "macos")]
        Some("signal-disposition") => signal_disposition(parse_i32(arguments.next())),
        #[cfg(target_os = "macos")]
        Some("signal-aware-sleep") => signal_aware_sleep(parse_u64(arguments.next())),
        _ => Err(format!("unknown fixture operation: {operation:?}")),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(73);
    }
}

#[cfg(unix)]
fn lock_child_and_exit(code: i32) -> Result<(), String> {
    let directory = std::env::current_dir()
        .map_err(|error| error.to_string())?
        .join("locked");
    std::fs::create_dir(&directory).map_err(|error| error.to_string())?;
    std::fs::write(directory.join("data"), b"data").map_err(|error| error.to_string())?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o0))
        .map_err(|error| error.to_string())?;
    std::process::exit(code)
}

#[cfg(target_os = "linux")]
fn dispatch_path_bwrap_probe() -> bool {
    let Some(marker) = std::env::var_os("TEST_MCP_CONSOLE_PATH_BWRAP_MARKER") else {
        return false;
    };
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(|arg0| std::path::Path::new(arg0).file_name())
        != Some(OsStr::new("bwrap"))
    {
        return false;
    }
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new("--help")) {
        println!("--as-pid-1 --argv0 --perms --ro-bind-fd");
    } else {
        std::fs::write(marker, b"selected").expect("write PATH bubblewrap marker");
        std::process::exit(97);
    }
    true
}

fn write_native_values(values: Vec<OsString>) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for value in values {
        let bytes = native_bytes(&value);
        let length = u32::try_from(bytes.len()).map_err(|_| "native value is too large")?;
        stdout
            .write_all(&length.to_be_bytes())
            .map_err(|error| error.to_string())?;
        stdout
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn write_environment(keys: Vec<OsString>) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for key in keys {
        match std::env::var_os(&key) {
            Some(value) => {
                let bytes = native_bytes(&value);
                let length =
                    u32::try_from(bytes.len()).map_err(|_| "environment value is too large")?;
                stdout
                    .write_all(&length.to_be_bytes())
                    .and_then(|()| stdout.write_all(&bytes))
                    .map_err(|error| error.to_string())?;
            }
            None => stdout
                .write_all(&u32::MAX.to_be_bytes())
                .map_err(|error| error.to_string())?,
        }
    }
    Ok(())
}

fn copy_streams() -> Result<(), String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    std::io::stderr()
        .write_all(&bytes)
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn close_stdin_then_ready_and_wait() -> Result<(), String> {
    if unsafe { libc::close(libc::STDIN_FILENO) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    std::io::stdout()
        .write_all(b"R")
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    loop {
        unsafe {
            libc::pause();
        }
    }
}

#[cfg(target_os = "linux")]
fn connected_unix_stream_io() -> Result<(), String> {
    let mut socket_type = 0;
    let mut socket_type_length = libc::socklen_t::try_from(std::mem::size_of_val(&socket_type))
        .map_err(|error| error.to_string())?;
    if unsafe {
        libc::getsockopt(
            libc::STDOUT_FILENO,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut socket_type).cast(),
            &raw mut socket_type_length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if socket_type != libc::SOCK_STREAM {
        return Err(format!("unexpected socket type: {socket_type}"));
    }
    let keepalive = 0;
    if unsafe {
        libc::setsockopt(
            libc::STDOUT_FILENO,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            (&raw const keepalive).cast(),
            libc::socklen_t::try_from(std::mem::size_of_val(&keepalive))
                .map_err(|error| error.to_string())?,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }

    for address_call in [
        libc::getsockname as unsafe extern "C" fn(_, _, _) -> _,
        libc::getpeername as unsafe extern "C" fn(_, _, _) -> _,
    ] {
        let mut address = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut address_length =
            libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_storage>())
                .map_err(|error| error.to_string())?;
        if unsafe {
            address_call(
                libc::STDOUT_FILENO,
                address.as_mut_ptr().cast(),
                &raw mut address_length,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let address = unsafe { address.assume_init() };
        if i32::from(address.ss_family) != libc::AF_UNIX {
            return Err(format!("unexpected socket family: {}", address.ss_family));
        }
    }

    let ready = b"R";
    let written = unsafe {
        libc::sendto(
            libc::STDOUT_FILENO,
            ready.as_ptr().cast(),
            ready.len(),
            libc::MSG_NOSIGNAL,
            std::ptr::null(),
            0,
        )
    };
    if written != 1 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let mut request = [0_u8; 1];
    loop {
        let read = unsafe {
            libc::recvfrom(
                libc::STDIN_FILENO,
                request.as_mut_ptr().cast(),
                request.len(),
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if read == 1 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error.to_string());
        }
        std::thread::yield_now();
    }
    if request != *b"I" {
        return Err(format!("unexpected request byte: {request:?}"));
    }

    let response = b"O";
    let written = unsafe {
        libc::sendto(
            libc::STDOUT_FILENO,
            response.as_ptr().cast(),
            response.len(),
            libc::MSG_NOSIGNAL,
            std::ptr::null(),
            0,
        )
    };
    if written != 1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if unsafe { libc::shutdown(libc::STDOUT_FILENO, libc::SHUT_WR) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn bind(host: Option<OsString>, port: Option<OsString>) -> Result<(), String> {
    let host = host
        .and_then(|host| host.into_string().ok())
        .ok_or_else(|| "missing or non-Unicode bind host".to_string())?;
    let port = port
        .and_then(|port| port.into_string().ok())
        .ok_or_else(|| "missing or non-Unicode bind port".to_string())?;
    std::net::TcpListener::bind(format!("{host}:{port}"))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn emit_large(length: usize) -> Result<(), String> {
    let stdout = std::thread::spawn(move || {
        let bytes = vec![b'o'; length];
        std::io::stdout().write_all(&bytes)
    });
    let stderr = std::thread::spawn(move || {
        let bytes = vec![b'e'; length];
        std::io::stderr().write_all(&bytes)
    });
    stdout
        .join()
        .map_err(|_| "stdout writer panicked".to_string())?
        .map_err(|error| error.to_string())?;
    stderr
        .join()
        .map_err(|_| "stderr writer panicked".to_string())?
        .map_err(|error| error.to_string())
}

fn write_file(path: Option<OsString>, contents: Option<OsString>) -> Result<(), String> {
    let path = PathBuf::from(path.ok_or_else(|| "missing path".to_string())?);
    let contents = contents.ok_or_else(|| "missing contents".to_string())?;
    std::fs::write(path, native_bytes(&contents)).map_err(|error| error.to_string())
}

fn connect(host: Option<OsString>, port: Option<OsString>) -> Result<(), String> {
    let host = unicode(host, "host")?;
    let port = unicode(port, "port")?;
    TcpStream::connect(format!("{host}:{port}")).map_err(|error| error.to_string())?;
    Ok(())
}

fn http_get(host: Option<OsString>, port: Option<OsString>) -> Result<(), String> {
    let host = unicode(host, "host")?;
    let port = unicode(port, "port")?;
    let response = proxy_http_request("GET", &host, &port, "/")?;
    require_http_status(&response, /*status*/ 200)?;
    std::io::stdout()
        .write_all(&response)
        .map_err(|error| error.to_string())
}

fn http_request(
    method: Option<OsString>,
    host: Option<OsString>,
    port: Option<OsString>,
) -> Result<(), String> {
    let method = unicode(method, "method")?;
    let host = unicode(host, "host")?;
    let port = unicode(port, "port")?;
    let response = proxy_http_request(&method, &host, &port, "/")?;
    require_http_status(&response, /*status*/ 200)
}

fn http_follow(host: Option<OsString>, port: Option<OsString>) -> Result<(), String> {
    let host = unicode(host, "host")?;
    let port = unicode(port, "port")?;
    let response = proxy_http_request("GET", &host, &port, "/")?;
    require_http_status(&response, /*status*/ 302)?;
    let response_text = std::str::from_utf8(&response).map_err(|error| error.to_string())?;
    let location = response_text
        .lines()
        .find_map(|line| line.strip_prefix("Location: "))
        .ok_or_else(|| "redirect response is missing Location".to_string())?;
    let authority_and_path = location
        .strip_prefix("http://")
        .ok_or_else(|| "redirect Location is not an HTTP URL".to_string())?;
    let (authority, path) = authority_and_path
        .split_once('/')
        .unwrap_or((authority_and_path, ""));
    let (redirect_host, redirect_port) = authority
        .rsplit_once(':')
        .ok_or_else(|| "redirect Location requires an explicit port".to_string())?;
    let response = proxy_http_request("GET", redirect_host, redirect_port, &format!("/{path}"))?;
    require_http_status(&response, /*status*/ 200)?;
    std::io::stdout()
        .write_all(&response)
        .map_err(|error| error.to_string())
}

fn proxy_http_request(method: &str, host: &str, port: &str, path: &str) -> Result<Vec<u8>, String> {
    let proxy = std::env::var("HTTP_PROXY").map_err(|_| "HTTP_PROXY is missing".to_string())?;
    let proxy = proxy
        .strip_prefix("http://")
        .ok_or_else(|| "HTTP_PROXY is not an HTTP URL".to_string())?;
    let mut stream = TcpStream::connect(proxy).map_err(|error| error.to_string())?;
    write!(
        stream,
        "{method} http://{host}:{port}{path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    Ok(response)
}

fn require_http_status(response: &[u8], status: u16) -> Result<(), String> {
    let expected_11 = format!("HTTP/1.1 {status}");
    let expected_10 = format!("HTTP/1.0 {status}");
    if response.starts_with(expected_11.as_bytes()) || response.starts_with(expected_10.as_bytes())
    {
        Ok(())
    } else {
        Err(format!("HTTP request did not return status {status}"))
    }
}

fn socks_connect(host: Option<OsString>, port: Option<OsString>) -> Result<(), String> {
    let host = unicode(host, "host")?;
    let port = unicode(port, "port")?
        .parse::<u16>()
        .map_err(|error| error.to_string())?;
    let proxy = std::env::var("ALL_PROXY").map_err(|_| "ALL_PROXY is missing".to_string())?;
    let proxy = proxy
        .strip_prefix("socks5h://")
        .or_else(|| proxy.strip_prefix("socks5://"))
        .ok_or_else(|| "ALL_PROXY is not a SOCKS5 URL".to_string())?;
    let mut stream = TcpStream::connect(proxy).map_err(|error| error.to_string())?;
    stream
        .write_all(&[5, 1, 0])
        .map_err(|error| error.to_string())?;
    let mut greeting = [0_u8; 2];
    stream
        .read_exact(&mut greeting)
        .map_err(|error| error.to_string())?;
    if greeting != [5, 0] {
        return Err(format!("SOCKS5 greeting failed: {greeting:?}"));
    }
    let host_length = u8::try_from(host.len()).map_err(|_| "SOCKS5 host is too long")?;
    let mut request = vec![5, 1, 0, 3, host_length];
    request.extend(host.as_bytes());
    request.extend(port.to_be_bytes());
    stream
        .write_all(&request)
        .map_err(|error| error.to_string())?;
    let mut response = [0_u8; 4];
    stream
        .read_exact(&mut response)
        .map_err(|error| error.to_string())?;
    if response[0] != 5 || response[1] != 0 {
        return Err(format!("SOCKS5 connect failed: {response:?}"));
    }
    let address_length = match response[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0_u8; 1];
            stream
                .read_exact(&mut length)
                .map_err(|error| error.to_string())?;
            length[0] as usize
        }
        kind => return Err(format!("invalid SOCKS5 address kind: {kind}")),
    };
    let mut remainder = vec![0_u8; address_length + 2];
    stream
        .read_exact(&mut remainder)
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn unix_connect(path: Option<OsString>) -> Result<(), String> {
    use std::os::unix::net::UnixStream;
    UnixStream::connect(path.ok_or_else(|| "missing Unix socket path".to_string())?)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn tty_status() -> Result<(), String> {
    let status = [
        unsafe { libc::isatty(libc::STDIN_FILENO) } as u8,
        unsafe { libc::isatty(libc::STDOUT_FILENO) } as u8,
        unsafe { libc::isatty(libc::STDERR_FILENO) } as u8,
    ];
    std::io::stdout()
        .write_all(&status)
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn open_controlling_terminal() -> Result<(), String> {
    std::fs::File::open("/dev/tty")
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn create_pty() -> Result<(), String> {
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
    } == -1
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let is_tty = unsafe { libc::isatty(slave) } == 1;
    unsafe {
        libc::close(master);
        libc::close(slave);
    }
    if !is_tty {
        return Err("new pseudo-terminal slave is not a TTY".to_string());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_policy_probe() -> Result<(), String> {
    use std::ffi::CString;

    let dtrace_helper = CString::new("/dev/dtracehelper").expect("static device path");
    let descriptor =
        unsafe { libc::open(dtrace_helper.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if descriptor == -1 {
        return Err(format!(
            "open /dev/dtracehelper: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut available = 0;
    let ioctl_result = unsafe { libc::ioctl(descriptor, libc::FIONREAD, &mut available) };
    let ioctl_error = std::io::Error::last_os_error();
    unsafe {
        libc::close(descriptor);
    }
    if ioctl_result == -1 && ioctl_error.raw_os_error() == Some(libc::EPERM) {
        return Err(format!("ioctl /dev/dtracehelper: {ioctl_error}"));
    }

    for name in [
        "kern.boottime",
        "kern.ngroups",
        "machdep.ptrauth_enabled",
        "security.mac.lockdown_mode_state",
        "kern.bootargs",
    ] {
        let name = CString::new(name).expect("static sysctl name");
        let mut length = 0;
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOENT) {
                return Err(format!("read sysctl {}: {error}", name.to_string_lossy()));
            }
        }
    }

    for (program, arguments) in [
        ("/usr/sbin/sysctl", ["-b", "kern.boottime"]),
        ("/usr/sbin/scutil", ["--proxy", ""]),
    ] {
        let mut command = Command::new(program);
        command
            .args(
                arguments
                    .into_iter()
                    .filter(|argument| !argument.is_empty()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = command.status().map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!("{program} failed with status {status}"));
        }
    }
    Ok(())
}

fn reopen(path: Option<OsString>) -> Result<(), String> {
    std::fs::File::open(path.ok_or_else(|| "missing reopen path".to_string())?)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn spawn_descendant(milliseconds: Option<OsString>) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Command::new(executable)
        .arg("sleep")
        .arg(milliseconds.ok_or_else(|| "missing duration".to_string())?)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn spawn_descendant_and_sleep(
    descendant_milliseconds: Option<OsString>,
    root_milliseconds: Option<OsString>,
) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let descendant = Command::new(executable)
        .arg("sleep")
        .arg(descendant_milliseconds.ok_or_else(|| "missing descendant duration".to_string())?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .write_all(&descendant.id().to_be_bytes())
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    std::thread::sleep(Duration::from_millis(parse_u64(root_milliseconds)));
    Ok(())
}

#[cfg(unix)]
fn spawn_session_escaping_descendant(milliseconds: Option<OsString>) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .arg("echo-then-sleep")
        .arg(milliseconds.ok_or_else(|| "missing duration".to_string())?);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(unix)]
fn spawn_session_escaping_descendant_and_wait(
    descendant_milliseconds: Option<OsString>,
    operation: &str,
) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .arg(operation)
        .arg(descendant_milliseconds.ok_or_else(|| "missing descendant duration".to_string())?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let descendant = command.spawn().map_err(|error| error.to_string())?;
    std::io::stdout()
        .write_all(&descendant.id().to_be_bytes())
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    std::io::stdin()
        .read_to_end(&mut Vec::new())
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_session_escaping_signal_aware_descendant(
    descendant_milliseconds: Option<OsString>,
) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .arg("signal-aware-sleep")
        .arg(descendant_milliseconds.ok_or_else(|| "missing descendant duration".to_string())?)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
extern "C" fn report_terminate(_: libc::c_int) {
    unsafe {
        libc::write(libc::STDOUT_FILENO, b"T".as_ptr().cast(), 1);
    }
}

#[cfg(target_os = "macos")]
fn signal_aware_sleep(milliseconds: u64) -> Result<(), String> {
    if unsafe {
        libc::signal(
            libc::SIGTERM,
            report_terminate as *const () as libc::sighandler_t,
        )
    } == libc::SIG_ERR
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    std::io::stdout()
        .write_all(&std::process::id().to_be_bytes())
        .and_then(|()| std::io::stdout().write_all(b"C"))
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    std::thread::sleep(Duration::from_millis(milliseconds));
    Ok(())
}

fn ready_then_wait() -> Result<(), String> {
    std::io::stdout()
        .write_all(b"R")
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    std::io::stdin()
        .read_to_end(&mut Vec::new())
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(unix)]
fn echo_then_sleep(milliseconds: u64) -> Result<(), String> {
    let mut byte = [0_u8; 1];
    std::io::stdin()
        .read_exact(&mut byte)
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .write_all(&byte)
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    std::thread::sleep(Duration::from_millis(milliseconds));
    Ok(())
}

#[cfg(unix)]
fn signal(signal: i32) -> Result<(), String> {
    if unsafe { libc::raise(signal) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn signal_disposition(signal: i32) -> Result<(), String> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    if unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let ignored = unsafe { action.assume_init() }.sa_sigaction == libc::SIG_IGN;
    std::io::stdout()
        .write_all(&[u8::from(ignored)])
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn pty_runner_launcher(mut arguments: impl Iterator<Item = OsString>) -> Result<(), String> {
    use std::os::fd::FromRawFd;

    let runner = arguments
        .next()
        .ok_or_else(|| "missing private runner path".to_string())?;
    let pid_descriptor = parse_i32(arguments.next());
    let release_descriptor = parse_i32(arguments.next());
    let control_descriptor = parse_i32(arguments.next());
    if arguments.next().as_deref() != Some(OsStr::new("--")) {
        return Err("missing private runner argument separator".to_string());
    }
    for descriptor in [pid_descriptor, release_descriptor] {
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags == -1
            || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    let mut child = Command::new(runner)
        .args(arguments)
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut pid_writer = unsafe { std::fs::File::from_raw_fd(pid_descriptor) };
    pid_writer
        .write_all(&child.id().to_be_bytes())
        .and_then(|()| pid_writer.flush())
        .map_err(|error| error.to_string())?;
    drop(pid_writer);
    unsafe { libc::close(control_descriptor) };
    let status = child.wait().map_err(|error| error.to_string())?;
    let mut release = unsafe { std::fs::File::from_raw_fd(release_descriptor) };
    release
        .read_exact(&mut [0_u8; 1])
        .map_err(|error| error.to_string())?;
    if let Some(signal) = status.signal()
        && unsafe { libc::raise(signal) } == -1
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    std::process::exit(status.code().unwrap_or(125));
}

fn unicode(value: Option<OsString>, name: &str) -> Result<String, String> {
    value
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| format!("missing or non-Unicode {name}"))
}

fn parse_usize(value: Option<OsString>) -> usize {
    unicode(value, "integer")
        .and_then(|value| {
            value
                .parse()
                .map_err(|error| format!("invalid integer: {error}"))
        })
        .expect("integer argument")
}

fn parse_u64(value: Option<OsString>) -> u64 {
    unicode(value, "integer")
        .and_then(|value| {
            value
                .parse()
                .map_err(|error| format!("invalid integer: {error}"))
        })
        .expect("integer argument")
}

fn parse_i32(value: Option<OsString>) -> i32 {
    unicode(value, "integer")
        .and_then(|value| {
            value
                .parse()
                .map_err(|error| format!("invalid integer: {error}"))
        })
        .expect("integer argument")
}

#[cfg(unix)]
fn native_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn native_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn native_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}
