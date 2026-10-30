//! Read-only GeoPackage validation of self-contained SQLite images.
use crate::{Error, Result};

#[cfg(feature = "sqlite")]
fn same_image_metadata(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.dev(), before.ino(), before.ctime(), before.ctime_nsec())
            != (after.dev(), after.ino(), after.ctime(), after.ctime_nsec())
        {
            return false;
        }
    }
    true
}

/// A GeoPackage core requirement number or a named extension requirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(untagged)]
pub enum GpkgRequirement {
    /// Numbering used by the GeoPackage specification and GDAL's validator.
    Core(u32),
    /// Extension specification identifier, including its requirement number.
    Extension(&'static str),
}
impl GpkgRequirement {
    /// Core requirement number; extension requirements retain their named identifier.
    pub fn number(self) -> Option<u32> {
        match self {
            Self::Core(n) => Some(n),
            Self::Extension(_) => None,
        }
    }
}
impl std::fmt::Display for GpkgRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(number) => write!(f, "requirement {number}"),
            Self::Extension(name) => write!(f, "extension requirement {name}"),
        }
    }
}
impl PartialEq<u32> for GpkgRequirement {
    fn eq(&self, n: &u32) -> bool {
        self.number() == Some(*n)
    }
}
/// A violated requirement and a diagnostic identifying the affected object.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct GpkgViolation {
    /// Numeric core requirement or named extension requirement.
    pub requirement: GpkgRequirement,
    /// Explanation, with a table/column/row identifier where available.
    pub message: String,
}
/// Controls shared by native, Python, JavaScript and CLI/MCP validation.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpkgValidationOptions {
    /// Add advisory attribute storage-class, range, length and date checks.
    pub full_check: bool,
    /// Warnings prevent a successful validation verdict.
    pub strict: bool,
    /// Use Unicode lower/upper/LIKE behavior for files created with ICU SQLite.
    /// False uses stock SQLite's ASCII case behavior.
    pub unicode_case: bool,
}
/// Validation verdict. Unsupported SQL/codec cases never produce a valid verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct GpkgValidation {
    /// True only when validation completed without errors, or strict-mode warnings.
    #[serde(rename = "valid")]
    pub is_valid: bool,
    /// No unsupported condition prevented completing the verdict.
    #[serde(rename = "complete")]
    pub is_complete: bool,
    /// Encoded GeoPackage version, such as 10400 for version 1.4.0.
    pub version: Option<u32>,
    /// Violated core or extension requirements.
    pub errors: Vec<GpkgViolation>,
    /// Advisory diagnostics; only strict mode makes these invalidate the verdict.
    pub warnings: Vec<String>,
    /// Checks that could not be completed, rather than proven specification violations.
    pub unsupported_checks: Vec<String>,
}
/// Validate a complete, checkpointed database image without loading SQLite extensions.
///
/// Checks cover SQLite records/indexes/foreign keys, core and extension metadata,
/// geometry binaries, tile payloads and supported read-only views. Unsupported view
/// forms/functions produce `is_complete=false`; they are not reported as valid.
/// The encoded image is retained by the caller; user rows are streamed, with index,
/// identifier, DISTINCT/set and sorted-result keys retained as needed. Join views also
/// retain their right-hand input rows once per evaluation, including geometry blobs.
pub fn validate_gpkg_bytes(bytes: &[u8], options: &GpkgValidationOptions) -> Result<GpkgValidation> {
    #[cfg(feature = "sqlite")]
    {
        let report = crate::gpkg_validation::check_core_with_options(bytes, options);
        let is_complete = report.unsupported_checks.is_empty();
        Ok(GpkgValidation {
            is_valid: is_complete && report.errors.is_empty() && (!options.strict || report.warnings.is_empty()),
            is_complete,
            version: report.version,
            errors: report.errors,
            warnings: report.warnings,
            unsupported_checks: report.unsupported_checks,
        })
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (bytes, options);
        Err(Error::disabled("sqlite"))
    }
}
/// Validate a native or `/vsimem/` GeoPackage, reading its complete image once.
///
/// Native files must be closed/checkpointed before validation. Nonempty WAL or
/// rollback-journal sidecars are refused; validation never checkpoints, recovers
/// or modifies a database. This is an image validator, not a live SQLite snapshot.
/// Remote/archive data can be supplied through the bytes API after reading it.
/// A path without the `.gpkg` extension violates requirement 3; uploaded bytes
/// have no filename and therefore do not undergo that check.
pub fn validate_gpkg(path: impl AsRef<std::path::Path>, options: &GpkgValidationOptions) -> Result<GpkgValidation> {
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (path, options);
        Err(Error::disabled("sqlite"))
    }
    #[cfg(feature = "sqlite")]
    {
        let path = path.as_ref();
        let bytes = if crate::memfs::is_virtual(path) {
            crate::memfs::read(path)?
        } else {
            let location =
                path.to_str().map(crate::vfs::Location::parse).unwrap_or_else(|| crate::vfs::Location::Local(path.to_path_buf()));
            let crate::vfs::Location::Local(path) = location else {
                return Err(Error::Unsupported(
                    "GeoPackage validation needs a native or /vsimem/ path; use the bytes API for other locations".into(),
                ));
            };
            if cfg!(target_family = "wasm") {
                return Err(Error::Unsupported("GeoPackage validation in WebAssembly needs /vsimem/ or the bytes API".into()));
            }
            let sidecars = || -> Result<()> {
                for suffix in ["-wal", "-journal"] {
                    let mut name = path.as_os_str().to_os_string();
                    name.push(suffix);
                    match std::fs::metadata(&name) {
                        Ok(metadata) if metadata.len() > 0 => {
                            return Err(Error::Unsupported(
                                "GeoPackage validation needs a checkpointed image without a nonempty WAL or rollback journal".into(),
                            ));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Ok(())
            };
            sidecars()?;
            let before = std::fs::metadata(&path)?;
            if !before.is_file() {
                return Err(Error::InvalidArgument("GeoPackage validation needs a regular file".into()));
            }
            let bytes = std::fs::read(&path)?;
            sidecars()?;
            let after = std::fs::metadata(&path)?;
            if !same_image_metadata(&before, &after) {
                return Err(Error::Unsupported("GeoPackage changed while its image was being read".into()));
            }
            bytes
        };
        let mut report = validate_gpkg_bytes(&bytes, options)?;
        if !path.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("gpkg")) {
            report.errors.push(GpkgViolation { requirement: GpkgRequirement::Core(3), message: "filename extension is not .gpkg".into() });
            report.is_valid = false;
        }
        Ok(report)
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn replacement_with_the_same_length_and_mtime_changes_image_identity() {
        let first = tempfile::NamedTempFile::new().unwrap();
        let second = tempfile::NamedTempFile::new().unwrap();
        let times = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        first.as_file().set_times(times).unwrap();
        second.as_file().set_times(times).unwrap();
        let before = first.as_file().metadata().unwrap();
        let after = second.as_file().metadata().unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert!(!same_image_metadata(&before, &after));
        assert!(same_image_metadata(&before, &before));
    }
    #[test]
    fn malformed_images_return_structured_verdicts() {
        let result = validate_gpkg_bytes(b"not sqlite", &GpkgValidationOptions::default()).unwrap();
        assert!(!result.is_valid && result.is_complete);
        assert_eq!(result.errors[0].requirement, GpkgRequirement::Core(1));
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["valid"], false);
        assert_eq!(json["complete"], true);
        assert_eq!(json["errors"][0]["requirement"], 1);
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn public_options_and_image_guards_on_real_geopackages() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import pathlib,sqlite3,sys
from osgeo import gdal
gdal.UseExceptions();root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2])
gdal.VectorTranslate(str(root/'base.gpkg'),str(data/'countries.gpkg'),layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
with sqlite3.connect(root/'base.gpkg') as db:
 db.executescript("CREATE TABLE attributes(id INTEGER PRIMARY KEY,v TINYINT);INSERT INTO attributes VALUES(1,300);INSERT INTO gpkg_contents(table_name,data_type,identifier) VALUES('attributes','attributes','attributes')")
(root/'incomplete.gpkg').write_bytes((root/'base.gpkg').read_bytes())
with sqlite3.connect(root/'incomplete.gpkg') as db:
 db.executescript("CREATE VIEW aggregate_view AS SELECT count(*) AS id FROM countries;INSERT INTO gpkg_contents(table_name,data_type,identifier) VALUES('aggregate_view','attributes','aggregate_view')")
(root/'unicode.gpkg').write_bytes((root/'base.gpkg').read_bytes())
with sqlite3.connect(root/'unicode.gpkg') as db:
 db.executescript("CREATE TABLE checked(v TEXT CHECK(lower(v)='é'));INSERT INTO checked VALUES('É')")
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let path = directory.path().join("base.gpkg");
        let bytes = std::fs::read(&path).unwrap();
        let options = GpkgValidationOptions::default();
        let normal = validate_gpkg(&path, &options).unwrap();
        assert!(normal.is_valid, "{normal:?}");
        assert_eq!(normal, validate_gpkg_bytes(&bytes, &options).unwrap());
        let full = validate_gpkg(&path, &GpkgValidationOptions { full_check: true, ..options.clone() }).unwrap();
        assert!(full.is_valid && full.is_complete);
        assert!(!full.warnings.is_empty());
        let strict = validate_gpkg(&path, &GpkgValidationOptions { full_check: true, strict: true, ..options.clone() }).unwrap();
        assert!(!strict.is_valid && strict.is_complete && strict.errors.is_empty());
        let incomplete = validate_gpkg(directory.path().join("incomplete.gpkg"), &options).unwrap();
        assert!(!incomplete.is_valid && !incomplete.is_complete && !incomplete.unsupported_checks.is_empty(), "{incomplete:?}");
        let unicode = directory.path().join("unicode.gpkg");
        assert!(!validate_gpkg(&unicode, &options).unwrap().is_valid);
        assert!(validate_gpkg(&unicode, &GpkgValidationOptions { unicode_case: true, ..options.clone() }).unwrap().is_valid);
        let wrong_name = directory.path().join("image.sqlite");
        std::fs::write(&wrong_name, &bytes).unwrap();
        let wrong = validate_gpkg(&wrong_name, &options).unwrap();
        assert!(wrong.errors.iter().any(|e| e.requirement == 3));
        for suffix in ["-wal", "-journal"] {
            let sidecar = directory.path().join(format!("base.gpkg{suffix}"));
            std::fs::write(&sidecar, b"uncheckpointed").unwrap();
            assert!(matches!(validate_gpkg(&path, &options), Err(Error::Unsupported(_))));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(std::fs::read(&sidecar).unwrap(), b"uncheckpointed");
            std::fs::remove_file(sidecar).unwrap();
        }
        let memory = "/vsimem/gpkg-validation-public-api.gpkg";
        crate::vsi::write(memory, &bytes).unwrap();
        assert_eq!(validate_gpkg(memory, &options).unwrap(), normal);
        assert_eq!(crate::vsi::read(memory).unwrap(), bytes);
        crate::vsi::delete(memory, false).unwrap();
    }
}
