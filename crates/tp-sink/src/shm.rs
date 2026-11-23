//! Named shared memory, one implementation per OS.

use crate::{Error, Result};

/// A mapped view of a named object. The object outlives the writer process as
/// long as any reader holds it (Windows) or until unlinked (POSIX), which is
/// what lets a restarted writer adopt the same region without readers noticing.
pub(crate) struct Mapping {
    ptr: *mut u8,
    len: usize,
    writable: bool,
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the mapping is plain memory; all cross-thread access to its contents
// goes through atomics or seqlock-validated copies in the callers.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn writable(&self) -> bool {
        self.writable
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION,
        MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE,
        UnmapViewOfFile, VirtualQuery,
    };

    /// SYSTEM and Administrators full control, the creating owner full control,
    /// LocalService (FrameServer) read/write, any authenticated user read. GR and
    /// GW on a section map to SECTION_MAP_READ|SECTION_QUERY and SECTION_MAP_WRITE.
    /// Read-only for everyone else keeps another user from spoofing the camera.
    pub const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;GRGW;;;LS)(A;;GR;;;AU)";

    fn wide(name: &str) -> Result<Vec<u16>> {
        if name.is_empty() || name.contains('\0') {
            return Err(Error::BadName);
        }
        Ok(name.encode_utf16().chain(std::iter::once(0)).collect())
    }

    fn map_whole(handle: HANDLE, writable: bool) -> Result<Mapping> {
        let access = if writable { FILE_MAP_READ | FILE_MAP_WRITE } else { FILE_MAP_READ };
        // SAFETY: handle is a live section handle owned by the caller.
        let view = unsafe { MapViewOfFile(handle, access, 0, 0, 0) };
        if view.Value.is_null() {
            let e = std::io::Error::last_os_error();
            // SAFETY: closing the handle we were given; it is not used again.
            unsafe { CloseHandle(handle) };
            return Err(e.into());
        }
        // Size 0 maps the whole section, so ask the VM manager how big it is.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: info is a valid out pointer of the stated size.
        let got = unsafe {
            VirtualQuery(view.Value, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
        };
        if got == 0 {
            let e = std::io::Error::last_os_error();
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(handle);
            }
            return Err(e.into());
        }
        Ok(Mapping { ptr: view.Value as *mut u8, len: info.RegionSize, writable, handle })
    }

    pub fn create(name: &str, size: usize) -> Result<(Mapping, bool)> {
        let wname = wide(name)?;
        let sddl: Vec<u16> = SDDL.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: sddl is NUL-terminated; sd receives a LocalAlloc'd descriptor.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let size = size as u64;
        // SAFETY: pagefile-backed section; all pointers valid for the call.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &sa,
                PAGE_READWRITE,
                (size >> 32) as u32,
                size as u32,
                wname.as_ptr(),
            )
        };
        let last = unsafe { GetLastError() };
        // SAFETY: sd came from ConvertStringSecurityDescriptor and is no longer needed.
        unsafe { LocalFree(sd) };
        if handle.is_null() {
            return Err(std::io::Error::from_raw_os_error(last as i32).into());
        }
        let existed = last == ERROR_ALREADY_EXISTS;
        let m = map_whole(handle, true)?;
        // An existing section keeps its original size; it cannot be grown while
        // any reader holds it, so the caller has to fit inside it.
        if m.len < size as usize {
            return Err(Error::TooSmall { needed: size as usize, actual: m.len });
        }
        Ok((m, !existed))
    }

    pub fn open(name: &str, writable: bool) -> Result<Mapping> {
        let wname = wide(name)?;
        let access = if writable { FILE_MAP_READ | FILE_MAP_WRITE } else { FILE_MAP_READ };
        // SAFETY: wname is NUL-terminated.
        let handle = unsafe { OpenFileMappingW(access, 0, wname.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        map_whole(handle, writable)
    }

    pub fn unlink(_name: &str) -> Result<()> {
        // Sections vanish with their last handle; there is nothing to remove.
        Ok(())
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: ptr and handle came from MapViewOfFile/CreateFileMapping.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr.cast() });
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::io;

    fn cname(name: &str) -> Result<CString> {
        // POSIX wants exactly one leading slash on Linux. macOS accepts an
        // app-group prefix such as "TEAMID.group.id/name", so only reject NULs.
        if name.is_empty() {
            return Err(Error::BadName);
        }
        CString::new(name).map_err(|_| Error::BadName)
    }

    #[cfg(target_os = "macos")]
    unsafe fn shm_open(name: &CString, flags: libc::c_int) -> libc::c_int {
        // Variadic on macOS, so the mode is promoted to c_uint.
        unsafe { libc::shm_open(name.as_ptr(), flags, 0o644 as libc::c_uint) }
    }

    #[cfg(not(target_os = "macos"))]
    unsafe fn shm_open(name: &CString, flags: libc::c_int) -> libc::c_int {
        unsafe { libc::shm_open(name.as_ptr(), flags, 0o644 as libc::mode_t) }
    }

    fn fd_size(fd: libc::c_int) -> io::Result<usize> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is open, st is a valid out pointer.
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st.st_size as usize)
    }

    fn map_fd(fd: libc::c_int, len: usize, writable: bool) -> Result<Mapping> {
        let prot = if writable { libc::PROT_READ | libc::PROT_WRITE } else { libc::PROT_READ };
        // SAFETY: fd is open and at least len bytes long.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0) };
        let err = io::Error::last_os_error();
        // The mapping keeps the object alive; the descriptor is not needed.
        unsafe { libc::close(fd) };
        if p == libc::MAP_FAILED {
            return Err(err.into());
        }
        Ok(Mapping { ptr: p as *mut u8, len, writable })
    }

    pub fn create(name: &str, size: usize) -> Result<(Mapping, bool)> {
        let c = cname(name)?;
        // SAFETY: c is NUL-terminated.
        let mut fd = unsafe { shm_open(&c, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR) };
        let mut created = true;
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(e.into());
            }
            created = false;
            fd = unsafe { shm_open(&c, libc::O_RDWR) };
            if fd < 0 {
                return Err(io::Error::last_os_error().into());
            }
            let cur = fd_size(fd)?;
            if cur < size {
                // Readers holding the old object keep their mapping and notice the
                // stale heartbeat, then reopen by name and find this one.
                unsafe {
                    libc::close(fd);
                    libc::shm_unlink(c.as_ptr());
                }
                fd = unsafe { shm_open(&c, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR) };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                created = true;
            }
        }
        if created {
            // umask may have stripped group/other read, which coreaudiod needs.
            unsafe { libc::fchmod(fd, 0o644) };
            if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
                let e = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(e.into());
            }
        }
        let len = fd_size(fd)?;
        Ok((map_fd(fd, len, true)?, created))
    }

    pub fn open(name: &str, writable: bool) -> Result<Mapping> {
        let c = cname(name)?;
        let flags = if writable { libc::O_RDWR } else { libc::O_RDONLY };
        let fd = unsafe { shm_open(&c, flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let len = fd_size(fd)?;
        if len == 0 {
            unsafe { libc::close(fd) };
            return Err(Error::NotReady);
        }
        map_fd(fd, len, writable)
    }

    pub fn unlink(name: &str) -> Result<()> {
        let c = cname(name)?;
        if unsafe { libc::shm_unlink(c.as_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ENOENT) {
                return Err(e.into());
            }
        }
        Ok(())
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: ptr/len came from mmap.
            unsafe { libc::munmap(self.ptr.cast(), self.len) };
        }
    }
}

/// Create the object or adopt an existing one. Returns true when newly created.
pub(crate) fn create(name: &str, size: usize) -> Result<(Mapping, bool)> {
    imp::create(name, size)
}

/// Open an existing object, read-write if `writable`.
pub(crate) fn open(name: &str, writable: bool) -> Result<Mapping> {
    imp::open(name, writable)
}

/// Open read-write if permitted, otherwise read-only. Readers only need write
/// access to publish their cursor, which is diagnostic.
pub(crate) fn open_prefer_rw(name: &str) -> Result<Mapping> {
    match open(name, true) {
        Ok(m) => Ok(m),
        Err(Error::Io(_)) => open(name, false),
        Err(e) => Err(e),
    }
}

/// Remove the name. On Windows this is a no-op because sections are reference
/// counted by handle.
pub fn unlink(name: &str) -> Result<()> {
    imp::unlink(name)
}

#[cfg(windows)]
pub use imp::SDDL;
