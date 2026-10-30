//! GeoPackage storage maintenance in pure Rust, preserving encoded records and schema SQL.
use crate::{Error, Result};

/// Physical storage before and after repacking. Logical values and rowids are retained.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepackReport {
    /// Original file size in bytes.
    pub bytes_before: u64,
    /// Compacted file size in bytes.
    pub bytes_after: u64,
    /// Preserved SQLite page size in bytes.
    pub page_size: u32,
    /// Original page count, including unused pages.
    pub pages_before: u64,
    /// Compacted page count.
    pub pages_after: u64,
}

/// Repack a self-contained GeoPackage image. All tables and indexes are rebuilt from their
/// encoded records, including WITHOUT ROWID tables, expression/collated indexes and UTF-16.
/// An external WAL or journal must have been checkpointed/recovered before taking the image.
pub fn repack_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    #[cfg(feature = "sqlite")]
    {
        check(bytes)?;
        crate::drivers::sqlite_repack::compact(bytes)
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = bytes;
        Err(Error::disabled("sqlite"))
    }
}

#[cfg(feature = "sqlite")]
fn check(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
        return Err(Error::InvalidData("not an SQLite GeoPackage".into()));
    }
    let id = u32::from_be_bytes(bytes[68..72].try_into().unwrap());
    if !matches!(id, 0x4750_4b47 | 0x4750_3130 | 0x4750_3131) {
        return Err(Error::InvalidData("SQLite application ID is not a GeoPackage".into()));
    }
    Ok(())
}

/// Repack a native GeoPackage in place under SQLite-compatible exclusive locks and a synced
/// rollback journal. In-memory GeoPackages are replaced atomically as one memory-store write.
/// Native paths retain their operating-system representation, including non-UTF-8 Unix names.
/// Active WAL files are refused; close/checkpoint the SQLite connection first.
pub fn repack(path: impl AsRef<std::path::Path>) -> Result<RepackReport> {
    #[cfg(feature = "sqlite")]
    {
        let path = path.as_ref();
        if crate::memfs::is_virtual(path) {
            let name = path.to_str().ok_or_else(|| Error::InvalidArgument("memory path must be UTF-8".into()))?;
            return crate::vsi::transform_memory(name, |before| {
                let after = repack_bytes(before)?;
                let result = report(before, &after);
                Ok((after, result))
            });
        }
        let location = path.to_str().map(crate::vfs::Location::parse).unwrap_or_else(|| crate::vfs::Location::Local(path.to_path_buf()));
        // Repacking identifies the native file by its SQLite header, not its
        // suffix. Explicit archive locations remain unsupported.
        let location = match location {
            crate::vfs::Location::Zip { archive, inner: None } if path.to_str().is_some_and(|name| !name.starts_with("/vsizip/")) => {
                *archive
            }
            location => location,
        };
        match location {
            crate::vfs::Location::Local(path) => {
                if cfg!(target_family = "wasm") {
                    return Err(Error::Unsupported("GeoPackage repack in WebAssembly needs /vsimem/ or the bytes API".into()));
                }
                if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
                    return Err(Error::InvalidArgument(
                        "GeoPackage repack needs a regular native file; symbolic links and special files are refused".into(),
                    ));
                }
                let mut editor = crate::drivers::sqlite_edit::Editor::open_repack(&path)?
                    .map_err(|why| Error::Unsupported(format!("GeoPackage repack: {}", why.0)))?;
                let before = editor.image()?;
                let after = repack_bytes(&before)?;
                let result = report(&before, &after);
                editor.replace_image(before, after)?;
                editor.commit()?;
                Ok(result)
            }
            _ => Err(Error::Unsupported("GeoPackage repack needs a native or /vsimem/ file".into())),
        }
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = path;
        Err(Error::disabled("sqlite"))
    }
}

#[cfg(feature = "sqlite")]
fn report(before: &[u8], after: &[u8]) -> RepackReport {
    let page_size = match u16::from_be_bytes([before[16], before[17]]) {
        1 => 65536,
        value => u32::from(value),
    };
    RepackReport {
        bytes_before: before.len() as u64,
        bytes_after: after.len() as u64,
        page_size,
        pages_before: before.len() as u64 / u64::from(page_size),
        pages_after: after.len() as u64 / u64::from(page_size),
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::drivers::vector::sqlite::{self as writer, Table, Value};
    fn sample() -> Vec<u8> {
        let mut tables = Vec::new();
        for name in ["gpkg_contents", "gpkg_spatial_ref_sys", "notes"] {
            tables.push(Table {
                name: name.into(),
                sql: format!("CREATE TABLE {name} (id INTEGER PRIMARY KEY, name TEXT)"),
                rows: vec![(3, vec![Value::Null, Value::Text("survey".into())]), (9, vec![Value::Null, Value::Text("kept".into())])],
                autoindexes: Vec::new(),
            });
        }
        writer::write_database(&tables, &[], 10400, 0x4750_4b47).unwrap()
    }
    #[test]
    #[cfg(any(all(unix, not(target_os = "emscripten")), windows))]
    fn native_counter_bumps_once_with_wraparound() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("counter.gpkg");
        for original_counter in [0u32, 41, u32::MAX] {
            let mut original = sample();
            original[24..28].copy_from_slice(&original_counter.to_be_bytes());
            original[92..96].copy_from_slice(&original_counter.to_be_bytes());
            let expected = original_counter.wrapping_add(1);
            let compacted = repack_bytes(&original).unwrap();
            // Byte compaction builds an image; only a native file commit advances the counter.
            assert_eq!(&compacted[24..28], &original_counter.to_be_bytes());
            assert_eq!(&compacted[92..96], &original_counter.to_be_bytes());
            std::fs::write(&path, &original).unwrap();
            repack(&path).unwrap();
            let native = std::fs::read(&path).unwrap();
            assert_eq!(&native[24..28], &expected.to_be_bytes());
            assert_eq!(&native[92..96], &expected.to_be_bytes());
        }
    }

    #[test]
    fn preserves_records_and_headers_in_memory() {
        let bytes = sample();
        let compact = repack_bytes(&bytes).unwrap();
        let before = crate::drivers::sqlite::Db::new(bytes).unwrap();
        let after = crate::drivers::sqlite::Db::new(compact).unwrap();
        for name in ["gpkg_contents", "gpkg_spatial_ref_sys", "notes"] {
            assert_eq!(before.table(name).unwrap().rows, after.table(name).unwrap().rows);
        }
        assert_eq!(after.header_u32(60), 10400);
        assert_eq!(after.header_u32(68), 0x4750_4b47);
        assert_eq!(after.header_u32(36), 0);
    }
    #[test]
    fn memory_repack_keeps_whitespace_distinct_from_another_database_name() {
        let spaced = "/vsimem/gpkg-space-identity.gpkg ";
        let plain = "/vsimem/gpkg-space-identity.gpkg";
        crate::vsi::write(plain, b"preserve unrelated object").unwrap();
        crate::vsi::write(spaced, &sample()).unwrap();
        repack(spaced).unwrap();
        assert_eq!(crate::vsi::read(plain).unwrap(), b"preserve unrelated object");
        assert!(repack_bytes(&crate::vsi::read(spaced).unwrap()).is_ok());
        crate::vsi::delete(spaced, false).unwrap();
        crate::vsi::delete(plain, false).unwrap();
    }

    #[test]
    fn memory_repack_accepts_an_archive_like_filename() {
        let name = "/vsimem/gpkg-repack-snapshot.zip";
        crate::vsi::write(name, &sample()).unwrap();
        let result = repack(name);
        let bytes = crate::vsi::read(name).unwrap();
        crate::vsi::delete(name, false).unwrap();
        let report = result.unwrap();
        assert_eq!(report.bytes_after, bytes.len() as u64);
        assert_eq!(crate::drivers::sqlite::Db::new(bytes).unwrap().table("notes").unwrap().rows.len(), 2);
    }

    #[test]
    fn memory_replacement_and_failed_repack_preserve_input() {
        let name = "/vsimem/gpkg-repack-core.gpkg";
        crate::vsi::write(name, &sample()).unwrap();
        let report = repack(name).unwrap();
        assert_eq!(report.page_size, 4096);
        assert_eq!(report.pages_after * 4096, report.bytes_after);
        crate::vsi::write(name, b"not a geopackage").unwrap();
        assert!(repack(name).is_err());
        assert_eq!(crate::vsi::read(name).unwrap(), b"not a geopackage");
        crate::vsi::delete(name, false).unwrap();
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn native_repack_accepts_archive_like_filenames() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["snapshot.zip", "snapshot.ZIP"] {
            let path = directory.path().join(name);
            std::fs::write(&path, sample()).unwrap();
            let report = repack(&path).unwrap();
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(report.bytes_after, bytes.len() as u64);
            assert_eq!(crate::drivers::sqlite::Db::new(bytes).unwrap().table("notes").unwrap().rows.len(), 2);
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn native_repack_uses_journal_without_leaving_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.gpkg");
        std::fs::write(&path, sample()).unwrap();
        let report = repack(path.to_str().unwrap()).unwrap();
        assert_eq!(report.bytes_after, std::fs::metadata(&path).unwrap().len());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let mut bad = sample();
        bad[68..72].fill(0);
        std::fs::write(&path, &bad).unwrap();
        assert!(repack(path.to_str().unwrap()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bad);
    }
    #[cfg(unix)]
    #[test]
    fn repack_refuses_sidecar_links_without_touching_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.gpkg");
        let foreign = dir.path().join("keep.bin");
        let original = sample();
        std::fs::write(&path, &original).unwrap();
        std::fs::write(&foreign, b"preserve foreign bytes").unwrap();
        for suffix in ["-journal", "-wal", "-shm"] {
            let sidecar = dir.path().join(format!("data.gpkg{suffix}"));
            for hard in [false, true] {
                if hard {
                    std::fs::hard_link(&foreign, &sidecar).unwrap();
                } else {
                    std::os::unix::fs::symlink(&foreign, &sidecar).unwrap();
                }
                assert!(repack(path.to_str().unwrap()).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), original);
                assert_eq!(std::fs::read(&foreign).unwrap(), b"preserve foreign bytes");
                std::fs::remove_file(&sidecar).unwrap();
            }
        }
    }
    #[test]
    fn memory_repack_does_not_overwrite_a_concurrent_update() {
        let name = "/vsimem/repack-concurrent.gpkg";
        crate::vsi::write(name, &sample()).unwrap();
        let result = crate::vsi::transform_memory(name, |before| {
            let after = repack_bytes(before)?;
            crate::vsi::write(name, b"newer writer")?;
            Ok((after, ()))
        });
        assert!(result.is_err(), "stale compaction must fail");
        assert_eq!(crate::vsi::read(name).unwrap(), b"newer writer");
        crate::vsi::delete(name, false).unwrap();
    }
}
