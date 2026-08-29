#![cfg(target_os = "macos")]

use crate::process::ProcessIdentity;
use crate::process::list_child_pids;
use crate::process::process_identity;
use crate::process::process_info;
use crate::process::signal_process;
use crate::protocol::LifecyclePolicy;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use std::time::Instant;

const EVENT_WAIT_INTERVAL: Duration = Duration::from_millis(10);
const TRACKER_EVENT_CAPACITY: usize = 32;
#[allow(deprecated)]
const PROCESS_REAP_EVENT: u32 = libc::NOTE_REAP;

pub(crate) enum TrackerCommand {
    Retire,
    Terminate { graceful: Duration, force: Duration },
}

pub(crate) struct TrackerOutcome {
    pub(crate) forced: bool,
}

pub(crate) struct DescendantTracker {
    kqueue: OwnedFd,
    state: TrackerState,
}

struct TrackerState {
    root: ProcessIdentity,
    active: HashMap<libc::pid_t, ProcessIdentity>,
}

enum EventWait {
    Events,
    RootExited,
    TimedOut,
}

impl DescendantTracker {
    pub(crate) fn start(root: ProcessIdentity) -> Result<Self, String> {
        let kqueue_descriptor = unsafe { libc::kqueue() };
        if kqueue_descriptor < 0 {
            return Err(format!(
                "failed to create the sandbox process tracker: {}",
                std::io::Error::last_os_error()
            ));
        }
        let kqueue = unsafe { OwnedFd::from_raw_fd(kqueue_descriptor) };
        let root_pid = root.pid;
        let mut state = TrackerState {
            root,
            active: HashMap::new(),
        };
        add_process_tree(
            kqueue.as_raw_fd(),
            root_pid,
            /*expected_parent*/ None,
            &mut state,
        )?;
        if state.active.get(&root_pid) != Some(&root) {
            return Err(format!(
                "sandbox root {root_pid} changed before descendant tracking"
            ));
        }
        Ok(Self { kqueue, state })
    }

    pub(crate) fn supervise(
        mut self,
        lifecycle: &LifecyclePolicy,
        commands: &Receiver<TrackerCommand>,
    ) -> Result<TrackerOutcome, String> {
        loop {
            if let Some(command) = next_command(commands)? {
                return self.apply_command(command, lifecycle, commands);
            }
            if self.root_has_exited()? {
                break;
            }
            self.wait_for_events(Some(EVENT_WAIT_INTERVAL))?;
        }

        if let Some(command) = self.wait_phase(
            Duration::from_millis(lifecycle.root_exit_grace_ms),
            commands,
        )? {
            return self.apply_command(command, lifecycle, commands);
        }
        self.refresh()?;
        if self.state.active.is_empty() {
            return Ok(TrackerOutcome { forced: false });
        }
        self.signal_active(libc::SIGTERM)?;
        if let Some(command) = self.wait_phase(
            Duration::from_millis(lifecycle.terminate_grace_ms),
            commands,
        )? {
            return self.apply_command(command, lifecycle, commands);
        }
        self.refresh()?;
        if self.state.active.is_empty() {
            return Ok(TrackerOutcome { forced: false });
        }
        self.force_retirement(Duration::from_millis(lifecycle.force_timeout_ms))
    }

    pub(crate) fn stop(self, timeout: Duration) -> Result<TrackerOutcome, String> {
        self.force_retirement(timeout)
    }

    fn wait_phase(
        &mut self,
        duration: Duration,
        commands: &Receiver<TrackerCommand>,
    ) -> Result<Option<TrackerCommand>, String> {
        let deadline = Instant::now() + duration;
        loop {
            if let Some(command) = next_command(commands)? {
                return Ok(Some(command));
            }
            self.refresh()?;
            if self.state.active.is_empty() || Instant::now() >= deadline {
                return Ok(None);
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(EVENT_WAIT_INTERVAL);
            self.wait_for_events(Some(wait))?;
        }
    }

    fn apply_command(
        mut self,
        command: TrackerCommand,
        lifecycle: &LifecyclePolicy,
        commands: &Receiver<TrackerCommand>,
    ) -> Result<TrackerOutcome, String> {
        match command {
            TrackerCommand::Retire => {
                self.force_retirement(Duration::from_millis(lifecycle.force_timeout_ms))
            }
            TrackerCommand::Terminate { graceful, force } => {
                self.refresh()?;
                if self.state.active.is_empty() {
                    return Ok(TrackerOutcome { forced: false });
                }
                self.signal_active(libc::SIGTERM)?;
                if let Some(command) = self.wait_phase(graceful, commands)? {
                    return match command {
                        TrackerCommand::Retire => self.force_retirement(force),
                        command @ TrackerCommand::Terminate { .. } => {
                            self.apply_command(command, lifecycle, commands)
                        }
                    };
                }
                self.refresh()?;
                if self.state.active.is_empty() {
                    Ok(TrackerOutcome { forced: false })
                } else {
                    self.force_retirement(force)
                }
            }
        }
    }

    fn force_retirement(mut self, timeout: Duration) -> Result<TrackerOutcome, String> {
        let deadline = Instant::now() + timeout;
        let mut forced = false;
        loop {
            self.refresh()?;
            if self.state.active.is_empty() {
                return Ok(TrackerOutcome { forced });
            }
            forced = true;
            self.signal_active(libc::SIGKILL)?;
            self.refresh()?;
            if self.state.active.is_empty() {
                return Ok(TrackerOutcome { forced });
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for sandbox descendants to be reaped".to_string());
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(EVENT_WAIT_INTERVAL);
            self.wait_for_events(Some(wait))?;
        }
    }

    fn refresh(&mut self) -> Result<(), String> {
        while matches!(
            self.wait_for_events(Some(Duration::ZERO))?,
            EventWait::Events | EventWait::RootExited
        ) {}
        discover_active_children(self.kqueue.as_raw_fd(), &mut self.state)?;
        if self.root_has_exited()? {
            self.state.active.remove(&self.state.root.pid);
        }
        remove_stale_processes(&mut self.state.active)
    }

    fn signal_active(&self, signal: libc::c_int) -> Result<(), String> {
        for identity in self.state.active.values().copied() {
            signal_process(identity, signal)?;
        }
        Ok(())
    }

    fn root_has_exited(&self) -> Result<bool, String> {
        let Some(info) = process_info(self.state.root.pid)? else {
            return Ok(true);
        };
        Ok(info.identity != self.state.root || info.is_zombie)
    }

    fn wait_for_events(&mut self, timeout: Option<Duration>) -> Result<EventWait, String> {
        let mut events: [libc::kevent; TRACKER_EVENT_CAPACITY] =
            unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
        let timeout = timeout.map(|duration| libc::timespec {
            tv_sec: duration.as_secs() as libc::time_t,
            tv_nsec: duration.subsec_nanos() as libc::c_long,
        });
        let timeout = timeout
            .as_ref()
            .map_or(std::ptr::null(), |timeout| timeout as *const _);
        let event_count = unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                timeout,
            )
        };
        if event_count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(EventWait::Events);
            }
            return Err(format!("sandbox process tracker failed: {error}"));
        }
        if event_count == 0 {
            return Ok(EventWait::TimedOut);
        }
        let mut root_exited = false;
        for event in events.iter().take(event_count as usize) {
            let pid = event.ident as libc::pid_t;
            let event_data = event.data;
            if event.flags & libc::EV_ERROR != 0 {
                if event.filter == libc::EVFILT_PROC && event_data == libc::ESRCH as libc::intptr_t
                {
                    self.state.active.remove(&pid);
                    root_exited |= pid == self.state.root.pid;
                    continue;
                }
                return Err(format!(
                    "sandbox process tracker received event error {event_data}"
                ));
            }
            if event.filter != libc::EVFILT_PROC {
                continue;
            }
            root_exited |= event.fflags & libc::NOTE_EXIT != 0 && pid == self.state.root.pid;
            if event.fflags & PROCESS_REAP_EVENT != 0 {
                if let Some(identity) = self.state.active.get(&pid).copied()
                    && process_identity(pid)? != Some(identity)
                {
                    self.state.active.remove(&pid);
                }
                continue;
            }
            if event.fflags & libc::NOTE_FORK != 0 {
                add_children(self.kqueue.as_raw_fd(), pid, &mut self.state)?;
            }
        }
        Ok(if root_exited {
            EventWait::RootExited
        } else {
            EventWait::Events
        })
    }
}

fn next_command(commands: &Receiver<TrackerCommand>) -> Result<Option<TrackerCommand>, String> {
    match commands.try_recv() {
        Ok(command) => Ok(Some(command)),
        Err(TryRecvError::Disconnected) => Ok(Some(TrackerCommand::Retire)),
        Err(TryRecvError::Empty) => Ok(None),
    }
}

fn remove_stale_processes(
    active: &mut HashMap<libc::pid_t, ProcessIdentity>,
) -> Result<(), String> {
    let identities = active.values().copied().collect::<Vec<_>>();
    for identity in identities {
        if process_identity(identity.pid)? != Some(identity) {
            active.remove(&identity.pid);
        }
    }
    Ok(())
}

fn add_process_tree(
    kqueue: libc::c_int,
    root_pid: libc::pid_t,
    expected_parent: Option<ProcessIdentity>,
    state: &mut TrackerState,
) -> Result<(), String> {
    // NOTE_FORK tells us that a fork happened, but libproc supplies the child identity only when
    // we take the subsequent snapshot. A child that exits and is reparented before that snapshot
    // is outside the observed tree. macOS also has no pidfd-like operation that combines an exact
    // process-identity check with signal delivery, so signal_process has an unavoidable TOCTOU
    // between its final identity check and kill(2).
    let mut queue = VecDeque::from([(root_pid, expected_parent)]);
    let mut visited = HashSet::new();
    while let Some((pid, expected_parent)) = queue.pop_front() {
        let Some(info) = process_info(pid)? else {
            continue;
        };
        if let Some(parent) = expected_parent
            && (info.parent_pid != parent.pid || process_identity(parent.pid)? != Some(parent))
        {
            continue;
        }
        let identity = info.identity;
        if !visited.insert(identity) {
            continue;
        }
        if info.is_zombie {
            state.active.insert(pid, identity);
            continue;
        }
        if state.active.get(&pid) != Some(&identity) {
            state.active.remove(&pid);
            match watch_process(kqueue, pid) {
                Ok(()) => {}
                Err(WatchProcessError::Gone) => continue,
                Err(WatchProcessError::Other(error)) => {
                    return Err(format!("failed to watch sandbox process {pid}: {error}"));
                }
            }
            if process_identity(pid)? != Some(identity) {
                remove_process_watch(kqueue, pid);
                continue;
            }
            state.active.insert(pid, identity);
        }
        queue.extend(
            list_child_pids(pid)?
                .into_iter()
                .map(|child| (child, Some(identity))),
        );
    }
    Ok(())
}

fn add_children(
    kqueue: libc::c_int,
    parent: libc::pid_t,
    state: &mut TrackerState,
) -> Result<(), String> {
    let Some(parent_identity) = state.active.get(&parent).copied() else {
        return Ok(());
    };
    for child in list_child_pids(parent)? {
        add_process_tree(kqueue, child, Some(parent_identity), state)?;
    }
    Ok(())
}

fn discover_active_children(kqueue: libc::c_int, state: &mut TrackerState) -> Result<(), String> {
    let parents = state.active.values().copied().collect::<Vec<_>>();
    for parent in parents {
        let Some(info) = process_info(parent.pid)? else {
            continue;
        };
        if info.identity == parent && !info.is_zombie {
            add_children(kqueue, parent.pid, state)?;
        }
    }
    Ok(())
}

enum WatchProcessError {
    Gone,
    Other(std::io::Error),
}

fn watch_process(kqueue: libc::c_int, pid: libc::pid_t) -> Result<(), WatchProcessError> {
    let event = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: libc::NOTE_FORK | libc::NOTE_EXIT | PROCESS_REAP_EVENT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let result =
        unsafe { libc::kevent(kqueue, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if result >= 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Err(WatchProcessError::Gone)
    } else {
        Err(WatchProcessError::Other(error))
    }
}

fn remove_process_watch(kqueue: libc::c_int, pid: libc::pid_t) {
    let event = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_DELETE,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let _ = unsafe { libc::kevent(kqueue, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
}
