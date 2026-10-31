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

fn scan(directory: &Path, paths: &mut Vec<PathBuf>, visited: &mut usize, depth: usize) -> Result<()> {
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
            paths.push(item.path());
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
    paths.sort();
    if paths.contains(&destination) {
        return Err(Error::InvalidArgument("RPF output cannot replace an input frame".into()));
    }
    let mut frames = Vec::new();
    let mut metadata_bytes = 0;
    let mut groups = std::collections::BTreeSet::new();
    for path in paths {
        let name = path.strip_prefix(&parent).map_err(|_| invalid("frame outside the TOC directory"))?;
        let name = name
            .to_str()
            .ok_or_else(|| Error::InvalidArgument("RPF frame paths must be UTF-8".into()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let mut reader = Counted { file: std::fs::File::open(&path)?, bytes: 0 };
        let frame = read_frame(&name, &mut reader)?;
        metadata_bytes += reader.bytes;
        groups.insert((options.scale.or_else(|| frame.inferred_scale()), frame.zone));
        frames.push(frame);
    }
    let bytes = encode(&frames, options)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    if overwrite {
        temporary.persist(&destination).map_err(|e| Error::Io(e.error))?;
    } else {
        temporary.persist_noclobber(&destination).map_err(|e| Error::Io(e.error))?;
    }
    Ok(RpfTocReport { frames: frames.len(), groups: groups.len(), metadata_bytes, output_bytes: bytes.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_directory_creation_is_metadata_only_when_available() {
        let Some(root) = std::env::var_os("TERRIO_TEST_DATA").map(PathBuf::from) else { return };
        let input = root.join("rpftoc_landsat");
        if !input.is_dir() {
            return;
        }
        let stage = tempfile::tempdir().unwrap();
        let frames = stage.path().join("frames");
        for name in ["ON/RPF/ZONE2/000C9010.ON2", "ON/RPF/ZONE2/000CA010.ON2", "TP/RPF/ZONE2/001DP010.TP2", "TP/RPF/ZONE2/001DQ010.TP2"] {
            let source = input.join(name);
            if !source.is_file() {
                return;
            }
            let target = frames.join(name);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(source, target).unwrap();
        }
        let output = stage.path().join("A.TOC");
        let report = create(&frames, Some(&output), &Default::default(), false).unwrap();
        assert_eq!((report.frames, report.groups), (4, 2));
        assert_eq!(report.metadata_bytes, 4 * 729);
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
