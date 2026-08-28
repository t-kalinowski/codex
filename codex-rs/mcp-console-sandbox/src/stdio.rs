use crate::protocol::StandardStreams;
use crate::protocol::StreamSpec;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;

#[cfg(unix)]
pub type ControlEndpoint = i32;
#[cfg(windows)]
pub type ControlEndpoint = u64;
#[cfg(not(any(unix, windows)))]
pub type ControlEndpoint = u64;

#[cfg(unix)]
type OwnedEndpoint = std::os::fd::OwnedFd;
#[cfg(windows)]
type OwnedEndpoint = std::os::windows::io::OwnedHandle;
#[cfg(not(any(unix, windows)))]
type OwnedEndpoint = ();

/// Native stream endpoints explicitly transferred to the runner at bootstrap.
///
/// Claiming the endpoints before the async runtime starts prevents numeric
/// protocol values from aliasing descriptors or handles allocated later for
/// runner infrastructure.
pub struct PassedStreamEndpoints {
    endpoints: BTreeMap<u64, OwnedEndpoint>,
    control: ControlEndpoint,
    #[cfg(unix)]
    inherited_standard_descriptors: BTreeSet<u64>,
    #[cfg(windows)]
    inherited_standard_handles: BTreeMap<u32, OwnedEndpoint>,
}

/// Private launch duplicates of the bootstrap-owned stream endpoints.
pub struct LaunchStreamEndpoints {
    endpoints: BTreeMap<u64, OwnedEndpoint>,
}

impl PassedStreamEndpoints {
    /// Claims bootstrap endpoints and makes the control endpoint private.
    pub fn claim(values: &[u64], control: ControlEndpoint) -> io::Result<Self> {
        imp::make_control_private(control)?;
        let reserved = imp::standard_endpoint_values()?;
        #[cfg(unix)]
        let inherited_standard_descriptors = imp::available_standard_endpoint_values();
        #[cfg(windows)]
        let inherited_standard_handles = imp::snapshot_standard_endpoints()?;
        let mut endpoints = BTreeMap::new();
        for &value in values {
            if endpoints.contains_key(&value) {
                return Err(invalid_input(format!(
                    "each passed target stream requires a distinct {}",
                    imp::ENDPOINT_KIND
                )));
            }
            if reserved.contains(&value) || imp::matches_standard(value)? {
                return Err(invalid_input(format!(
                    "a passed target stream {} cannot alias a runner standard stream",
                    imp::ENDPOINT_KIND
                )));
            }
            if imp::matches_control(value, control)? {
                return Err(invalid_input(format!(
                    "the private control {} cannot be used as a target stream",
                    imp::ENDPOINT_KIND
                )));
            }
            let endpoint = imp::claim_endpoint(value)?;
            endpoints.insert(value, endpoint);
        }
        Ok(Self {
            endpoints,
            control,
            #[cfg(unix)]
            inherited_standard_descriptors,
            #[cfg(windows)]
            inherited_standard_handles,
        })
    }

    /// Requires the launch request to consume exactly the claimed endpoints.
    pub fn validate_request(&self, streams: &StandardStreams) -> io::Result<()> {
        imp::validate_inherited_control_alias(streams, self.control)?;
        #[cfg(unix)]
        imp::validate_inherited_bootstrap(streams, &self.inherited_standard_descriptors)?;
        #[cfg(windows)]
        imp::validate_inherited_bootstrap(streams, &self.inherited_standard_handles)?;
        let requested = requested_endpoints(streams)?;
        let claimed = self.endpoints.keys().copied().collect::<BTreeSet<_>>();
        if let Some(value) = requested.difference(&claimed).next() {
            return Err(invalid_input(format!(
                "passed target stream {} {value} was not declared at runner bootstrap",
                imp::ENDPOINT_KIND
            )));
        }
        if let Some(value) = claimed.difference(&requested).next() {
            return Err(invalid_input(format!(
                "bootstrap target stream {} {value} is unused by the launch request",
                imp::ENDPOINT_KIND
            )));
        }
        Ok(())
    }

    /// Duplicates the endpoints for one native process-creation attempt.
    pub fn duplicate_for_launch(
        &self,
        streams: &StandardStreams,
    ) -> io::Result<LaunchStreamEndpoints> {
        self.validate_request(streams)?;
        let endpoints = self
            .endpoints
            .iter()
            .map(|(&value, endpoint)| imp::try_clone(endpoint).map(|endpoint| (value, endpoint)))
            .collect::<io::Result<_>>()?;
        Ok(LaunchStreamEndpoints { endpoints })
    }

    /// Closes the runner's bootstrap copies after native target creation.
    pub fn release(&mut self) {
        self.endpoints.clear();
        #[cfg(windows)]
        self.inherited_standard_handles.clear();
    }
}

#[cfg(unix)]
impl LaunchStreamEndpoints {
    pub fn take_fd(&mut self, value: u64) -> Option<std::os::fd::OwnedFd> {
        self.endpoints.remove(&value)
    }
}

#[cfg(windows)]
impl LaunchStreamEndpoints {
    pub fn handle(&self, value: u64) -> Option<std::os::windows::io::BorrowedHandle<'_>> {
        use std::os::windows::io::AsHandle;

        self.endpoints.get(&value).map(AsHandle::as_handle)
    }
}

/// Closes the runner's copies of standard streams inherited by the target.
///
/// Native launch duplicates each selected endpoint into the target before this
/// function runs. The resident runner no longer needs its own copy once launch
/// succeeds, and retaining it would delay pipe EOF and terminal retirement.
pub fn close_inherited_runner_streams(
    streams: &StandardStreams,
    control: ControlEndpoint,
) -> io::Result<()> {
    imp::validate_inherited_control_alias(streams, control)?;
    imp::close_inherited_runner_streams(streams)
}

fn requested_endpoints(streams: &StandardStreams) -> io::Result<BTreeSet<u64>> {
    let mut requested = BTreeSet::new();
    for stream in [streams.stdin, streams.stdout, streams.stderr] {
        let StreamSpec::PassedHandle { handle } = stream else {
            continue;
        };
        if !requested.insert(handle) {
            return Err(invalid_input(format!(
                "each passed target stream requires a distinct {}",
                imp::ENDPOINT_KIND
            )));
        }
    }
    Ok(requested)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    pub(super) const ENDPOINT_KIND: &str = "descriptor";

    pub(super) fn make_control_private(control: ControlEndpoint) -> io::Result<()> {
        set_close_on_exec(control).map_err(|error| {
            invalid_input(format!("private control descriptor is invalid: {error}"))
        })
    }

    pub(super) fn standard_endpoint_values() -> io::Result<BTreeSet<u64>> {
        Ok(BTreeSet::from([
            libc::STDIN_FILENO as u64,
            libc::STDOUT_FILENO as u64,
            libc::STDERR_FILENO as u64,
        ]))
    }

    pub(super) fn available_standard_endpoint_values() -> BTreeSet<u64> {
        [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
            .into_iter()
            .filter(|descriptor| descriptor_is_inheritable(*descriptor))
            .map(|descriptor| descriptor as u64)
            .collect()
    }

    pub(super) fn matches_control(value: u64, control: ControlEndpoint) -> io::Result<bool> {
        let descriptor = i32::try_from(value)
            .map_err(|_| invalid_input("passed target stream descriptor is too large"))?;
        Ok(descriptor == control || same_open_file(descriptor, control)?)
    }

    pub(super) fn matches_standard(value: u64) -> io::Result<bool> {
        let descriptor = i32::try_from(value)
            .map_err(|_| invalid_input("passed target stream descriptor is too large"))?;
        for standard in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            if unsafe { libc::fcntl(standard, libc::F_GETFD) } != -1
                && same_open_file(descriptor, standard)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn claim_endpoint(value: u64) -> io::Result<OwnedEndpoint> {
        let descriptor = i32::try_from(value)
            .map_err(|_| invalid_input("passed target stream descriptor is too large"))?;
        set_close_on_exec(descriptor).map_err(|error| {
            invalid_input(format!(
                "passed target stream descriptor is invalid: {error}"
            ))
        })?;
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }

    pub(super) fn try_clone(endpoint: &OwnedEndpoint) -> io::Result<OwnedEndpoint> {
        endpoint.try_clone()
    }

    pub(super) fn validate_inherited_control_alias(
        streams: &StandardStreams,
        control: ControlEndpoint,
    ) -> io::Result<()> {
        for (stream, descriptor) in [
            (streams.stdin, libc::STDIN_FILENO),
            (streams.stdout, libc::STDOUT_FILENO),
            (streams.stderr, libc::STDERR_FILENO),
        ] {
            if stream == StreamSpec::Inherited {
                if descriptor == control || same_open_file(descriptor, control)? {
                    return Err(invalid_input(
                        "the private control descriptor cannot be inherited by the target",
                    ));
                }
                if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } == -1 {
                    return Err(invalid_input(format!(
                        "inherited target standard-stream descriptor {descriptor} is unavailable"
                    )));
                }
                if is_null_device(descriptor)? {
                    return Err(invalid_input(format!(
                        "inherited target standard-stream descriptor {descriptor} is the null device; use null mode"
                    )));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_inherited_bootstrap(
        streams: &StandardStreams,
        available: &BTreeSet<u64>,
    ) -> io::Result<()> {
        for (stream, descriptor) in [
            (streams.stdin, libc::STDIN_FILENO),
            (streams.stdout, libc::STDOUT_FILENO),
            (streams.stderr, libc::STDERR_FILENO),
        ] {
            if stream == StreamSpec::Inherited && !available.contains(&(descriptor as u64)) {
                return Err(invalid_input(format!(
                    "inherited target standard-stream descriptor {descriptor} was unavailable at runner bootstrap"
                )));
            }
        }
        Ok(())
    }

    fn descriptor_is_inheritable(descriptor: i32) -> bool {
        (unsafe { libc::fcntl(descriptor, libc::F_GETFD) }) != -1
            && !is_null_device(descriptor).unwrap_or(true)
    }

    fn is_null_device(descriptor: i32) -> io::Result<bool> {
        let descriptor_stat = descriptor_stat(descriptor)?;
        let mut null_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::stat(c"/dev/null".as_ptr(), null_stat.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let null_stat = unsafe { null_stat.assume_init() };
        Ok(same_stat_identity(&descriptor_stat, &null_stat))
    }

    fn same_open_file(left: i32, right: i32) -> io::Result<bool> {
        let left = descriptor_stat(left)?;
        let right = descriptor_stat(right)?;
        Ok(same_stat_identity(&left, &right))
    }

    fn descriptor_stat(descriptor: i32) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(descriptor, stat.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn same_stat_identity(left: &libc::stat, right: &libc::stat) -> bool {
        left.st_mode == right.st_mode
            && left.st_dev == right.st_dev
            && left.st_ino == right.st_ino
            && left.st_rdev == right.st_rdev
    }

    pub(super) fn close_inherited_runner_streams(streams: &StandardStreams) -> io::Result<()> {
        for (stream, descriptor) in [
            (streams.stdin, libc::STDIN_FILENO),
            (streams.stdout, libc::STDOUT_FILENO),
            (streams.stderr, libc::STDERR_FILENO),
        ] {
            if stream == StreamSpec::Inherited && unsafe { libc::close(descriptor) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
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
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::CompareObjectHandles;
    use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
    use windows_sys::Win32::Foundation::DuplicateHandle;
    use windows_sys::Win32::Foundation::GetHandleInformation;
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Foundation::SetHandleInformation;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
    use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
    use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
    use windows_sys::Win32::System::Console::SetStdHandle;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) const ENDPOINT_KIND: &str = "handle";

    pub(super) fn make_control_private(control: ControlEndpoint) -> io::Result<()> {
        let control = native_handle(control)?;
        make_private(control)
            .map_err(|error| invalid_input(format!("private control handle is invalid: {error}")))
    }

    pub(super) fn standard_endpoint_values() -> io::Result<BTreeSet<u64>> {
        let mut values = BTreeSet::new();
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(kind) };
            if handle != 0 && handle != INVALID_HANDLE_VALUE {
                values.insert(handle as usize as u64);
            }
        }
        Ok(values)
    }

    pub(super) fn snapshot_standard_endpoints() -> io::Result<BTreeMap<u32, OwnedEndpoint>> {
        let mut snapshots = BTreeMap::new();
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(kind) };
            if handle == 0 || handle == INVALID_HANDLE_VALUE || !is_valid(handle) {
                continue;
            }
            snapshots.insert(kind, duplicate_private(handle)?);
        }
        Ok(snapshots)
    }

    pub(super) fn matches_control(value: u64, control: ControlEndpoint) -> io::Result<bool> {
        let handle = native_handle(value)?;
        let control = native_handle(control)?;
        Ok(handle == control || unsafe { CompareObjectHandles(handle, control) } != 0)
    }

    pub(super) fn matches_standard(value: u64) -> io::Result<bool> {
        let handle = native_handle(value)?;
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let standard = unsafe { GetStdHandle(kind) };
            if standard != 0
                && standard != INVALID_HANDLE_VALUE
                && unsafe { CompareObjectHandles(handle, standard) } != 0
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn claim_endpoint(value: u64) -> io::Result<OwnedEndpoint> {
        let handle = native_handle(value)?;
        make_private(handle).map_err(|error| {
            invalid_input(format!("passed target stream handle is invalid: {error}"))
        })?;
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as *mut std::ffi::c_void) })
    }

    pub(super) fn try_clone(endpoint: &OwnedEndpoint) -> io::Result<OwnedEndpoint> {
        endpoint.try_clone()
    }

    pub(super) fn validate_inherited_control_alias(
        streams: &StandardStreams,
        control: ControlEndpoint,
    ) -> io::Result<()> {
        let control_handle = native_handle(control)?;
        for (_, handle) in inherited_handles(streams)? {
            if handle == control_handle
                || unsafe { CompareObjectHandles(handle, control_handle) } != 0
            {
                return Err(invalid_input(
                    "the private control handle cannot be inherited by the target",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_inherited_bootstrap(
        streams: &StandardStreams,
        snapshots: &BTreeMap<u32, OwnedEndpoint>,
    ) -> io::Result<()> {
        for (stream, kind) in [
            (streams.stdin, STD_INPUT_HANDLE),
            (streams.stdout, STD_OUTPUT_HANDLE),
            (streams.stderr, STD_ERROR_HANDLE),
        ] {
            if stream != StreamSpec::Inherited {
                continue;
            }
            let current = unsafe { GetStdHandle(kind) };
            let Some(snapshot) = snapshots.get(&kind) else {
                return Err(invalid_input(
                    "an inherited target standard-stream handle was unavailable at runner bootstrap or changed since runner bootstrap",
                ));
            };
            if current == 0
                || current == INVALID_HANDLE_VALUE
                || !is_valid(current)
                || unsafe { CompareObjectHandles(current, snapshot.as_raw_handle() as isize) } == 0
            {
                return Err(invalid_input(
                    "an inherited target standard-stream handle changed since runner bootstrap",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn close_inherited_runner_streams(streams: &StandardStreams) -> io::Result<()> {
        let mut handles = BTreeSet::new();
        for (kind, handle) in inherited_handles(streams)? {
            if unsafe { SetStdHandle(kind, 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            handles.insert(handle);
        }
        for handle in handles {
            if unsafe { CloseHandle(handle) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn inherited_handles(streams: &StandardStreams) -> io::Result<Vec<(u32, isize)>> {
        let mut handles = Vec::new();
        for (stream, kind) in [
            (streams.stdin, STD_INPUT_HANDLE),
            (streams.stdout, STD_OUTPUT_HANDLE),
            (streams.stderr, STD_ERROR_HANDLE),
        ] {
            if stream != StreamSpec::Inherited {
                continue;
            }
            let handle = unsafe { GetStdHandle(kind) };
            if handle == 0 || handle == INVALID_HANDLE_VALUE {
                return Err(invalid_input(
                    "an inherited target standard-stream handle is unavailable",
                ));
            }
            handles.push((kind, handle));
        }
        Ok(handles)
    }

    fn native_handle(value: u64) -> io::Result<isize> {
        if value == 0 || value == u64::MAX {
            return Err(invalid_input("native handle value is invalid"));
        }
        let value = usize::try_from(value)
            .map_err(|_| invalid_input("native handle value is too large"))?;
        Ok(isize::from_ne_bytes(value.to_ne_bytes()))
    }

    fn make_private(handle: isize) -> io::Result<()> {
        let mut flags = 0;
        if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn is_valid(handle: isize) -> bool {
        let mut flags = 0;
        (unsafe { GetHandleInformation(handle, &mut flags) }) != 0
    }

    fn duplicate_private(handle: isize) -> io::Result<OwnedHandle> {
        let mut duplicate = 0;
        let current_process = unsafe { GetCurrentProcess() };
        if unsafe {
            DuplicateHandle(
                current_process,
                handle,
                current_process,
                &mut duplicate,
                /*dwdesiredaccess*/ 0,
                /*binherithandle*/ 0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(duplicate as _) })
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::*;

    pub(super) const ENDPOINT_KIND: &str = "endpoint";

    pub(super) fn make_control_private(_control: ControlEndpoint) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn standard_endpoint_values() -> io::Result<BTreeSet<u64>> {
        Ok(BTreeSet::new())
    }

    pub(super) fn matches_control(value: u64, control: ControlEndpoint) -> io::Result<bool> {
        Ok(value == control)
    }

    pub(super) fn matches_standard(_value: u64) -> io::Result<bool> {
        Ok(false)
    }

    pub(super) fn claim_endpoint(_value: u64) -> io::Result<OwnedEndpoint> {
        Err(invalid_input("passed target streams are unsupported"))
    }

    pub(super) fn try_clone(_endpoint: &OwnedEndpoint) -> io::Result<OwnedEndpoint> {
        Ok(())
    }

    pub(super) fn validate_inherited_control_alias(
        _streams: &StandardStreams,
        _control: ControlEndpoint,
    ) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn close_inherited_runner_streams(_streams: &StandardStreams) -> io::Result<()> {
        Ok(())
    }
}
