use anyhow::Context;
use anyhow::Result;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::process::Stdio;

pub fn peer(mut arguments: impl Iterator<Item = String>) -> Result<()> {
    let runner = arguments.next().context("runner executable")?;
    let child = Command::new("/bin/sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    println!("{}", child.id());
    std::io::stdout().flush()?;
    Err(Command::new(runner).args(arguments).exec().into())
}

pub fn target(kind: &str) -> Result<()> {
    let mut mask = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGINT);
    }
    let mut previous = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut previous) },
        0
    );
    assert_eq!(unsafe { libc::sigismember(&previous, libc::SIGINT) }, 0);
    println!(
        "{}",
        serde_json::json!({"group": unsafe { libc::getpgrp() }, "pid": std::process::id()})
    );
    std::io::stdout().flush()?;
    if kind == "exclusive" {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        assert_eq!(line, "terminal input\n");
        print!("{line}");
        std::io::stdout().flush()?;
    }
    let mut signal = 0;
    assert_eq!(unsafe { libc::sigwait(&mask, &mut signal) }, 0);
    assert_eq!(signal, libc::SIGINT);
    println!("1");
    std::process::exit(42);
}
