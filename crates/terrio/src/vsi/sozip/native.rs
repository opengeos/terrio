//! Native archive snapshots. File identity must not be inferred from timestamps:
//! distinct files can have identical lengths, modification times and creation times.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

#[derive(Clone)]
pub(super) struct NativeSnapshot {
    metadata: Metadata,
    #[cfg(windows)]
    identity: WindowsIdentity,
}

impl NativeSnapshot {
    /// Capture metadata and identity from the actual retained reader handle.
    pub(super) fn capture(file: &File) -> io::Result<Self> {
        Ok(Self {
            metadata: file.metadata()?,
            #[cfg(windows)]
            identity: windows_identity(file)?,
        })
    }

    pub(super) fn matches_file(&self, file: &File) -> io::Result<bool> {
        let unchanged = same_metadata(&self.metadata, &file.metadata()?);
        #[cfg(windows)]
        let unchanged = unchanged && self.identity == windows_identity(file)?;
        Ok(unchanged)
    }

    pub(super) fn matches_path(&self, path: &Path) -> io::Result<bool> {
        #[cfg(windows)]
        {
            // Query the file currently named by the path, not the retained old handle.
            self.matches_file(&File::open(path)?)
        }
        #[cfg(not(windows))]
        {
            Ok(same_metadata(&self.metadata, &std::fs::metadata(path)?))
        }
    }
}

fn same_metadata(before: &Metadata, after: &Metadata) -> bool {
    let unchanged =
        before.len() == after.len() && before.modified().ok() == after.modified().ok() && before.created().ok() == after.created().ok();
    #[cfg(unix)]
    let unchanged = {
        use std::os::unix::fs::MetadataExt;
        unchanged
            && (before.dev(), before.ino(), before.ctime(), before.ctime_nsec())
                == (after.dev(), after.ino(), after.ctime(), after.ctime_nsec())
    };
    unchanged
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsIdentity {
    Full { volume: u64, id: [u8; 16] },
    Legacy { volume: u32, index: u64 },
}

#[cfg(windows)]
fn windows_identity(file: &File) -> io::Result<WindowsIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    };
    let mut information = FILE_ID_INFO::default();
    // SAFETY: the file owns a live handle and the writable buffer has the exact
    // layout and size required by FileIdInfo. No pointer is retained by the call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0
    {
        return Ok(WindowsIdentity::Full { volume: information.VolumeSerialNumber, id: information.FileId.Identifier });
    }
    let error = io::Error::last_os_error();
    if !error
        .raw_os_error()
        .is_some_and(|code| [ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED].contains(&(code as u32)))
    {
        return Err(error);
    }
    // Older filesystem providers may not implement 128-bit IDs. The legacy API
    // still supplies volume/file identity; do not fall back to timestamps alone.
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a live owned handle and a correctly sized writable output buffer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(WindowsIdentity::Legacy {
        volume: information.dwVolumeSerialNumber,
        index: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}
