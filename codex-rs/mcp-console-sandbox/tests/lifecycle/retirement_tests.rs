use super::*;
use pretty_assertions::assert_eq;
use std::io::BufRead;
use std::io::BufReader;

#[test]
fn root_exit_retires_unobserved_group_and_closes_partial_output() {
    for status in [23, 0] {
        let directory = tempfile::tempdir().unwrap();
        let mut request = fixture("lifecycle", &["group-tree"]);
        request["lifecycle"] = json!({"private_tmp": {"environment": ["TMPDIR"]}});
        let mut config = request.clone();
        for key in ["command", "cwd", "environment"] {
            config.as_object_mut().unwrap().remove(key);
        }
        let mut command = runner(directory.path());
        command
            .args(["--config-env", "SANDBOX_TEST_CONFIG", "--"])
            .args(
                request["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap()),
            )
            .env("SANDBOX_TEST_CONFIG", config.to_string())
            .stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        drop(command);
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        // Freeze discovery before the target creates its child. The original
        // group remains owned even if the parent exits before any fork event
        // can be processed. Linux covers this with its PID namespace instead.
        #[cfg(target_os = "macos")]
        let queue = {
            use std::os::fd::FromRawFd;
            let queue = unsafe { libc::kqueue() };
            assert!(queue >= 0);
            let queue = unsafe { std::os::fd::OwnedFd::from_raw_fd(queue) };
            let event = libc::kevent {
                ident: ready["pid"].as_u64().unwrap() as usize,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            assert_eq!(
                unsafe {
                    libc::kevent(
                        queue.as_raw_fd(),
                        &event,
                        1,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null(),
                    )
                },
                0
            );
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGSTOP) }, 0);
            let mut stopped = 0;
            assert_eq!(
                unsafe { libc::waitpid(child.id() as i32, &mut stopped, libc::WUNTRACED) },
                child.id() as i32
            );
            assert!(libc::WIFSTOPPED(stopped));
            queue
        };
        let mut unrelated = Command::new("/bin/sleep").arg("600").spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"x").unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let _descendant: i32 = line.trim().parse().unwrap();
        stdin.write_all(&[status]).unwrap();
        #[cfg(target_os = "macos")]
        {
            let mut event = unsafe { std::mem::zeroed() };
            let timeout = libc::timespec {
                tv_sec: 10,
                tv_nsec: 0,
            };
            let count = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &timeout,
                )
            };
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGCONT) }, 0);
            assert_eq!(count, 1);
        }
        let result = child.wait().unwrap();
        // The runner's exit is the barrier. Do not wait for leaked descendants
        // to close the streams before checking whether retirement succeeded.
        let mut fds = [
            libc::pollfd {
                fd: stdout.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: child.stderr.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        assert!(unsafe { libc::poll(fds.as_mut_ptr(), 2, 0) } >= 0);
        let closed = fds.iter().all(|fd| fd.revents & libc::POLLHUP != 0);
        let unrelated_alive = unrelated.try_wait().unwrap().is_none();
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        // Failed implementations must not leave the fixture behind.
        #[cfg(target_os = "macos")]
        if !closed {
            unsafe {
                libc::kill(_descendant, libc::SIGKILL);
            }
        }
        assert!(
            closed,
            "runner returned {result} while descendant {_descendant} retained streams; temporary exists: {}",
            Path::new(ready["temporary"].as_str().unwrap()).exists()
        );
        assert!(unrelated_alive);
        let mut tail = String::new();
        stdout.read_to_string(&mut tail).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            (result.code(), tail, output.stderr),
            (Some(i32::from(status)), "{\"partial\":".to_owned(), vec![])
        );
        assert!(!Path::new(ready["temporary"].as_str().unwrap()).exists());
    }
}
