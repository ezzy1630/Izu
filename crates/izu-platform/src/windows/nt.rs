//! Minimal synchronous, borrowed-handle Windows Native API boundary.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DISPOSITION_DELETE, FILE_DISPOSITION_INFORMATION_EX,
    FILE_DISPOSITION_POSIX_SEMANTICS, FILE_LINK_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FileDirectoryInformation, FileDispositionInformationEx, FileLinkInformation,
    FileRenameInformation, NtCreateFile, NtFlushBuffersFile, NtQueryDirectoryFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_NO_MORE_FILES, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE, FileAttributeTagInfo, FlushFileBuffers,
    GetFileInformationByHandleEx, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

#[derive(Clone, Copy)]
pub(super) enum Access {
    Directory,
    DirectoryWritableProbe,
    Read,
    ReadWrite,
    Inspect,
    Delete,
}

fn status_result(status: i32) -> io::Result<()> {
    if status >= 0 {
        Ok(())
    } else {
        // SAFETY: The function consumes an integer NTSTATUS and returns its
        // Win32 error mapping; it does not dereference or retain any pointer.
        let code = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// `name` is either a validated ordinary component with a borrowed parent, or
/// a trusted absolute NT root name with no parent. Handles are not inheritable.
pub(super) fn open(
    parent: Option<&File>,
    name: &[u16],
    access: Access,
    create: bool,
) -> io::Result<File> {
    let length = name
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "NT name exceeds size limit"))?;
    let string = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.map_or(ptr::null_mut(), AsRawHandle::as_raw_handle),
        ObjectName: &string,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let (rights, kind) = match access {
        Access::Directory => (
            FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE,
        ),
        Access::DirectoryWritableProbe => {
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE, FILE_DIRECTORY_FILE)
        }
        Access::Read => (FILE_GENERIC_READ, FILE_NON_DIRECTORY_FILE),
        Access::ReadWrite => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_NON_DIRECTORY_FILE,
        ),
        Access::Inspect => (FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0),
        Access::Delete => (DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0),
    };
    let mut handle: HANDLE = ptr::null_mut();
    let mut completion = IO_STATUS_BLOCK::default();
    // SAFETY: All input structs and the counted UTF-16 slice stay live throughout
    // this synchronous call. Their sizes/layouts are the windows-sys C bindings.
    // Writable output handle/completion storage is initialized and aligned.
    // Parent remains borrowed; no APC/event/EAs or asynchronous completion is
    // requested. Non-inheritable OBJECT_ATTRIBUTES transfers only a newly opened
    // handle on success, which is immediately placed in an owning File below.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            rights,
            &attributes,
            &mut completion,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            if create { FILE_CREATE } else { FILE_OPEN },
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        eprintln!(
            "NtCreateFile failed: status={status:#010x}, parent={}, utf16={name:?}, access={rights:#x}, kind={kind:#x}, create={create}",
            parent.is_some(),
        );
    }
    status_result(status)?;
    // SAFETY: NtCreateFile returned success and transferred one valid owned
    // handle. It has not been wrapped, closed, or shared as an owner elsewhere.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Reopen the held directory itself with an independent native file object.
/// A dot is a Win32 path convention and is not a valid NT child component.
/// The zero-length counted name refers to RootDirectory itself; no pathname
/// reconstruction, duplicate-handle cursor sharing, or fallback is permitted.
pub(super) fn reopen_directory(file: &File, writable_probe: bool) -> io::Result<File> {
    open(
        Some(file),
        &[],
        if writable_probe {
            Access::DirectoryWritableProbe
        } else {
            Access::Directory
        },
        false,
    )
}

pub(super) fn attributes(file: &File) -> io::Result<u32> {
    let mut information = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: The live borrowed handle, selected output class, correctly sized
    // initialized struct and synchronous output lifetime match the API contract.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut information as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(information.FileAttributes)
    }
}

/// Experimental OS barrier only; success is not itself a durable ACK policy.
pub(super) fn flush(file: &File) -> io::Result<()> {
    let mut completion = IO_STATUS_BLOCK::default();
    // SAFETY: The handle remains borrowed, completion storage is writable and
    // aligned, and all our native handles were opened for synchronous I/O.
    let status = unsafe { NtFlushBuffersFile(file.as_raw_handle(), &mut completion) };
    status_result(status)
}

pub(super) fn flush_win32(file: &File) -> io::Result<()> {
    // SAFETY: The caller owns a live borrowed filesystem handle, not a pipe or
    // console. FlushFileBuffers neither takes ownership nor retains pointers.
    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// Rename/link information uses the same C field offsets and union width. A
// fixed buffer avoids dynamically aligned allocations and bounds every name.
#[repr(C)]
struct NameInformation {
    replace: u32,
    root: HANDLE,
    bytes: u32,
    name: [u16; 255],
}

#[repr(C, align(8))]
struct NameBuffer([u8; size_of::<NameInformation>()]);

const _: () = {
    // Check the header offsets against authoritative generated C bindings on
    // each compiled ABI; the fixed trailing array changes only buffer capacity.
    assert!(
        std::mem::offset_of!(NameInformation, root)
            == std::mem::offset_of!(FILE_RENAME_INFORMATION, RootDirectory)
    );
    assert!(
        std::mem::offset_of!(NameInformation, bytes)
            == std::mem::offset_of!(FILE_RENAME_INFORMATION, FileNameLength)
    );
    assert!(
        std::mem::offset_of!(NameInformation, name)
            == std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName)
    );
    assert!(
        std::mem::offset_of!(NameInformation, root)
            == std::mem::offset_of!(FILE_LINK_INFORMATION, RootDirectory)
    );
    assert!(
        std::mem::offset_of!(NameInformation, name)
            == std::mem::offset_of!(FILE_LINK_INFORMATION, FileName)
    );
};

pub(super) fn rename_or_link(
    source: &File,
    parent: &File,
    name: &[u16],
    replace: bool,
    link: bool,
) -> io::Result<()> {
    if name.is_empty() || name.len() > 255 || (link && replace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid NT target name",
        ));
    }
    // Explicit byte storage zeroes every byte including C-layout padding. Never
    // submit a Rust field-initialized struct's unspecified padding to the kernel.
    let mut information = NameBuffer([0; size_of::<NameInformation>()]);
    information.0[..4].copy_from_slice(&u32::from(replace).to_ne_bytes());
    let root_offset = std::mem::offset_of!(NameInformation, root);
    let root = (parent.as_raw_handle() as usize).to_ne_bytes();
    information.0[root_offset..root_offset + root.len()].copy_from_slice(&root);
    let count_offset = std::mem::offset_of!(NameInformation, bytes);
    information.0[count_offset..count_offset + 4]
        .copy_from_slice(&((name.len() * 2) as u32).to_ne_bytes());
    let name_offset = std::mem::offset_of!(NameInformation, name);
    for (position, unit) in name.iter().enumerate() {
        let offset = name_offset + position * 2;
        information.0[offset..offset + 2].copy_from_slice(&unit.to_ne_bytes());
    }
    let mut completion = IO_STATUS_BLOCK::default();
    // SAFETY: Both handles remain borrowed; this C-layout buffer provides the
    // native rename/link header followed by the counted UTF-16 component. All
    // fields/storage are initialized, aligned and live until synchronous return.
    // The target is relative to the held directory; no filesystem path lookup
    // through its former pathname is involved. The API retains no pointers.
    let status = unsafe {
        NtSetInformationFile(
            source.as_raw_handle(),
            &mut completion,
            information.0.as_ptr().cast(),
            information.0.len() as u32,
            if link {
                FileLinkInformation
            } else {
                FileRenameInformation
            },
        )
    };
    if status < 0 {
        eprintln!(
            "NtSetInformationFile name change failed: status={status:#010x}, link={link}, replace={replace}, utf16={name:?}"
        );
    }
    status_result(status)
}

pub(super) fn remove(file: &File) -> io::Result<()> {
    let information = FILE_DISPOSITION_INFORMATION_EX {
        Flags: FILE_DISPOSITION_DELETE | FILE_DISPOSITION_POSIX_SEMANTICS,
    };
    let mut completion = IO_STATUS_BLOCK::default();
    // SAFETY: Live borrowed handle with DELETE access, matching initialized
    // fixed-layout output-class input and completion storage; synchronous call.
    // The operation marks this exact object for deletion without traversing it.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut completion,
            (&information as *const FILE_DISPOSITION_INFORMATION_EX).cast(),
            size_of::<FILE_DISPOSITION_INFORMATION_EX>() as u32,
            FileDispositionInformationEx,
        )
    };
    status_result(status)
}

#[repr(C, align(8))]
struct QueryBuffer([u8; 4096]);

pub(super) struct Entry {
    pub name: Vec<u16>,
    pub attributes: u32,
}

pub(super) fn next_entry(file: &File, restart: bool) -> io::Result<Option<Entry>> {
    let mut buffer = QueryBuffer([0; 4096]);
    let mut completion = IO_STATUS_BLOCK::default();
    // SAFETY: The live synchronous directory handle and writable 8-byte-aligned
    // bounded buffer remain valid until return. No event/APC/context is supplied.
    // ReturnSingleEntry prevents chains of records; we parse only the initialized
    // byte count after success, with all offsets/lengths checked below.
    let status = unsafe {
        NtQueryDirectoryFile(
            file.as_raw_handle(),
            ptr::null_mut(),
            None,
            ptr::null(),
            &mut completion,
            buffer.0.as_mut_ptr().cast(),
            buffer.0.len() as u32,
            FileDirectoryInformation,
            true,
            ptr::null(),
            restart,
        )
    };
    if status == STATUS_NO_MORE_FILES {
        return Ok(None);
    }
    status_result(status)?;
    let received = completion.Information;
    let bytes = buffer
        .0
        .get(..received)
        .ok_or_else(|| invalid_query("invalid directory byte count"))?;
    // FILE_DIRECTORY_INFORMATION: attributes at byte 56, filename length at
    // 60, and UTF-16 filename at 64. Read bytes rather than casting kernel data.
    if bytes.len() < 64 {
        return Err(invalid_query("short directory information"));
    }
    let attributes = u32::from_le_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]);
    let length = u32::from_le_bytes([bytes[60], bytes[61], bytes[62], bytes[63]]) as usize;
    if length == 0 || !length.is_multiple_of(2) || length > 510 || length > bytes.len() - 64 {
        return Err(invalid_query("invalid directory filename length"));
    }
    let mut name = Vec::new();
    name.try_reserve_exact(length / 2)
        .map_err(|error| io::Error::other(error.to_string()))?;
    for unit in bytes[64..64 + length].as_chunks::<2>().0 {
        name.push(u16::from_le_bytes([unit[0], unit[1]]));
    }
    Ok(Some(Entry { name, attributes }))
}

fn invalid_query(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
