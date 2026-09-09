use anyhow::Context;
use anyhow::Result;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::process::Stdio;

pub fn run(operation: &str) -> Result<()> {
    if operation == "group-tree" {
        println!(
            "{}",
            serde_json::json!({"pid": std::process::id(), "temporary": std::env::var("TMPDIR")?})
        );
        std::io::stdout().flush()?;
        std::io::stdin().read_exact(&mut [0])?;
        // Retain all standard streams, including an unfinished framed output.
        let child = Command::new("/bin/sleep").arg("600").spawn()?;
        println!("{}", child.id());
        print!("{{\"partial\":");
        std::io::stdout().flush()?;
        let mut status = [0];
        std::io::stdin().read_exact(&mut status)?;
        std::process::exit(i32::from(status[0]));
    }
    if operation == "signals" {
        for signal in [
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGCHLD,
            libc::SIGUSR2,
            #[cfg(target_os = "linux")]
            libc::SIGRTMAX(),
        ] {
            let mut action = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) },
                0
            );
            assert_eq!(action.sa_sigaction, libc::SIG_IGN, "signal {signal}");
        }
        let mut mask = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask) },
            0
        );
        assert_eq!(unsafe { libc::sigismember(&mask, libc::SIGUSR1) }, 1);
        std::process::exit(42);
    }
    if operation == "descendant" {
        assert_ne!(unsafe { libc::setsid() }, -1);
        println!("ready");
        std::io::stdout().flush()?;
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    let temporary = std::path::PathBuf::from(std::env::var("TMPDIR").context("private TMPDIR")?);
    if matches!(operation, "detached" | "interrupted" | "normal-tree") {
        let mut child = Command::new(std::env::current_exe()?)
            .args(["lifecycle", "descendant"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut ready = [0; 6];
        child
            .stdout
            .take()
            .context("descendant stdout")?
            .read_exact(&mut ready)?;
        assert_eq!(&ready, b"ready\n");
        let mut mask = unsafe { std::mem::zeroed() };
        if operation == "interrupted" {
            unsafe {
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, libc::SIGINT);
                libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
            }
        }
        println!(
            "{}",
            serde_json::json!({"temporary": temporary, "descendant": child.id(), "pid": std::process::id()})
        );
        std::io::stdout().flush()?;
        if operation == "interrupted" {
            let mut signal = 0;
            assert_eq!(unsafe { libc::sigwait(&mask, &mut signal) }, 0);
            assert_eq!(signal, libc::SIGINT);
            std::process::exit(42);
        }
        std::io::stdin().read_exact(&mut [0])?;
        if operation == "normal-tree" {
            std::process::exit(42);
        }
        return Ok(());
    }
    println!("{}", temporary.display());
    std::fs::write(temporary.join("ordinary-data"), b"temporary")?;
    match operation {
        "success" => {}
        "nonzero" => std::process::exit(42),
        "replace" => {
            std::fs::remove_dir_all(&temporary)?;
            std::fs::create_dir(&temporary)?;
        }
        "mode-zero" => {
            let nested = temporary.join("nested");
            std::fs::create_dir(&nested)?;
            std::fs::set_permissions(nested, std::fs::Permissions::from_mode(0o000))?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o000))?;
        }
        "symlink" => std::os::unix::fs::symlink("/", temporary.join("host"))?,
        _ => anyhow::bail!("unknown lifecycle operation: {operation}"),
    }
    Ok(())
}
