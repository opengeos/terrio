//! Locating datasets: local paths, HTTP(S) URLs, cloud object storage and zip archives.
//!
//! Accepted forms:
//!
//! - local paths;
//! - `http://` and `https://` URLs;
//! - `s3://bucket/key`: signed with AWS credentials when they are configured, unsigned otherwise
//!   or with `AWS_NO_SIGN_REQUEST=YES` (see [`crate::s3`]);
//! - `gs://bucket/key` (public buckets);
//! - `az://container/blob` (public containers, or with a SAS token in `AZURE_STORAGE_SAS_TOKEN`),
//!   in the storage account named by `AZURE_STORAGE_ACCOUNT`; without it, `az://account/container/blob`;
//! - zip archives, either the archive alone (its main dataset is opened) or `archive.zip!inner/path`;
//! - GDAL's virtual file system prefixes `/vsicurl/`, `/vsis3/`, `/vsigs/`, `/vsiaz/` and `/vsizip/`.
//!
//! [`resolve`] copies remote files and archive members to a temporary directory that lives as long
//! as the returned [`LocalPath`]. GeoTIFFs can instead be read in place, fetching only the parts
//! read: see [`crate::LazyRaster`].

use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Extensions of the files that make up a shapefile besides the `.shp` itself.
#[cfg(remote)]
const SHAPEFILE_SIDECARS: &[&str] = &["shx", "dbf", "prj", "cpg"];

/// Extensions tried, in order, to find the main dataset inside an archive.
pub(crate) const MAIN_DATASET_EXTENSIONS: &[&str] = &[
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
    "tab",
    "osm",
    "csv",
    "tif",
    "tiff",
    "jp2",
    "img",
    "asc",
    "hgt",
    "hdr",
    "zarr",
    "las",
    "laz",
    "ply",
    "e57",
];

/// A dataset location resolved to the local file system.
#[derive(Debug)]
pub struct LocalPath {
    path: PathBuf,
    #[cfg(not(target_family = "wasm"))]
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
            #[cfg(not(target_family = "wasm"))]
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
    /// An object in Amazon S3, or the S3-compatible service in `AWS_S3_ENDPOINT`.
    S3 {
        /// The bucket.
        bucket: String,
        /// The object key.
        key: String,
    },
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
        if let Some(rest) = input.strip_prefix("/vsiaz/") {
            return Location::parse(&format!("az://{rest}"));
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
        if let Some(rest) = input.strip_prefix("/vsiaz/") {
            return Location::parse_unarchived(&format!("az://{rest}"));
        }
        if let Some(rest) = input.strip_prefix("s3://") {
            let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
            Location::S3 { bucket: bucket.to_string(), key: key.to_string() }
        } else if let Some(rest) = input.strip_prefix("gs://") {
            Location::Url(format!("https://storage.googleapis.com/{rest}"))
        } else if let Some(rest) = input.strip_prefix("az://") {
            let (account, path) = match std::env::var("AZURE_STORAGE_ACCOUNT").ok().filter(|a| !a.is_empty()) {
                Some(account) => (account, rest),
                None => {
                    let (account, path) = rest.split_once('/').unwrap_or((rest, ""));
                    (account.to_string(), path)
                }
            };
            let sas = std::env::var("AZURE_STORAGE_SAS_TOKEN").unwrap_or_default();
            let sas = sas.trim_start_matches('?');
            let query = if sas.is_empty() { String::new() } else { format!("?{sas}") };
            Location::Url(format!("https://{account}.blob.core.windows.net/{path}{query}"))
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
            Location::S3 { key, .. } => key.rsplit('/').next().unwrap_or_default().to_string(),
            Location::Zip { archive, inner: Some(inner) } => {
                let _ = archive;
                inner.rsplit('/').next().unwrap_or(inner).to_string()
            }
            Location::Zip { archive, inner: None } => archive.file_name(),
        }
    }

    /// The file name of the dataset at this location: for an archive named without a member,
    /// that of its main dataset, found in a local archive's listing (else the archive's name
    /// without `.zip`, as `tile.las` for `tile.las.zip`).
    pub fn dataset_name(&self) -> String {
        let Location::Zip { archive, inner: None } = self else {
            return self.file_name();
        };
        #[cfg(not(target_family = "wasm"))]
        if let Location::Local(p) = archive.as_ref()
            && let Some(name) = archive_main_member(p)
        {
            return name.rsplit('/').next().unwrap_or(&name).to_string();
        }
        let name = archive.file_name();
        match name.len().checked_sub(4).filter(|&i| name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".zip")) {
            Some(i) => name[..i].to_string(),
            None => name,
        }
    }
}

/// The main dataset of a local zip archive, from its listing: the first member (by name) with
/// the highest-priority extension.
#[cfg(not(target_family = "wasm"))]
fn archive_main_member(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let mut names: Vec<String> = zip.file_names().filter_map(|n| n.ok()).filter(|n| !n.ends_with('/')).map(|n| n.into_owned()).collect();
    names.sort_unstable();
    MAIN_DATASET_EXTENSIONS.iter().find_map(|ext| names.iter().find(|n| extension_of(n).as_deref() == Some(*ext))).cloned()
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

/// The scheme and host (with its port) of a URL, lowercased: what decides whether two URLs are
/// of the same origin. `None` when `url` has no scheme.
pub(crate) fn url_origin(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Without user information, and without the scheme's default port.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h).to_ascii_lowercase();
    let host = match (scheme.as_str(), host.rsplit_once(':')) {
        ("https", Some((h, "443"))) | ("http", Some((h, "80"))) => h.to_string(),
        _ => host,
    };
    Some((scheme, host))
}

/// Resolves a dataset location to a local path, downloading or extracting it when needed.
pub fn resolve(input: &str) -> Result<LocalPath> {
    resolve_location(&Location::parse(input))
}

fn resolve_location(loc: &Location) -> Result<LocalPath> {
    match loc {
        Location::Local(p) => Ok(LocalPath::local(p.clone())),
        #[cfg(remote)]
        Location::Url(_) | Location::S3 { .. } => download(loc),
        #[cfg(all(not(remote), not(target_family = "wasm")))]
        Location::Url(_) | Location::S3 { .. } => Err(Error::disabled("remote")),
        #[cfg(not(target_family = "wasm"))]
        Location::Zip { archive, inner } => extract(archive, inner.as_deref()),
        #[cfg(target_family = "wasm")]
        _ => Err(Error::Unsupported("remote files and archives are not supported in WebAssembly builds".into())),
    }
}

impl Location {
    /// The remote file at this location, when it is one.
    #[cfg(remote)]
    #[cfg_attr(docsrs, doc(cfg(feature = "remote")))]
    pub fn remote(&self) -> Option<crate::http::Remote> {
        match self {
            Location::Url(u) => Some(crate::http::Remote::Url(u.clone())),
            Location::S3 { bucket, key } => Some(crate::http::Remote::S3(crate::s3::S3Object { bucket: bucket.clone(), key: key.clone() })),
            _ => None,
        }
    }

    /// The same location with another extension (for sidecar files).
    #[cfg(remote)]
    fn with_extension(&self, ext: &str) -> Option<Location> {
        let swap = |s: &str| {
            let (path, query) = s.split_once('?').map_or((s, None), |(p, q)| (p, Some(q)));
            let dot = path.rfind('.').filter(|&i| !path[i..].contains('/'))?;
            Some(format!("{}.{ext}{}", &path[..dot], query.map(|q| format!("?{q}")).unwrap_or_default()))
        };
        match self {
            Location::Url(u) => swap(u).map(Location::Url),
            Location::S3 { bucket, key } => swap(key).map(|key| Location::S3 { bucket: bucket.clone(), key }),
            _ => None,
        }
    }
}

#[cfg(remote)]
fn download(loc: &Location) -> Result<LocalPath> {
    let remote = loc.remote().ok_or_else(|| Error::InvalidArgument("not a remote location".into()))?;
    let temp = tempfile::Builder::new().prefix("terrio-").tempdir()?;
    let name = loc.file_name();
    let name = if name.is_empty() { "download".to_string() } else { name };
    let path = temp.path().join(&name);
    crate::http::download(&remote, &path)?;
    if extension_of(&name).as_deref() == Some("shp") {
        let stem = Path::new(&name).file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        for ext in SHAPEFILE_SIDECARS {
            // Sidecars are optional; the reader reports any that are required and missing.
            if let Some(sidecar) = loc.with_extension(ext).and_then(|l| l.remote()) {
                let _ = crate::http::download(&sidecar, &temp.path().join(format!("{stem}.{ext}")));
            }
        }
    }
    Ok(LocalPath { path, _temp: Some(temp) })
}

#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
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

#[cfg(not(target_family = "wasm"))]
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
        assert_eq!(url_origin("https://user@Example.org:443/a?b"), url_origin("https://example.org/c"));
        assert_ne!(url_origin("https://example.org:8443/a"), url_origin("https://example.org/a"));
        assert_ne!(url_origin("http://example.org/a"), url_origin("https://example.org/a"));
        assert_eq!(Location::parse("/vsis3/bucket/dir/a.tif"), Location::S3 { bucket: "bucket".into(), key: "dir/a.tif".into() });
        assert_eq!(Location::parse("s3://bucket/dir/a.tif").file_name(), "a.tif");
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
