use anyhow::Context;
use anyhow::Result;
use std::io::Read;
use std::io::Write;

pub fn adversary() -> Result<()> {
    println!("ready");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let request: serde_json::Value = serde_json::from_str(&input)?;
    let supervisor = request["supervisor"].as_i64().context("supervisor PID")? as i32;
    assert_eq!(
        unsafe { libc::kill(supervisor, libc::SIGUSR1) },
        -1,
        "target signalled host supervisor"
    );
    #[cfg(target_os = "linux")]
    {
        assert!(std::fs::read_dir(format!("/proc/{supervisor}/fd")).is_err());
        assert!(std::fs::File::open(format!("/proc/{supervisor}/mem")).is_err());
        assert!(std::fs::File::open(format!("/proc/{supervisor}/environ")).is_err());
        assert_eq!(
            std::fs::read_link("/proc/self")?,
            std::path::PathBuf::from(std::process::id().to_string())
        );
    }
    #[cfg(target_os = "macos")]
    {
        let mut task = 0;
        #[allow(deprecated)]
        let result = unsafe { libc::task_for_pid(libc::mach_task_self(), supervisor, &mut task) };
        assert_ne!(
            result,
            libc::KERN_SUCCESS,
            "target obtained supervisor task port"
        );
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, supervisor];
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut length = buffer.len();
        assert_eq!(
            unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    buffer.as_mut_ptr().cast(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            },
            -1,
            "target read supervisor launch environment"
        );
        let mut peer = std::process::Command::new("/bin/sleep").arg("30").spawn()?;
        mib[2] = peer.id() as i32;
        length = buffer.len();
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        peer.kill()?;
        peer.wait()?;
        assert_eq!(result, 0, "same-sandbox process inspection was denied");
    }
    // Setting a transport-looking variable inside the workload has no effect.
    unsafe {
        std::env::set_var(
            "SANDBOX_TEST_CONFIG",
            r#"{"filesystem":{"type":"unrestricted"}}"#,
        );
    }
    assert!(
        std::fs::write(
            request["forbidden"].as_str().context("forbidden path")?,
            b"escape"
        )
        .is_err()
    );
    for fd in 3..256 {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) },
            -1,
            "inherited control fd {fd}"
        );
    }
    Ok(())
}

pub fn survivor(forbidden: String, address: String) -> Result<()> {
    println!(
        "{}",
        serde_json::json!({"temporary": std::env::var("TMPDIR")?, "pid": std::process::id()})
    );
    std::io::stdout().flush()?;
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    assert!(!input.is_empty());
    assert!(std::fs::write(&forbidden, b"escape").is_err());
    assert!(
        std::net::TcpStream::connect_timeout(&address.parse()?, std::time::Duration::from_secs(1))
            .is_err()
    );
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["write", &forbidden])
        .output()?;
    assert!(!output.status.success());
    println!("still restricted");
    Ok(())
}
