use crate::desktop::DESKTOP_ALL_ACCESS;
use crate::token::get_logon_sid_bytes;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::ptr;
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::Foundation::GENERIC_EXECUTE;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GRANT_ACCESS;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::REVOKE_ACCESS;
use windows_sys::Win32::Security::Authorization::SE_WINDOW_OBJECT;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::CONTAINER_INHERIT_ACE;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::INHERIT_ONLY_ACE;
use windows_sys::Win32::Security::OBJECT_INHERIT_ACE;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::StationsAndDesktops::CloseDesktop;
use windows_sys::Win32::System::StationsAndDesktops::CloseWindowStation;
use windows_sys::Win32::System::StationsAndDesktops::GetProcessWindowStation;
use windows_sys::Win32::System::StationsAndDesktops::OpenDesktopW;
use windows_sys::Win32::System::StationsAndDesktops::OpenWindowStationW;
use windows_sys::Win32::System::StationsAndDesktops::SetProcessWindowStation;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_ACCESSCLIPBOARD;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_ACCESSGLOBALATOMS;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_CREATEDESKTOP;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_ENUMDESKTOPS;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_ENUMERATE;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_EXITWINDOWS;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_READATTRIBUTES;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_READSCREEN;
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_WRITEATTRIBUTES;

const READ_CONTROL_ACCESS: u32 = 0x0002_0000;
const WRITE_DAC_ACCESS: u32 = 0x0004_0000;
const STANDARD_RIGHTS_REQUIRED: u32 = 0x000f_0000;
const WINSTA_ALL_ACCESS: u32 = WINSTA_ENUMDESKTOPS as u32
    | WINSTA_READATTRIBUTES as u32
    | WINSTA_ACCESSCLIPBOARD as u32
    | WINSTA_CREATEDESKTOP as u32
    | WINSTA_WRITEATTRIBUTES as u32
    | WINSTA_ACCESSGLOBALATOMS as u32
    | WINSTA_EXITWINDOWS as u32
    | WINSTA_ENUMERATE as u32
    | WINSTA_READSCREEN as u32
    | STANDARD_RIGHTS_REQUIRED;
const GENERIC_ACCESS: u32 = GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL;

#[derive(Clone, Copy)]
pub(super) enum StandaloneDesktop {
    Default,
    Private,
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

/// Temporary logon-session ACEs needed by `CreateProcessAsUserW` targets.
///
/// The target token retains only this session SID plus policy capability SIDs.
/// Granting the unique logon SID to the selected Windows objects avoids adding
/// `Everyone` or the persistent sandbox account to the restricting SID set.
pub(super) struct WindowAccessLease {
    station: isize,
    desktop: isize,
    logon_sid: Vec<u8>,
    station_granted: bool,
    desktop_granted: bool,
}

impl WindowAccessLease {
    pub(super) fn prepare(helper_process: HANDLE, desktop: StandaloneDesktop) -> Result<Self> {
        let logon_sid = helper_logon_sid(helper_process)?;
        let station_name = to_wide("Winsta0");
        let station = unsafe {
            OpenWindowStationW(
                station_name.as_ptr(),
                /*finherit*/ 0,
                READ_CONTROL_ACCESS | WRITE_DAC_ACCESS,
            )
        };
        if station == 0 {
            return Err(std::io::Error::last_os_error())
                .context("open Winsta0 for standalone logon-session access");
        }

        let desktop_handle = match desktop {
            StandaloneDesktop::Default => match open_default_desktop(station) {
                Ok(desktop) => desktop,
                Err(error) => {
                    unsafe {
                        CloseWindowStation(station);
                    }
                    return Err(error);
                }
            },
            StandaloneDesktop::Private => 0,
        };
        let mut lease = Self {
            station,
            desktop: desktop_handle,
            logon_sid,
            station_granted: false,
            desktop_granted: false,
        };
        grant_station_access(lease.station, lease.logon_sid.as_mut_ptr().cast())?;
        lease.station_granted = true;
        if lease.desktop != 0 {
            grant_access(
                lease.desktop,
                lease.logon_sid.as_mut_ptr().cast(),
                DESKTOP_ALL_ACCESS,
                /*inheritance*/ 0,
                "Default desktop",
            )?;
            lease.desktop_granted = true;
        }
        Ok(lease)
    }

    pub(super) fn release(mut self) -> Result<()> {
        let result = self.revoke();
        self.close_handles();
        result
    }

    fn revoke(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        if self.desktop_granted {
            if let Err(error) = revoke_access(
                self.desktop,
                self.logon_sid.as_mut_ptr().cast(),
                "Default desktop",
            ) {
                errors.push(format!("{error:#}"));
            }
            self.desktop_granted = false;
        }
        if self.station_granted {
            if let Err(error) =
                revoke_access(self.station, self.logon_sid.as_mut_ptr().cast(), "Winsta0")
            {
                errors.push(format!("{error:#}"));
            }
            self.station_granted = false;
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(errors.join("; "))
        }
    }

    fn close_handles(&mut self) {
        unsafe {
            if self.desktop != 0 {
                CloseDesktop(self.desktop);
                self.desktop = 0;
            }
            if self.station != 0 {
                CloseWindowStation(self.station);
                self.station = 0;
            }
        }
    }
}

impl Drop for WindowAccessLease {
    fn drop(&mut self) {
        if let Err(error) = self.revoke() {
            eprintln!("standalone Windows station cleanup failed: {error:#}");
        }
        self.close_handles();
    }
}

fn helper_logon_sid(helper_process: HANDLE) -> Result<Vec<u8>> {
    let mut token = 0;
    if unsafe { OpenProcessToken(helper_process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("open standalone helper token for logon SID");
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };
    unsafe { get_logon_sid_bytes(token.as_raw_handle() as HANDLE) }
        .context("read standalone helper logon SID")
}

fn open_default_desktop(station: isize) -> Result<isize> {
    let previous_station = unsafe { GetProcessWindowStation() };
    if previous_station == 0 {
        return Err(std::io::Error::last_os_error()).context("read runner process window station");
    }
    if unsafe { SetProcessWindowStation(station) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("select Winsta0 while opening the Default desktop");
    }
    let desktop_name = to_wide("Default");
    let desktop = unsafe {
        OpenDesktopW(
            desktop_name.as_ptr(),
            /*dwflags*/ 0,
            /*finherit*/ 0,
            READ_CONTROL_ACCESS
                | WRITE_DAC_ACCESS
                | windows_sys::Win32::System::StationsAndDesktops::DESKTOP_READOBJECTS
                | windows_sys::Win32::System::StationsAndDesktops::DESKTOP_WRITEOBJECTS,
        )
    };
    let open_error = if desktop == 0 {
        Some(unsafe { GetLastError() })
    } else {
        None
    };
    if unsafe { SetProcessWindowStation(previous_station) } == 0 {
        if desktop != 0 {
            unsafe {
                CloseDesktop(desktop);
            }
        }
        return Err(std::io::Error::last_os_error())
            .context("restore runner process window station");
    }
    if let Some(error) = open_error {
        return Err(std::io::Error::from_raw_os_error(error as i32))
            .context("open Winsta0\\Default for standalone logon-session access");
    }
    Ok(desktop)
}

fn grant_station_access(station: isize, sid: *mut c_void) -> Result<()> {
    let entries = [
        explicit_access(
            sid,
            GENERIC_ACCESS,
            GRANT_ACCESS,
            CONTAINER_INHERIT_ACE | INHERIT_ONLY_ACE | OBJECT_INHERIT_ACE,
        ),
        explicit_access(
            sid,
            WINSTA_ALL_ACCESS,
            GRANT_ACCESS,
            windows_sys::Win32::Security::NO_PROPAGATE_INHERIT_ACE,
        ),
    ];
    update_dacl(station, &entries, "Winsta0")
}

fn grant_access(
    object: isize,
    sid: *mut c_void,
    access: u32,
    inheritance: u32,
    label: &str,
) -> Result<()> {
    update_dacl(
        object,
        &[explicit_access(sid, access, GRANT_ACCESS, inheritance)],
        label,
    )
}

fn revoke_access(object: isize, sid: *mut c_void, label: &str) -> Result<()> {
    update_dacl(
        object,
        &[explicit_access(
            sid,
            0,
            REVOKE_ACCESS,
            /*inheritance*/ 0,
        )],
        label,
    )
}

fn explicit_access(
    sid: *mut c_void,
    access: u32,
    mode: i32,
    inheritance: u32,
) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    }
}

fn update_dacl(object: isize, entries: &[EXPLICIT_ACCESS_W], label: &str) -> Result<()> {
    let mut old_dacl = ptr::null_mut();
    let mut security_descriptor = ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            object,
            SE_WINDOW_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut old_dacl,
            ptr::null_mut(),
            &mut security_descriptor,
        )
    };
    let _security_descriptor = LocalAllocation(security_descriptor.cast());
    if result != 0 {
        anyhow::bail!("GetSecurityInfo failed for {label}: {result}");
    }

    let mut updated_dacl = ptr::null_mut();
    let result = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            old_dacl,
            &mut updated_dacl,
        )
    };
    let _updated_dacl = LocalAllocation(updated_dacl.cast());
    if result != 0 {
        anyhow::bail!("SetEntriesInAclW failed for {label}: {result}");
    }
    let result = unsafe {
        SetSecurityInfo(
            object,
            SE_WINDOW_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            updated_dacl,
            ptr::null_mut(),
        )
    };
    if result != 0 {
        anyhow::bail!("SetSecurityInfo failed for {label}: {result}");
    }
    Ok(())
}
