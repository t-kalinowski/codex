#![allow(clippy::expect_used)]

#[cfg(windows)]
use serde_json::Value;
#[cfg(windows)]
use serde_json::json;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

fn main() {
    #[cfg(unix)]
    if codex_mcp_console_sandbox::watchdog::dispatch_if_requested() {
        return;
    }
    #[cfg(windows)]
    if dispatch_ready_loss_helper_if_requested() {
        return;
    }

    let mut arguments = std::env::args_os().skip(1);
    let operation = arguments.next().expect("fixture operation");
    let result = match operation.to_str() {
        Some("argv") => write_native_values(arguments.collect()),
        Some("environment") => write_environment(arguments.collect()),
        Some("environment-entries") => write_environment_entries(arguments.collect()),
        Some("cwd") => write_native_values(vec![
            std::env::current_dir()
                .expect("current directory")
                .into_os_string(),
        ]),
        Some("copy") => copy_streams(),
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
        #[cfg(windows)]
        Some("attempt-windows-job-breakaway") => attempt_windows_job_breakaway(),
        #[cfg(windows)]
        Some("assert-windows-helper-control-denied") => assert_windows_helper_control_denied(),
        #[cfg(windows)]
        Some("assert-windows-helper-token-unavailable") => {
            assert_windows_helper_token_unavailable()
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        Some("spawn-reporting-descendant") => spawn_reporting_descendant(arguments.next()),
        #[cfg(all(unix, not(target_os = "linux")))]
        Some("spawn-ignore-term-descendant") => spawn_ignore_term_descendant(arguments.next()),
        #[cfg(target_os = "linux")]
        Some("spawn-marked-descendant") => {
            spawn_marked_descendant(arguments.next(), arguments.next())
        }
        #[cfg(target_os = "linux")]
        Some("marked-sleep") => marked_sleep(arguments.next(), arguments.next()),
        #[cfg(all(unix, not(target_os = "linux")))]
        Some("report-host-process-id-and-sleep") => {
            report_host_process_id_and_sleep(parse_u64(arguments.next()))
        }
        #[cfg(target_os = "linux")]
        Some("spawn-session-escaping-descendant") => {
            spawn_session_escaping_descendant(arguments.next())
        }
        #[cfg(unix)]
        Some("spawn-echoing-descendant") => spawn_echoing_descendant(arguments.next()),
        #[cfg(unix)]
        Some("echo-then-sleep") => echo_then_sleep(parse_u64(arguments.next())),
        #[cfg(unix)]
        Some("signal") => signal(parse_i32(arguments.next())),
        #[cfg(unix)]
        Some("ignore-term") => ignore_term(parse_u64(arguments.next())),
        #[cfg(unix)]
        Some("watchdog-reserved-fd") => watchdog_reserved_fd(),
        #[cfg(unix)]
        Some("watchdog-disarm") => watchdog_disarm(),
        _ => Err(format!("unknown fixture operation: {operation:?}")),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(73);
    }
}

#[cfg(windows)]
fn dispatch_ready_loss_helper_if_requested() -> bool {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if let Some(response) =
        codex_windows_sandbox::windows_sandbox_standalone_helper_compatibility_response(
            codex_windows_sandbox::WindowsSandboxStandaloneHelperKind::CommandRunner,
            &arguments,
        )
    {
        dispatch_test_compatibility_behavior(&response);
        return true;
    }
    if arguments.len() != 3
        || arguments[0] != "--standalone-sandbox-runner"
        || !arguments[1]
            .to_str()
            .is_some_and(|argument| argument.starts_with("--pipe-in="))
        || !arguments[2]
            .to_str()
            .is_some_and(|argument| argument.starts_with("--pipe-out="))
    {
        return false;
    }
    let behavior = std::env::current_exe().ok().and_then(|executable| {
        std::fs::read_to_string(executable.with_extension("mcp-console-test-behavior")).ok()
    });
    let result = if behavior.as_deref() == Some("final_holding") {
        send_final_then_hold(&arguments[1], &arguments[2])
    } else {
        consume_spawn_without_ready(&arguments[1], &arguments[2])
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(73);
    }
    true
}

#[cfg(windows)]
fn dispatch_test_compatibility_behavior(response: &str) {
    let executable = std::env::current_exe().expect("fixture executable");
    let behavior_path = executable.with_extension("mcp-console-test-behavior");
    let Ok(behavior) = std::fs::read_to_string(behavior_path) else {
        print!("{response}");
        return;
    };
    match behavior.as_str() {
        "timeout" => std::thread::sleep(Duration::from_secs(30)),
        "noisy_output" => {
            print!("{response}unexpected stdout");
            eprintln!("unexpected stderr");
        }
        "oversized_output" => std::io::stdout()
            .write_all(&vec![b'x'; 1025])
            .expect("write oversized compatibility output"),
        "pipe_holding_descendant" => {
            let mut descendant = Command::new(&executable)
                .arg("sleep")
                .arg("30000")
                .spawn()
                .expect("spawn pipe-holding compatibility descendant");
            std::fs::write(
                executable.with_extension("mcp-console-test-descendant-pid"),
                descendant.id().to_string(),
            )
            .expect("write compatibility descendant process ID");
            std::thread::spawn(move || {
                let _ = descendant.wait();
            });
            print!("{response}");
        }
        "final_holding" => print!("{response}"),
        behavior => panic!("unknown compatibility test behavior: {behavior}"),
    }
}

#[cfg(windows)]
fn send_final_then_hold(pipe_in: &OsStr, pipe_out: &OsStr) -> Result<(), String> {
    let mut reader = open_test_helper_pipe(pipe_in, "--pipe-in=", /*read*/ true)?;
    let mut writer = open_test_helper_pipe(pipe_out, "--pipe-out=", /*read*/ false)?;
    let spawn = read_test_helper_frame(&mut reader)?;
    if spawn["message"]["type"] != "spawn" {
        return Err("expected standalone spawn request".to_string());
    }
    write_test_helper_frame(
        &mut writer,
        json!({
            "type": "ready",
            "payload": { "process_id": std::process::id() },
        }),
    )?;
    let commit = read_test_helper_frame(&mut reader)?;
    if commit["message"]["type"] != "commit_launch" {
        return Err("expected standalone launch commit".to_string());
    }
    write_test_helper_frame(&mut writer, json!({ "type": "committed" }))?;
    let target = json!({ "exited": { "code": 41 } });
    write_test_helper_frame(
        &mut writer,
        json!({ "type": "root_exited", "payload": target }),
    )?;
    write_test_helper_frame(
        &mut writer,
        json!({
            "type": "final",
            "payload": {
                "target": target,
                "retirement": {
                    "complete": true,
                    "forced": false,
                    "error": null,
                },
                "infrastructure_error": null,
                "cleanup_error": null,
            },
        }),
    )?;
    let mut remaining = Vec::new();
    reader
        .read_to_end(&mut remaining)
        .map_err(|error| format!("wait for test helper control loss: {error}"))?;
    std::thread::sleep(Duration::from_secs(30));
    Ok(())
}

#[cfg(windows)]
fn open_test_helper_pipe(
    argument: &OsStr,
    prefix: &str,
    read: bool,
) -> Result<std::fs::File, String> {
    let pipe = argument
        .to_str()
        .and_then(|argument| argument.strip_prefix(prefix))
        .ok_or_else(|| format!("missing test helper pipe argument {prefix}"))?;
    let mut options = std::fs::OpenOptions::new();
    options.read(read).write(!read);
    options
        .open(pipe)
        .map_err(|error| format!("open test helper pipe: {error}"))
}

#[cfg(windows)]
fn read_test_helper_frame(reader: &mut std::fs::File) -> Result<Value, String> {
    const MAX_PRIVATE_FRAME_SIZE: usize = 1024 * 1024;

    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length)
        .map_err(|error| format!("read test helper frame length: {error}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_PRIVATE_FRAME_SIZE {
        return Err("test helper frame is oversized".to_string());
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("read test helper frame payload: {error}"))?;
    serde_json::from_slice(&payload).map_err(|error| format!("parse test helper frame: {error}"))
}

#[cfg(windows)]
fn write_test_helper_frame(writer: &mut std::fs::File, message: Value) -> Result<(), String> {
    let payload = serde_json::to_vec(&json!({ "version": 1, "message": message }))
        .map_err(|error| format!("serialize test helper frame: {error}"))?;
    let length = u32::try_from(payload.len()).map_err(|_| "test helper frame is too large")?;
    writer
        .write_all(&length.to_le_bytes())
        .and_then(|()| writer.write_all(&payload))
        .and_then(|()| writer.flush())
        .map_err(|error| format!("write test helper frame: {error}"))
}

#[cfg(windows)]
fn consume_spawn_without_ready(pipe_in: &OsStr, pipe_out: &OsStr) -> Result<(), String> {
    const MAX_PRIVATE_FRAME_SIZE: usize = 1024 * 1024;

    let pipe_in = pipe_in
        .to_str()
        .and_then(|argument| argument.strip_prefix("--pipe-in="))
        .ok_or_else(|| "missing helper input pipe".to_string())?;
    let pipe_out = pipe_out
        .to_str()
        .and_then(|argument| argument.strip_prefix("--pipe-out="))
        .ok_or_else(|| "missing helper output pipe".to_string())?;
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .open(pipe_in)
        .map_err(|error| format!("open helper input pipe: {error}"))?;
    let _writer = std::fs::OpenOptions::new()
        .write(true)
        .open(pipe_out)
        .map_err(|error| format!("open helper output pipe: {error}"))?;
    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length)
        .map_err(|error| format!("read helper spawn length: {error}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_PRIVATE_FRAME_SIZE {
        return Err("helper spawn frame is oversized".to_string());
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("read helper spawn payload: {error}"))?;
    Ok(())
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

fn write_environment_entries(keys: Vec<OsString>) -> Result<(), String> {
    let entries = std::env::vars_os()
        .filter(|(key, _)| {
            keys.iter()
                .any(|requested| environment_keys_equal(key, requested))
        })
        .flat_map(|(key, value)| [key, value])
        .collect();
    write_native_values(entries)
}

#[cfg(windows)]
fn environment_keys_equal(left: &OsStr, right: &OsStr) -> bool {
    left.to_str()
        .zip(right.to_str())
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
}

#[cfg(not(windows))]
fn environment_keys_equal(left: &OsStr, right: &OsStr) -> bool {
    left == right
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

#[cfg(all(unix, not(target_os = "linux")))]
fn spawn_ignore_term_descendant(milliseconds: Option<OsString>) -> Result<(), String> {
    let milliseconds = milliseconds.ok_or_else(|| "missing duration".to_string())?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Command::new(executable)
        .arg("ignore-term")
        .arg(&milliseconds)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    std::thread::sleep(Duration::from_millis(parse_u64(Some(milliseconds))));
    Ok(())
}

#[cfg(windows)]
fn attempt_windows_job_breakaway() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;

    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    match Command::new(executable)
        .arg("sleep")
        .arg("30000")
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(_) => Ok(()),
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            Err("sandbox target escaped its non-breakaway Job Object".to_string())
        }
    }
}

#[cfg(windows)]
fn assert_windows_helper_control_denied() -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_CREATE_PROCESS;
    use windows_sys::Win32::System::Threading::PROCESS_DUP_HANDLE;
    use windows_sys::Win32::System::Threading::PROCESS_SET_INFORMATION;
    use windows_sys::Win32::System::Threading::PROCESS_SUSPEND_RESUME;
    use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
    use windows_sys::Win32::System::Threading::PROCESS_VM_OPERATION;
    use windows_sys::Win32::System::Threading::PROCESS_VM_WRITE;

    let helper_process_id = windows_parent_process_id()?;
    for (label, access) in [
        ("duplicate handles", PROCESS_DUP_HANDLE),
        ("create child processes", PROCESS_CREATE_PROCESS),
        ("terminate", PROCESS_TERMINATE),
        ("change process state", PROCESS_SET_INFORMATION),
        ("suspend or resume", PROCESS_SUSPEND_RESUME),
        (
            "write process memory",
            PROCESS_VM_OPERATION | PROCESS_VM_WRITE,
        ),
    ] {
        let handle = unsafe {
            OpenProcess(access, /*b_inherit_handle*/ 0, helper_process_id)
        };
        if handle != 0 {
            unsafe { CloseHandle(handle) };
            return Err(format!(
                "sandbox target could {label} through its standalone helper process"
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
            return Err(format!(
                "opening the standalone helper to {label} failed with unexpected error: {error}"
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn assert_windows_helper_token_unavailable() -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::DuplicateToken;
    use windows_sys::Win32::Security::ImpersonateLoggedOnUser;
    use windows_sys::Win32::Security::RevertToSelf;
    use windows_sys::Win32::Security::SecurityImpersonation;
    use windows_sys::Win32::Security::TOKEN_DUPLICATE;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPTHREAD;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32First;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32Next;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    use windows_sys::Win32::System::Threading::OpenThread;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    use windows_sys::Win32::System::Threading::THREAD_DIRECT_IMPERSONATION;
    use windows_sys::Win32::System::Threading::THREAD_IMPERSONATE;
    use windows_sys::Win32::System::Threading::THREAD_SET_CONTEXT;

    let helper_process_id = windows_parent_process_id()?;
    let helper_process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            /*b_inherit_handle*/ 0,
            helper_process_id,
        )
    };
    if helper_process == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "create helper thread snapshot: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut helper_thread_count = 0;
            if unsafe { Thread32First(snapshot, &mut entry) } != 0 {
                loop {
                    if entry.th32OwnerProcessID == helper_process_id {
                        helper_thread_count += 1;
                        for (label, access) in [
                            ("impersonate", THREAD_IMPERSONATE),
                            (
                                "supply a direct impersonation context",
                                THREAD_DIRECT_IMPERSONATION,
                            ),
                            ("rewrite its DACL", WRITE_DAC),
                            ("set its execution context", THREAD_SET_CONTEXT),
                        ] {
                            let thread = unsafe {
                                OpenThread(access, /*b_inherit_handle*/ 0, entry.th32ThreadID)
                            };
                            if thread != 0 {
                                unsafe { CloseHandle(thread) };
                                unsafe { CloseHandle(snapshot) };
                                return Err(format!(
                                    "sandbox target could {label} through standalone helper thread {}",
                                    entry.th32ThreadID
                                ));
                            }
                            let error = std::io::Error::last_os_error();
                            if error.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
                                unsafe { CloseHandle(snapshot) };
                                return Err(format!(
                                    "opening standalone helper thread {} to {label} failed with unexpected error: {error}",
                                    entry.th32ThreadID
                                ));
                            }
                        }
                    }
                    if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
                        break;
                    }
                }
            }
            unsafe { CloseHandle(snapshot) };
            if helper_thread_count == 0 {
                return Err("standalone helper has no observable threads".to_string());
            }
            return Ok(());
        }
        return Err(format!(
            "opening the standalone helper for token query failed with unexpected error: {error}"
        ));
    }

    let mut helper_token = 0;
    let opened_token = unsafe {
        OpenProcessToken(
            helper_process,
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut helper_token,
        )
    };
    unsafe { CloseHandle(helper_process) };
    if opened_token == 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "sandbox target could query its standalone helper process; opening its token failed: {error}"
        ));
    }

    let mut duplicate = 0;
    let duplicated = unsafe { DuplicateToken(helper_token, SecurityImpersonation, &mut duplicate) };
    if duplicated == 0 {
        let error = std::io::Error::last_os_error();
        unsafe { CloseHandle(helper_token) };
        return Err(format!(
            "sandbox target obtained its standalone helper token; duplicating it failed: {error}"
        ));
    }
    let impersonated = unsafe { ImpersonateLoggedOnUser(duplicate) };
    let impersonation_error = if impersonated == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    if impersonated != 0 {
        unsafe {
            let _ = RevertToSelf();
        }
    }
    unsafe {
        CloseHandle(duplicate);
        CloseHandle(helper_token);
    }
    if let Some(error) = impersonation_error {
        return Err(format!(
            "sandbox target duplicated its standalone helper token; impersonating it failed: {error}"
        ));
    }
    Err("sandbox target impersonated its unrestricted standalone helper token".to_string())
}

#[cfg(windows)]
fn windows_parent_process_id() -> Result<u32, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32NextW;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(format!(
            "create process snapshot: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = None;
    if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
        loop {
            if entry.th32ProcessID == unsafe { GetCurrentProcessId() } {
                found = Some(entry.th32ParentProcessID);
                break;
            }
            if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snapshot) };
    found.ok_or_else(|| "current process is missing from the process snapshot".to_string())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn spawn_reporting_descendant(milliseconds: Option<OsString>) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Command::new(executable)
        .arg("report-host-process-id-and-sleep")
        .arg(milliseconds.ok_or_else(|| "missing duration".to_string())?)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn report_host_process_id_and_sleep(milliseconds: u64) -> Result<(), String> {
    let process_id = host_process_id()?;
    std::io::stdout()
        .write_all(&process_id.to_be_bytes())
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())?;
    std::thread::sleep(Duration::from_millis(milliseconds));
    Ok(())
}

#[cfg(target_os = "linux")]
fn spawn_marked_descendant(
    marker: Option<OsString>,
    milliseconds: Option<OsString>,
) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Command::new(executable)
        .arg("marked-sleep")
        .arg(marker.ok_or_else(|| "missing descendant marker".to_string())?)
        .arg(milliseconds.ok_or_else(|| "missing duration".to_string())?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .write_all(b"R")
        .and_then(|()| std::io::stdout().flush())
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
fn marked_sleep(marker: Option<OsString>, milliseconds: Option<OsString>) -> Result<(), String> {
    if marker.is_none_or(|marker| marker.is_empty()) {
        return Err("missing descendant marker".to_string());
    }
    std::thread::sleep(Duration::from_millis(parse_u64(milliseconds)));
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn host_process_id() -> Result<u32, String> {
    u32::try_from(unsafe { libc::getpid() }).map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
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
fn spawn_echoing_descendant(milliseconds: Option<OsString>) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Command::new(executable)
        .arg("echo-then-sleep")
        .arg(milliseconds.ok_or_else(|| "missing duration".to_string())?)
        .spawn()
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

#[cfg(unix)]
fn ignore_term(milliseconds: u64) -> Result<(), String> {
    if unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error().to_string());
    }
    std::io::stdout()
        .write_all(b"R")
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    std::thread::sleep(Duration::from_millis(milliseconds));
    Ok(())
}

#[cfg(unix)]
fn watchdog_reserved_fd() -> Result<(), String> {
    use codex_mcp_console_sandbox::watchdog::ProcessGroupWatchdog;
    use std::os::unix::process::CommandExt;

    const WATCHDOG_OWNER_FD: i32 = 197;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut target = Command::new(executable)
        .arg("sleep")
        .arg("30000")
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    let target_process_group = target.id();

    let watchdog_result = (|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let null_fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        if null_fd == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        for fd in libc::STDERR_FILENO + 1..WATCHDOG_OWNER_FD {
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EBADF) {
                    return Err(error.to_string());
                }
                if unsafe { libc::dup2(null_fd, fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
        }
        unsafe {
            libc::close(WATCHDOG_OWNER_FD);
        }

        let watchdog = runtime
            .block_on(ProcessGroupWatchdog::start(target_process_group))
            .map_err(|error| error.to_string())?;
        codex_utils_pty::process_group::kill_process_group(target_process_group)
            .map_err(|error| error.to_string())?;
        target.wait().map_err(|error| error.to_string())?;
        runtime
            .block_on(watchdog.disarm())
            .map_err(|error| error.to_string())
    })();
    if let Err(error) = watchdog_result {
        let _ = codex_utils_pty::process_group::kill_process_group(target_process_group);
        let _ = target.wait();
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn watchdog_disarm() -> Result<(), String> {
    use codex_mcp_console_sandbox::watchdog::ProcessGroupWatchdog;
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut target = Command::new(executable)
        .arg("copy")
        .process_group(0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let target_process_group = target.id();

    let result = (|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let watchdog = runtime
            .block_on(ProcessGroupWatchdog::start(target_process_group))
            .map_err(|error| error.to_string())?;
        runtime
            .block_on(watchdog.disarm())
            .map_err(|error| error.to_string())?;

        let mut target_stdin = target
            .stdin
            .take()
            .ok_or_else(|| "watchdog target stdin is missing".to_string())?;
        target_stdin
            .write_all(b"D")
            .map_err(|error| error.to_string())?;
        drop(target_stdin);
        let mut response = [0_u8; 1];
        target
            .stdout
            .take()
            .ok_or_else(|| "watchdog target stdout is missing".to_string())?
            .read_exact(&mut response)
            .map_err(|error| error.to_string())?;
        if response != *b"D" {
            return Err(format!("watchdog target returned {response:?}"));
        }
        let status = target.wait().map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!("watchdog target exited with {status}"));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = codex_utils_pty::process_group::kill_process_group(target_process_group);
        let _ = target.wait();
    }
    result
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
