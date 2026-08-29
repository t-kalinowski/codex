use std::io;

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::protocol::StandardStreams;
    use crate::protocol::StreamSpec;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    /// Native stream descriptors explicitly transferred to the runner at bootstrap.
    ///
    /// Claiming them before the async runtime starts prevents protocol values from
    /// aliasing descriptors allocated later for runner infrastructure.
    pub struct PassedStreamEndpoints {
        endpoints: BTreeMap<u64, OwnedFd>,
        control: i32,
        inherited_standard_descriptors: BTreeSet<u64>,
    }

    /// Private launch duplicates of the bootstrap-owned stream descriptors.
    pub struct LaunchStreamEndpoints {
        endpoints: BTreeMap<u64, OwnedFd>,
    }

    pub struct ForegroundTerminal {
        descriptor: OwnedFd,
        original_process_group: libc::pid_t,
        restore_on_drop: bool,
    }

    impl PassedStreamEndpoints {
        pub fn claim(values: &[u64], control: i32) -> io::Result<Self> {
            set_close_on_exec(control).map_err(|error| {
                invalid_input(format!("private control descriptor is invalid: {error}"))
            })?;
            let inherited_standard_descriptors = available_standard_descriptors();
            let mut endpoints = BTreeMap::new();
            for &value in values {
                if endpoints.contains_key(&value) {
                    return Err(invalid_input(
                        "each passed target stream requires a distinct descriptor",
                    ));
                }
                let descriptor = native_descriptor(value)?;
                if [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
                    .contains(&descriptor)
                    || matches_standard(descriptor)?
                {
                    return Err(invalid_input(
                        "a passed target stream descriptor cannot alias a runner standard stream",
                    ));
                }
                if descriptor == control || same_open_file(descriptor, control)? {
                    return Err(invalid_input(
                        "the private control descriptor cannot be used as a target stream",
                    ));
                }
                set_close_on_exec(descriptor).map_err(|error| {
                    invalid_input(format!(
                        "passed target stream descriptor is invalid: {error}"
                    ))
                })?;
                endpoints.insert(value, unsafe { OwnedFd::from_raw_fd(descriptor) });
            }
            Ok(Self {
                endpoints,
                control,
                inherited_standard_descriptors,
            })
        }

        pub fn validate_request(&self, streams: &StandardStreams) -> io::Result<()> {
            validate_inherited_streams(
                streams,
                self.control,
                &self.inherited_standard_descriptors,
            )?;
            let requested = requested_descriptors(streams)?;
            let claimed = self.endpoints.keys().copied().collect::<BTreeSet<_>>();
            if let Some(value) = requested.difference(&claimed).next() {
                return Err(invalid_input(format!(
                    "passed target stream descriptor {value} was not declared at runner bootstrap"
                )));
            }
            if let Some(value) = claimed.difference(&requested).next() {
                return Err(invalid_input(format!(
                    "bootstrap target stream descriptor {value} is unused by the launch request"
                )));
            }
            Ok(())
        }

        pub fn duplicate_for_launch(
            &self,
            streams: &StandardStreams,
        ) -> io::Result<LaunchStreamEndpoints> {
            self.validate_request(streams)?;
            let endpoints = self
                .endpoints
                .iter()
                .map(|(&value, endpoint)| endpoint.try_clone().map(|endpoint| (value, endpoint)))
                .collect::<io::Result<_>>()?;
            Ok(LaunchStreamEndpoints { endpoints })
        }

        pub fn foreground_terminal(
            &self,
            streams: &StandardStreams,
        ) -> io::Result<Option<ForegroundTerminal>> {
            let runner_process_group = unsafe { libc::getpgrp() };
            let launcher_process_group = unsafe { libc::getpgid(libc::getppid()) };
            for (stream, target_descriptor) in standard_streams(streams) {
                let source_descriptor = match stream {
                    StreamSpec::Inherited => target_descriptor,
                    StreamSpec::PassedHandle { handle } => self
                        .endpoints
                        .get(&handle)
                        .map(AsRawFd::as_raw_fd)
                        .ok_or_else(|| {
                            invalid_input("passed target terminal descriptor was not retained")
                        })?,
                    StreamSpec::Null => continue,
                };
                if unsafe { libc::isatty(source_descriptor) } == 0 {
                    continue;
                }
                let foreground_process_group = unsafe { libc::tcgetpgrp(source_descriptor) };
                if foreground_process_group == -1 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ENOTTY) {
                        continue;
                    }
                    return Err(error);
                }
                if foreground_process_group != runner_process_group
                    && foreground_process_group != launcher_process_group
                {
                    continue;
                }
                let descriptor =
                    unsafe { libc::fcntl(source_descriptor, libc::F_DUPFD_CLOEXEC, 3) };
                if descriptor == -1 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(Some(ForegroundTerminal {
                    descriptor: unsafe { OwnedFd::from_raw_fd(descriptor) },
                    original_process_group: foreground_process_group,
                    restore_on_drop: true,
                }));
            }
            Ok(None)
        }

        pub fn release(&mut self) {
            self.endpoints.clear();
        }
    }

    impl LaunchStreamEndpoints {
        pub fn take_fd(&mut self, value: u64) -> Option<OwnedFd> {
            self.endpoints.remove(&value)
        }
    }

    impl ForegroundTerminal {
        pub(crate) fn duplicate_for_lifetime_manager(&self) -> io::Result<OwnedFd> {
            self.descriptor.try_clone()
        }

        pub(crate) unsafe fn from_inherited_descriptor(descriptor: i32) -> io::Result<Self> {
            let original_process_group = unsafe { libc::tcgetpgrp(descriptor) };
            if original_process_group == -1 {
                return Err(io::Error::last_os_error());
            }
            set_close_on_exec(descriptor)?;
            Ok(Self {
                descriptor: unsafe { OwnedFd::from_raw_fd(descriptor) },
                original_process_group,
                restore_on_drop: true,
            })
        }

        pub fn assign(&self, process_group: libc::pid_t) -> io::Result<()> {
            set_foreground_process_group(self.descriptor.as_raw_fd(), process_group)
        }

        pub fn restore(mut self) -> io::Result<()> {
            self.restore_on_drop = false;
            restore_foreground(self.descriptor.as_raw_fd(), self.original_process_group)
        }
    }

    impl Drop for ForegroundTerminal {
        fn drop(&mut self) {
            if self.restore_on_drop {
                let _ =
                    restore_foreground(self.descriptor.as_raw_fd(), self.original_process_group);
            }
        }
    }

    fn restore_foreground(descriptor: i32, process_group: libc::pid_t) -> io::Result<()> {
        match set_foreground_process_group(descriptor, process_group) {
            Err(error) if error.raw_os_error() == Some(libc::ENOTTY) => Ok(()),
            result => result,
        }
    }

    fn set_foreground_process_group(descriptor: i32, process_group: libc::pid_t) -> io::Result<()> {
        let mut signal_set: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut previous_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut signal_set);
            libc::sigaddset(&mut signal_set, libc::SIGTTOU);
        }
        let mask_result =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signal_set, &mut previous_mask) };
        if mask_result != 0 {
            return Err(io::Error::from_raw_os_error(mask_result));
        }
        let terminal_result = unsafe { libc::tcsetpgrp(descriptor, process_group) };
        let terminal_error = io::Error::last_os_error();
        let mask_result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous_mask, std::ptr::null_mut())
        };
        if terminal_result != 0 {
            return Err(terminal_error);
        }
        if mask_result != 0 {
            return Err(io::Error::from_raw_os_error(mask_result));
        }
        Ok(())
    }

    /// Closes the runner copies of standard streams inherited by the target.
    pub fn close_inherited_runner_streams(
        streams: &StandardStreams,
        control: i32,
    ) -> io::Result<()> {
        validate_inherited_control_alias(streams, control)?;
        for (stream, descriptor) in standard_streams(streams) {
            if stream == StreamSpec::Inherited && unsafe { libc::close(descriptor) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn requested_descriptors(streams: &StandardStreams) -> io::Result<BTreeSet<u64>> {
        let mut requested = BTreeSet::new();
        for stream in [streams.stdin, streams.stdout, streams.stderr] {
            let StreamSpec::PassedHandle { handle } = stream else {
                continue;
            };
            if !requested.insert(handle) {
                return Err(invalid_input(
                    "each passed target stream requires a distinct descriptor",
                ));
            }
        }
        Ok(requested)
    }

    fn validate_inherited_streams(
        streams: &StandardStreams,
        control: i32,
        available: &BTreeSet<u64>,
    ) -> io::Result<()> {
        validate_inherited_control_alias(streams, control)?;
        for (stream, descriptor) in standard_streams(streams) {
            if stream == StreamSpec::Inherited && !available.contains(&(descriptor as u64)) {
                return Err(invalid_input(format!(
                    "inherited target standard-stream descriptor {descriptor} was unavailable at runner bootstrap"
                )));
            }
        }
        Ok(())
    }

    fn validate_inherited_control_alias(streams: &StandardStreams, control: i32) -> io::Result<()> {
        for (stream, descriptor) in standard_streams(streams) {
            if stream == StreamSpec::Inherited
                && (descriptor == control || same_open_file(descriptor, control)?)
            {
                return Err(invalid_input(
                    "the private control descriptor cannot be inherited by the target",
                ));
            }
        }
        Ok(())
    }

    fn standard_streams(streams: &StandardStreams) -> [(StreamSpec, i32); 3] {
        [
            (streams.stdin, libc::STDIN_FILENO),
            (streams.stdout, libc::STDOUT_FILENO),
            (streams.stderr, libc::STDERR_FILENO),
        ]
    }

    fn available_standard_descriptors() -> BTreeSet<u64> {
        [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
            .into_iter()
            .filter(|descriptor| unsafe { libc::fcntl(*descriptor, libc::F_GETFD) } != -1)
            .map(|descriptor| descriptor as u64)
            .collect()
    }

    fn matches_standard(descriptor: i32) -> io::Result<bool> {
        for standard in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            if unsafe { libc::fcntl(standard, libc::F_GETFD) } != -1
                && same_open_file(descriptor, standard)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn native_descriptor(value: u64) -> io::Result<i32> {
        i32::try_from(value)
            .map_err(|_| invalid_input("passed target stream descriptor is too large"))
    }

    fn same_open_file(left: i32, right: i32) -> io::Result<bool> {
        let left = descriptor_stat(left)?;
        let right = descriptor_stat(right)?;
        Ok(left.st_mode == right.st_mode
            && left.st_dev == right.st_dev
            && left.st_ino == right.st_ino
            && left.st_rdev == right.st_rdev)
    }

    fn descriptor_stat(descriptor: i32) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(descriptor, stat.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn set_close_on_exec(descriptor: i32) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn invalid_input(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message.into())
    }
}

#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
pub struct PassedStreamEndpoints;

#[cfg(windows)]
impl PassedStreamEndpoints {
    pub fn claim(values: &[u64], control: u64) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
        use windows_sys::Win32::Foundation::SetHandleInformation;

        if !values.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "passed target stream handles are unavailable while Windows launch is deferred",
            ));
        }
        let control = usize::try_from(control).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "control handle is too large")
        })?;
        let control = isize::from_ne_bytes(control.to_ne_bytes());
        if control == 0
            || control == -1
            || unsafe { SetHandleInformation(control, HANDLE_FLAG_INHERIT, 0) } == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "private control handle is invalid: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        Ok(Self)
    }
}
