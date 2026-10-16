//! The questions libuv asks of a Windows path where std's answers differ
//! from node's: a stat's verdict on a path std could not stat, a path's
//! attributes (access, chmod), and a directory handle's first listing
//! (src/win/fs.c, v1.51.0, the libuv node v22.22.2 bundles).
//!
//! std stats through GetFileInformationByHandle, whose
//! ERROR_INVALID_FUNCTION on any device reads as EISDIR, and opens the path
//! where libuv first asks for it by name. libuv's answers are what node
//! reports; conformance cases 540, 541 and 393 hold them.

use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{NTSTATUS, RtlNtStatusToDosError};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// FILE_DEVICE_NULL (winioctl.h): the device type NUL reports.
pub(crate) const FILE_DEVICE_NULL: u32 = 0x15;

/// What libuv's stat makes of a path (fs__stat_impl_from_path) when std's
/// stat of it failed.
#[derive(Debug)]
pub(crate) enum StatVerdict {
    /// NUL, which libuv gives a fixed character device's stat.
    NullDevice,
    /// libuv's stat fails, with this error.
    Failed(std::io::Error),
    /// libuv's questions all answered: std's failure stands.
    Unexplained,
}

/// FILE_STAT_BASIC_INFORMATION (winbase.h, Windows 11 24H2 SDK), which
/// GetFileInformationByName fills for FileStatBasicByNameInfo. windows-sys
/// 0.61 does not declare it.
#[repr(C)]
#[derive(Default)]
struct FileStatBasicInformation {
    file_id: i64,
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    allocation_size: i64,
    end_of_file: i64,
    file_attributes: u32,
    reparse_tag: u32,
    number_of_links: u32,
    device_type: u32,
    device_characteristics: u32,
    reserved: u32,
    volume_serial_number: i64,
    file_id_128: [u8; 16],
}

/// FILE_INFO_BY_NAME_CLASS's FileStatBasicByNameInfo.
const FILE_STAT_BASIC_BY_NAME_INFO: i32 = 3;

type GetFileInformationByName =
    unsafe extern "system" fn(*const u16, i32, *mut core::ffi::c_void, u32) -> i32;

/// GetFileInformationByName, where this Windows has it (11 24H2 and later),
/// found as libuv finds it (src/win/winapi.c): in the API set
/// `api-ms-win-core-file-l2-1-4.dll`, which the loader has already mapped
/// when it exists.
fn get_file_information_by_name() -> Option<GetFileInformationByName> {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    static FOUND: OnceLock<Option<GetFileInformationByName>> = OnceLock::new();
    *FOUND.get_or_init(|| {
        let module: Vec<u16> = "api-ms-win-core-file-l2-1-4.dll\0".encode_utf16().collect();
        // SAFETY: `module` is a NUL-terminated UTF-16 name that outlives the
        // call; GetModuleHandleW takes no reference on what it returns.
        let handle = unsafe { GetModuleHandleW(module.as_ptr()) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: `handle` is a module the loader has mapped and the name is a
        // NUL-terminated ANSI string.
        let found = unsafe { GetProcAddress(handle, c"GetFileInformationByName".as_ptr().cast()) }?;
        // SAFETY: the export has exactly this signature (BOOL WINAPI
        // GetFileInformationByName(PCWSTR, FILE_INFO_BY_NAME_CLASS, PVOID,
        // ULONG)); a function pointer of one ABI is transmuted to another
        // of the same size.
        Some(unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, GetFileInformationByName>(
                found,
            )
        })
    })
}

fn wide(path: &str) -> Vec<u16> {
    std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// NT_ERROR: a status of the error severity (warnings, such as the buffer
/// overflow FileAllInformation answers with, are not failures).
fn nt_error(status: NTSTATUS) -> bool {
    (status as u32) >> 30 == 3
}

fn nt_failure(status: NTSTATUS) -> std::io::Error {
    // SAFETY: RtlNtStatusToDosError reads only its argument.
    std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

/// libuv's stat of `path` (fs__stat_impl_from_path), asked once std's stat
/// of it has failed:
///
/// - GetFileInformationByName, where Windows has it. Not found, path not
///   found, not ready and a bad net name are libuv's answer as they are;
///   that is why a path continuing past a looping link is ENOENT, where the
///   open std makes fails ERROR_CANT_RESOLVE_FILENAME (ELOOP). A device
///   that answers FILE_DEVICE_NULL is NUL. Any other failure, and a reparse
///   point, go on to:
/// - a handle (FILE_READ_ATTRIBUTES, backup semantics, the link itself for
///   lstat), its device type (NtQueryVolumeInformationFile) -- NUL again --
///   and FileAllInformation (NtQueryInformationFile), whose failure on any
///   other device (the pipe and mailslot namespaces: EINVAL) is the stat's.
///
/// Measured on node v22.22.2 (conformance cases 540 and 541).
pub(crate) fn stat_verdict(path: &str, lstat: bool) -> StatVerdict {
    use windows_sys::Win32::Foundation::{
        ERROR_BAD_NET_NAME, ERROR_FILE_NOT_FOUND, ERROR_NOT_READY, ERROR_PATH_NOT_FOUND,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if let Some(by_name) = get_file_information_by_name() {
        let name = wide(path);
        let mut info = FileStatBasicInformation::default();
        // SAFETY: `name` is NUL-terminated and outlives the call; `info` is a
        // writable FILE_STAT_BASIC_INFORMATION of exactly the size passed,
        // the struct the call fills for FileStatBasicByNameInfo.
        let ok = unsafe {
            by_name(
                name.as_ptr(),
                FILE_STAT_BASIC_BY_NAME_INFO,
                (&mut info as *mut FileStatBasicInformation).cast(),
                std::mem::size_of::<FileStatBasicInformation>() as u32,
            )
        };
        if ok == 0 {
            let failed = std::io::Error::last_os_error();
            let final_answer = failed.raw_os_error().is_some_and(|code| {
                [
                    ERROR_FILE_NOT_FOUND,
                    ERROR_PATH_NOT_FOUND,
                    ERROR_NOT_READY,
                    ERROR_BAD_NET_NAME,
                ]
                .iter()
                .any(|&known| code == known as i32)
            });
            if final_answer {
                return StatVerdict::Failed(failed);
            }
        } else if !(lstat && info.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0) {
            return if info.device_type == FILE_DEVICE_NULL {
                StatVerdict::NullDevice
            } else {
                StatVerdict::Unexplained
            };
        }
    }
    let file = match crate::ops::open_for_stat(path, lstat) {
        Ok(file) => file,
        Err(failed) => return StatVerdict::Failed(failed),
    };
    match device_type(&file) {
        Err(failed) => return StatVerdict::Failed(failed),
        Ok(FILE_DEVICE_NULL) => return StatVerdict::NullDevice,
        Ok(_) => {}
    }
    match all_information(&file) {
        Err(failed) => StatVerdict::Failed(failed),
        Ok(()) => StatVerdict::Unexplained,
    }
}

/// A handle's device type (FILE_FS_DEVICE_INFORMATION.DeviceType).
fn device_type(file: &std::fs::File) -> std::io::Result<u32> {
    use windows_sys::Wdk::Storage::FileSystem::{
        FileFsDeviceInformation, NtQueryVolumeInformationFile,
    };
    // FILE_FS_DEVICE_INFORMATION (ntddk.h), declared here rather than
    // enabling a windows-sys feature for one two-field struct.
    #[repr(C)]
    #[derive(Default)]
    struct DeviceInformation {
        device_type: u32,
        characteristics: u32,
    }
    let mut io = IO_STATUS_BLOCK::default();
    let mut info = DeviceInformation::default();
    // SAFETY: `file` owns its handle for the whole call; `io` and `info` are
    // writable and `info` is exactly the size passed, the struct
    // FileFsDeviceInformation fills.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            file.as_raw_handle().cast(),
            &mut io,
            (&mut info as *mut DeviceInformation).cast(),
            std::mem::size_of::<DeviceInformation>() as u32,
            FileFsDeviceInformation,
        )
    };
    if nt_error(status) {
        return Err(nt_failure(status));
    }
    Ok(info.device_type)
}

/// Whether a handle answers FileAllInformation, as libuv's stat asks it.
fn all_information(file: &std::fs::File) -> std::io::Result<()> {
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_ALL_INFORMATION, FileAllInformation, NtQueryInformationFile,
    };
    let mut io = IO_STATUS_BLOCK::default();
    let mut info = FILE_ALL_INFORMATION::default();
    // SAFETY: `file` owns its handle for the whole call; `io` and `info` are
    // writable and `info` is exactly the size passed. The name at its end
    // does not fit, which the call reports as a warning, not an error.
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle().cast(),
            &mut io,
            (&mut info as *mut FILE_ALL_INFORMATION).cast(),
            std::mem::size_of::<FILE_ALL_INFORMATION>() as u32,
            FileAllInformation,
        )
    };
    if nt_error(status) {
        return Err(nt_failure(status));
    }
    Ok(())
}

/// GetFileAttributesW of `path`: the one question libuv's access asks, and
/// the CRT's _wchmod (libuv's chmod) asks before it sets them. It reads a
/// link's own attributes -- a dangling or looping link has them -- and a
/// device refuses it ERROR_INVALID_PARAMETER, a share root that is no disk
/// ERROR_NOT_SUPPORTED.
pub(crate) fn file_attributes(path: &str) -> std::io::Result<u32> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let attributes = unsafe { GetFileAttributesW(name.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(std::io::Error::last_os_error());
    }
    Ok(attributes)
}

/// libuv's chmod (the CRT's _wchmod): the read-only attribute set when
/// `mode` lacks the owner's write bit and cleared when it has it, every
/// other attribute kept, on the path itself -- a link, not its target.
pub(crate) fn chmod(path: &str, mode: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_READONLY, SetFileAttributesW};
    let attributes = file_attributes(path)?;
    let attributes = if mode & 0o200 != 0 {
        attributes & !FILE_ATTRIBUTE_READONLY
    } else {
        attributes | FILE_ATTRIBUTE_READONLY
    };
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    if unsafe { SetFileAttributesW(name.as_ptr(), attributes) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// libuv's scandir's first listing of a directory handle
/// (NtQueryDirectoryFile, FileDirectoryInformation, into an 8 KiB buffer)
/// and what it makes of the answer: STATUS_INVALID_PARAMETER -- a handle
/// that is no directory -- is ENOTDIR; any other failure is reported as
/// it is (a device: KsecDD's STATUS_INVALID_DEVICE_REQUEST, NUL's too, is
/// EISDIR); entries, or none, pass.
pub(crate) fn directory_query(file: &std::fs::File) -> std::io::Result<()> {
    use windows_sys::Wdk::Storage::FileSystem::{FileDirectoryInformation, NtQueryDirectoryFile};
    use windows_sys::Win32::Foundation::{STATUS_INVALID_PARAMETER, STATUS_NO_MORE_FILES};
    let mut io = IO_STATUS_BLOCK::default();
    // u64s for the alignment FILE_DIRECTORY_INFORMATION's fields need.
    let mut buffer = [0u64; 1024];
    // SAFETY: `file` owns its handle, opened for synchronous I/O (no
    // FILE_FLAG_OVERLAPPED), so the call completes before it returns and
    // writes nothing after; `io` and `buffer` are writable, `buffer` of
    // exactly the length passed. No event, APC or file name is given.
    let status = unsafe {
        NtQueryDirectoryFile(
            file.as_raw_handle().cast(),
            std::ptr::null_mut(),
            None,
            std::ptr::null(),
            &mut io,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(&buffer) as u32,
            FileDirectoryInformation,
            false,
            std::ptr::null(),
            false,
        )
    };
    // NT_SUCCESS, or the warning that there is nothing (more) to list.
    if status >= 0 || status == STATUS_NO_MORE_FILES {
        return Ok(());
    }
    if status == STATUS_INVALID_PARAMETER {
        return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
    }
    Err(nt_failure(status))
}
