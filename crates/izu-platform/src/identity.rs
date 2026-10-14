use std::fs::File;
use std::io;

/// Identity of an open filesystem object on this computer.
///
/// This is an in-memory equality key, not a native-format field, owner identity,
/// or proof of persistence. Keep the object handle alive while relying on it:
/// filesystems may reuse identifiers after an object is deleted. Query failures
/// remain errors; paths, lengths, and timestamps never substitute for identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity(Identity);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Identity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows { volume: u64, file: [u8; 16] },
}

impl FileIdentity {
    pub fn of(file: &File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Self(Identity::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            }))
        }
        #[cfg(windows)]
        {
            windows::query(file)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = file;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "open-file identity is unsupported on this platform",
            ))
        }
    }
}

// Stable Rust's Windows MetadataExt file identifiers are still nightly-only.
// Keep the necessary read-only, handle-borrowing FFI in this one module.
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use super::{File, FileIdentity, Identity, io};
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    pub(super) fn query(file: &File) -> io::Result<FileIdentity> {
        let mut information = MaybeUninit::<FILE_ID_INFO>::uninit();
        let size = u32::try_from(size_of::<FILE_ID_INFO>())
            .map_err(|_| io::Error::other("file identity buffer size exceeds Windows API limit"))?;
        // SAFETY: File owns a live handle and stays borrowed until
        // the call returns. The output pointer is aligned, writable storage for
        // exactly FILE_ID_INFO; FileIdInfo selects that structure and the buffer
        // size matches it. The API neither takes ownership nor retains pointers.
        let success = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                information.as_mut_ptr().cast(),
                size,
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: A successful FileIdInfo query initializes the complete
        // FILE_ID_INFO output. On failure we returned without reading storage.
        let information = unsafe { information.assume_init() };
        Ok(FileIdentity(Identity::Windows {
            volume: information.VolumeSerialNumber,
            file: information.FileId.Identifier,
        }))
    }
}
