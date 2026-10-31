// SPDX-FileCopyrightText: 2026 Qiusheng Wu
// SPDX-License-Identifier: MIT
//! Native directory discovery and atomic publication of a validated TOC.
use super::{RpfTocOptions, RpfTocReport, encode, frame, invalid, read_frame};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

struct Counted {
    file: std::fs::File,
    bytes: u64,
}
impl Read for Counted {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(into)?;
        self.bytes += n as u64;
        Ok(n)
    }
}
impl Seek for Counted {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(from)
    }
}

struct Candidate {
    path: PathBuf,
    identity: [u64; 2],
}
fn open_nofollow(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("RPF input must be a regular file without symlinks"));
    }
    Ok(file)
}
fn file_identity(file: &std::fs::File) -> Result<[u64; 2]> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok([metadata.dev(), metadata.ino()])
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the file owns a live handle and information is a valid writable output.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) } == 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok([
            u64::from(information.dwVolumeSerialNumber),
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        ])
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(Error::Unsupported("safe RPF directory creation requires Unix or Windows file identities".into()))
    }
}
#[cfg(unix)]
fn inventory_identity(_path: &Path, metadata: &std::fs::Metadata) -> Result<[u64; 2]> {
    use std::os::unix::fs::MetadataExt;
    Ok([metadata.dev(), metadata.ino()])
}
#[cfg(not(unix))]
fn inventory_identity(path: &Path, _metadata: &std::fs::Metadata) -> Result<[u64; 2]> {
    file_identity(&open_nofollow(path)?)
}
fn open_candidate(candidate: &Candidate) -> Result<std::fs::File> {
    let file = open_nofollow(&candidate.path)?;
    if file_identity(&file)? != candidate.identity {
        return Err(invalid("RPF input changed during directory discovery"));
    }
    Ok(file)
}

fn scan(directory: &Path, paths: &mut Vec<Candidate>, visited: &mut usize, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err(invalid("input directory nesting exceeds 64 levels"));
    }
    for item in std::fs::read_dir(directory)? {
        let item = item?;
        *visited += 1;
        if *visited > 200_000 {
            return Err(invalid("input directory inventory exceeds 200000 entries"));
        }
        let kind = item.file_type()?;
        // Do not follow directory cycles or index files outside the selected tree.
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            scan(&item.path(), paths, visited, depth + 1)?;
        } else if kind.is_file() && item.file_name().to_str().is_some_and(|name| frame::name_parts(name).is_ok()) {
            let path = item.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            let identity = inventory_identity(&path, &metadata)?;
            paths.push(Candidate { path, identity });
            if paths.len() > 100_000 {
                return Err(invalid("input directory contains more than 100000 CADRG candidates"));
            }
        }
    }
    Ok(())
}

pub(super) fn create(input: &Path, output: Option<&Path>, options: &RpfTocOptions, overwrite: bool) -> Result<RpfTocReport> {
    let root = input.canonicalize()?;
    if !root.is_dir() {
        return Err(Error::InvalidArgument("RPF input must be a directory".into()));
    }
    let default_output = root.join("A.TOC");
    let output = output.unwrap_or(&default_output);
    let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")).canonicalize()?;
    // Keep stored paths portable and traversal-free, including when a caller
    // chooses a TOC in an ancestor of the input tree.
    if !root.starts_with(&parent) {
        return Err(Error::InvalidArgument("RPF output must be in the input directory or one of its ancestors".into()));
    }
    let destination = parent.join(output.file_name().ok_or_else(|| Error::InvalidArgument("RPF output needs a filename".into()))?);
    if destination.exists() && !overwrite {
        return Err(Error::InvalidArgument("RPF output exists; enable overwrite to replace it".into()));
    }
    let mut paths = Vec::new();
    scan(&root, &mut paths, &mut 0, 0)?;
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    if paths.iter().any(|candidate| candidate.path == destination) {
        return Err(Error::InvalidArgument("RPF output cannot replace an input frame".into()));
    }
    let mut frames = Vec::new();
    let mut metadata_bytes = 0;
    let mut skipped_files = 0;
    let mut groups = std::collections::BTreeSet::new();
    for candidate in paths {
        let path = &candidate.path;
        let name = path.strip_prefix(&parent).map_err(|_| invalid("frame outside the TOC directory"))?;
        let name = name
            .to_str()
            .ok_or_else(|| Error::InvalidArgument("RPF frame paths must be UTF-8".into()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let mut reader = Counted { file: open_candidate(&candidate)?, bytes: 0 };
        let mut prefix = [0u8; 9];
        let supported = match reader.read_exact(&mut prefix) {
            Ok(()) => matches!(&prefix, b"NITF02.00" | b"NITF02.10"),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => false,
            Err(error) => return Err(Error::Io(error)),
        };
        if !supported {
            metadata_bytes += reader.bytes;
            skipped_files += 1;
            continue;
        }
        let frame = read_frame(&name, &mut reader)?;
        metadata_bytes += reader.bytes;
        groups.insert((options.scale.or_else(|| frame.inferred_scale()), frame.zone));
        frames.push(frame);
    }
    let bytes = encode(&frames, options)?;
    #[cfg(unix)]
    let previous_permissions = if overwrite {
        match std::fs::metadata(&destination) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::Io(error)),
        }
    } else {
        None
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".terrio-rpftoc-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Creation applies the process umask, without inspecting/changing global state.
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temporary = builder.tempfile_in(&parent)?;
    #[cfg(unix)]
    if let Some(permissions) = previous_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    if overwrite {
        temporary.persist(&destination).map_err(|e| Error::Io(e.error))?;
    } else {
        temporary.persist_noclobber(&destination).map_err(|e| Error::Io(e.error))?;
    }
    Ok(RpfTocReport { frames: frames.len(), skipped_files, groups: groups.len(), metadata_bytes, output_bytes: bytes.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn valid_frame(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let grid = super::super::grid::Grid::new(2, 1_000_000).unwrap();
        let path = directory.join("000C9010.ON2");
        std::fs::write(&path, frame::test_frame(grid.coverage([21, 3], [21, 3]))).unwrap();
        path
    }
    #[test]
    fn non_nitf_photos_are_skipped_and_counted() {
        let stage = tempfile::tempdir().unwrap();
        let path = valid_frame(stage.path());
        let length = std::fs::metadata(path).unwrap().len();
        std::fs::write(stage.path().join("DSC00123.JPG"), b"this is a photo").unwrap();
        std::fs::write(stage.path().join("SCAN0001.TIF"), b"tiff").unwrap();
        let report = create(stage.path(), None, &Default::default(), false).unwrap();
        assert_eq!((report.frames, report.skipped_files), (1, 2));
        assert_eq!(report.metadata_bytes, length + 9 + 9 + 4);
    }
    #[test]
    fn output_cannot_replace_a_frame_or_use_a_sibling_directory() {
        let stage = tempfile::tempdir().unwrap();
        let input = stage.path().join("frames");
        let path = valid_frame(&input);
        let before = std::fs::read(&path).unwrap();
        assert!(create(&input, Some(&path), &Default::default(), true).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let sibling = stage.path().join("sibling");
        std::fs::create_dir(&sibling).unwrap();
        assert!(create(&input, Some(&sibling.join("A.TOC")), &Default::default(), false).is_err());
        assert!(!sibling.join("A.TOC").exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_frames_are_not_indexed() {
        let stage = tempfile::tempdir().unwrap();
        let path = valid_frame(stage.path());
        std::os::unix::fs::symlink(path, stage.path().join("000CA010.ON2")).unwrap();
        let report = create(stage.path(), None, &Default::default(), false).unwrap();
        assert_eq!((report.frames, report.skipped_files), (1, 0));
    }
    #[cfg(unix)]
    #[test]
    fn inventory_substitutions_are_rejected_before_reading() {
        let stage = tempfile::tempdir().unwrap();
        let input = stage.path().join("frames");
        let path = valid_frame(&input);
        let original = std::fs::File::open(&path).unwrap();
        let mut candidates = Vec::new();
        scan(&input, &mut candidates, &mut 0, 0).unwrap();
        assert_eq!(candidates.len(), 1);
        let outside = valid_frame(&stage.path().join("outside"));
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(outside, &path).unwrap();
        assert!(open_candidate(&candidates[0]).is_err());
        std::fs::remove_file(&path).unwrap();
        valid_frame(&input);
        assert!(matches!(open_candidate(&candidates[0]),Err(Error::InvalidData(message)) if message.contains("changed")));
        drop(original);
    }
    #[cfg(unix)]
    #[test]
    fn new_files_respect_umask_and_overwrites_preserve_mode() {
        use std::os::unix::fs::PermissionsExt;
        let stage = tempfile::tempdir().unwrap();
        valid_frame(stage.path());
        let control = stage.path().join("control");
        std::fs::write(&control, b"").unwrap();
        let expected = std::fs::metadata(control).unwrap().permissions().mode() & 0o777;
        create(stage.path(), None, &Default::default(), false).unwrap();
        let output = stage.path().join("A.TOC");
        assert_eq!(std::fs::metadata(&output).unwrap().permissions().mode() & 0o777, expected);
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o640)).unwrap();
        create(stage.path(), None, &Default::default(), true).unwrap();
        assert_eq!(std::fs::metadata(output).unwrap().permissions().mode() & 0o777, 0o640);
    }
    #[test]
    #[ignore = "requires prepared RPF fixtures; see docs/testing.md"]
    fn real_directory_creation_is_metadata_only_when_available() {
        let root = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA to prepared RPF fixtures"));
        let paths = super::super::prepared_flat_frames(&root);
        let count = paths.len();
        let input = root.join("rpftoc_landsat");
        let stage = tempfile::tempdir().unwrap();
        let frames = stage.path().join("frames");
        for source in paths {
            let target = frames.join(source.strip_prefix(&input).unwrap());
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(source, target).unwrap();
        }
        let output = stage.path().join("A.TOC");
        let report = create(&frames, Some(&output), &Default::default(), false).unwrap();
        assert_eq!((report.frames, report.groups, report.skipped_files), (count, 2, 0));
        assert!(report.metadata_bytes > 0 && report.metadata_bytes < count as u64 * 1024);
        assert_eq!(report.output_bytes as u64, std::fs::metadata(&output).unwrap().len());
        let bytes = std::fs::read(&output).unwrap();
        let prefix = b"./frames/ON/RPF";
        assert!(bytes.windows(prefix.len()).any(|w| w == prefix));
        assert!(create(&frames, Some(&output), &Default::default(), false).is_err());
        assert!(create(&frames, Some(&output), &Default::default(), true).is_ok());
    }
    #[test]
    fn invalid_or_empty_input_preserves_destination() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("A.TOC");
        std::fs::write(&output, b"original").unwrap();
        assert!(create(root.path(), Some(&output), &Default::default(), true).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
        let sub = root.path().join("output");
        std::fs::create_dir(&sub).unwrap();
        assert!(create(root.path(), Some(&sub.join("A.TOC")), &Default::default(), false).is_err());
        assert!(!sub.join("A.TOC").exists());
    }
}
