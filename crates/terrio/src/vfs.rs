//! Locating datasets: local paths, HTTP(S) URLs, cloud object storage and zip archives.
//!
//! Accepted forms:
//!
//! - local paths;
//! - `http://` and `https://` URLs;
//! - `s3://bucket/key` (public buckets, or the endpoint in `AWS_S3_ENDPOINT`) and
//!   `gs://bucket/key` (public buckets);
//! - zip archives, either the archive alone (its main dataset is opened) or `archive.zip!inner/path`;
//! - GDAL's virtual file system prefixes `/vsicurl/`, `/vsis3/`, `/vsigs/` and `/vsizip/`.
//!
//! Remote files and archive members are copied to a temporary directory that lives as long as the
//! returned [`LocalPath`].

use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Extensions of the files that make up a shapefile besides the `.shp` itself.
const SHAPEFILE_SIDECARS: &[&str] = &["shx", "dbf", "prj", "cpg"];

/// Extensions tried, in order, to find the main dataset inside an archive.
const MAIN_DATASET_EXTENSIONS: &[&str] = &[
    "shp",
    "gpkg",
    "geojson",
    "json",
    "fgb",
    "parquet",
    "geoparquet",
    "kml",
    "gml",
    "gpx",
    "topojson",
    "mif",
    "csv",
    "tif",
    "tiff",
    "jp2",
    "img",
    "asc",
    "hdr",
    "zarr",
];

/// A dataset location resolved to the local file system.
#[derive(Debug)]
pub struct LocalPath {
    path: PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    _temp: Option<tempfile::TempDir>,
}

impl LocalPath {
    /// The local path of the dataset.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The local path as a string.
    pub fn as_str(&self) -> &str {
        self.path.to_str().unwrap_or_default()
    }

    fn local(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(not(target_arch = "wasm32"))]
            _temp: None,
        }
    }
}

/// A parsed dataset location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A local file or directory.
    Local(PathBuf),
    /// An HTTP(S) URL.
    Url(String),
    /// A member of a zip archive (or the archive's main dataset when `inner` is `None`).
    Zip {
        /// The archive.
        archive: Box<Location>,
        /// The member's path within the archive.
        inner: Option<String>,
    },
}

impl Location {
    /// Parses a dataset location.
    pub fn parse(input: &str) -> Self {
        let input = input.trim();
        if let Some(rest) = input.strip_prefix("/vsizip/") {
            let rest = rest.strip_prefix('{').map(|r| r.replacen('}', "", 1)).unwrap_or_else(|| rest.to_string());
            return match find_archive_end(&rest) {
                Some(end) => {
                    let inner = rest[end..].trim_start_matches('/');
                    Location::Zip {
                        archive: Box::new(Location::parse_unarchived(&rest[..end])),
                        inner: (!inner.is_empty()).then(|| inner.to_string()),
                    }
                }
                None => Location::Zip { archive: Box::new(Location::parse_unarchived(&rest)), inner: None },
            };
        }
        if let Some(rest) = input.strip_prefix("/vsicurl/") {
            return Location::parse(rest);
        }
        if let Some(rest) = input.strip_prefix("/vsis3/") {
            return Location::parse(&format!("s3://{rest}"));
        }
        if let Some(rest) = input.strip_prefix("/vsigs/") {
            return Location::parse(&format!("gs://{rest}"));
        }
        if let Some((archive, inner)) = input.split_once(".zip!") {
            return Location::Zip {
                archive: Box::new(Location::parse_unarchived(&format!("{archive}.zip"))),
                inner: Some(inner.trim_start_matches('/').to_string()),
            };
        }
        let base = Location::parse_unarchived(input);
        if extension_of(&base.file_name()).as_deref() == Some("zip") {
            return Location::Zip { archive: Box::new(base), inner: None };
        }
        base
    }

    /// Parses a location that is not looked into even when it is an archive.
    fn parse_unarchived(input: &str) -> Self {
        if let Some(rest) = input.strip_prefix("/vsicurl/") {
            return Location::parse_unarchived(rest);
        }
        if let Some(rest) = input.strip_prefix("/vsis3/") {
            return Location::parse_unarchived(&format!("s3://{rest}"));
        }
        if let Some(rest) = input.strip_prefix("/vsigs/") {
            return Location::parse_unarchived(&format!("gs://{rest}"));
        }
        if let Some(rest) = input.strip_prefix("s3://") {
            let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
            match std::env::var("AWS_S3_ENDPOINT") {
                Ok(endpoint) => {
                    let endpoint = endpoint.trim_end_matches('/');
                    let endpoint = if endpoint.contains("://") { endpoint.to_string() } else { format!("https://{endpoint}") };
                    Location::Url(format!("{endpoint}/{bucket}/{key}"))
                }
                Err(_) => Location::Url(format!("https://{bucket}.s3.amazonaws.com/{key}")),
            }
        } else if let Some(rest) = input.strip_prefix("gs://") {
            Location::Url(format!("https://storage.googleapis.com/{rest}"))
        } else if input.starts_with("http://") || input.starts_with("https://") {
            Location::Url(input.to_string())
        } else {
            Location::Local(PathBuf::from(input))
        }
    }

    /// Whether the dataset is on the local file system (not remote, not in an archive).
    pub fn is_local(&self) -> bool {
        matches!(self, Location::Local(_))
    }

    /// The location's file name.
    pub fn file_name(&self) -> String {
        match self {
            Location::Local(p) => p.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string(),
            Location::Url(u) => url_file_name(u),
            Location::Zip { archive, inner: Some(inner) } => {
                let _ = archive;
                inner.rsplit('/').next().unwrap_or(inner).to_string()
            }
            Location::Zip { archive, inner: None } => archive.file_name(),
        }
    }
}

/// The byte offset just past the `.zip` archive name in a `/vsizip/` path.
fn find_archive_end(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(".zip") {
        let end = from + i + 4;
        if end == s.len() || s[end..].starts_with('/') {
            return Some(end);
        }
        from = end;
    }
    None
}

fn url_file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or_default().to_string()
}

fn extension_of(name: &str) -> Option<String> {
    Path::new(name).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

/// Resolves a dataset location to a local path, downloading or extracting it when needed.
pub fn resolve(input: &str) -> Result<LocalPath> {
    resolve_location(&Location::parse(input))
}

fn resolve_location(loc: &Location) -> Result<LocalPath> {
    match loc {
        Location::Local(p) => Ok(LocalPath::local(p.clone())),
        #[cfg(not(target_arch = "wasm32"))]
        Location::Url(url) => download(url),
        #[cfg(not(target_arch = "wasm32"))]
        Location::Zip { archive, inner } => extract(archive, inner.as_deref()),
        #[cfg(target_arch = "wasm32")]
        _ => Err(Error::Unsupported("remote files and archives are not supported in WebAssembly builds".into())),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn download(url: &str) -> Result<LocalPath> {
    let temp = tempfile::Builder::new().prefix("terrio-").tempdir()?;
    let name = url_file_name(url);
    let name = if name.is_empty() { "download".to_string() } else { name };
    let path = temp.path().join(&name);
    http_get_to_file(url, &path)?;
    if extension_of(&name).as_deref() == Some("shp") {
        let clean = url.split(['?', '#']).next().unwrap_or(url);
        let base = &clean[..clean.len() - ".shp".len()];
        let stem = Path::new(&name).file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        for ext in SHAPEFILE_SIDECARS {
            // Sidecars are optional; the reader reports any that are required and missing.
            let _ = http_get_to_file(&format!("{base}.{ext}"), &temp.path().join(format!("{stem}.{ext}")));
        }
    }
    Ok(LocalPath { path, _temp: Some(temp) })
}

/// Fetches `url` into `path`.
#[cfg(not(target_arch = "wasm32"))]
fn http_get_to_file(url: &str, path: &Path) -> Result<()> {
    let response = match ureq::get(url).call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => return Err(Error::Http(format!("{url}: status {code}"))),
        Err(e) => return Err(Error::Http(format!("{url}: {e}"))),
    };
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(path)?;
    std::io::copy(&mut reader, &mut file)?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn extract(archive: &Location, inner: Option<&str>) -> Result<LocalPath> {
    let archive_local = resolve_location(archive)?;
    let temp = tempfile::Builder::new().prefix("terrio-").tempdir()?;
    let file = std::fs::File::open(archive_local.path())?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    zip.extract(temp.path())?;
    let path = match inner {
        Some(inner) => {
            let p = temp.path().join(inner);
            if !p.exists() {
                return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("'{inner}' is not in the archive"))));
            }
            p
        }
        None => {
            find_main_dataset(temp.path())?.ok_or_else(|| Error::Unsupported(format!("no dataset found in '{}'", archive.file_name())))?
        }
    };
    Ok(LocalPath { path, _temp: Some(temp) })
}

/// Finds the main dataset in an extracted archive: the first file (by name) with the
/// highest-priority extension, searching subdirectories too.
#[cfg(not(target_arch = "wasm32"))]
fn find_main_dataset(dir: &Path) -> Result<Option<PathBuf>> {
    let mut files = Vec::new();
    collect_files(dir, &mut files)?;
    files.sort();
    for ext in MAIN_DATASET_EXTENSIONS {
        if let Some(f) = files.iter().find(|f| extension_of(f.to_str().unwrap_or_default()).as_deref() == Some(ext)) {
            return Ok(Some(f.clone()));
        }
    }
    Ok(None)
}

#[cfg(not(target_arch = "wasm32"))]
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let is_zarr = extension_of(path.to_str().unwrap_or_default()).as_deref() == Some("zarr");
        if path.is_dir() && !is_zarr {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_locations() {
        assert_eq!(Location::parse("a/b.tif"), Location::Local("a/b.tif".into()));
        assert_eq!(Location::parse("/vsicurl/https://x.org/a.tif"), Location::Url("https://x.org/a.tif".into()));
        assert_eq!(Location::parse("gs://bucket/a.tif"), Location::Url("https://storage.googleapis.com/bucket/a.tif".into()));
        assert_eq!(
            Location::parse("/vsizip/data/a.zip/dir/b.shp"),
            Location::Zip { archive: Box::new(Location::Local("data/a.zip".into())), inner: Some("dir/b.shp".into()) }
        );
        assert_eq!(
            Location::parse("https://x.org/a.zip"),
            Location::Zip { archive: Box::new(Location::Url("https://x.org/a.zip".into())), inner: None }
        );
        assert_eq!(
            Location::parse("data/a.zip!b.shp"),
            Location::Zip { archive: Box::new(Location::Local("data/a.zip".into())), inner: Some("b.shp".into()) }
        );
        assert_eq!(
            Location::parse("/vsizip//vsicurl/https://x.org/a.zip/b.shp"),
            Location::Zip { archive: Box::new(Location::Url("https://x.org/a.zip".into())), inner: Some("b.shp".into()) }
        );
    }
}
