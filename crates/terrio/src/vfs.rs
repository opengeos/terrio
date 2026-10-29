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
#[cfg(not(target_arch = "wasm32"))]
use std::io::{Read, Seek, SeekFrom};
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

/// Whether `input` is the URL of an `az://` blob marked for bearer-token requests (a relative link
/// resolved against one, for instance). Only this process's marker counts, which holds random bits
/// no dataset name from outside can carry (see `http::azure_bearer_prefix`): a public
/// `azure+https://...` is no URL.
fn is_azure_bearer_url(input: &str) -> bool {
    #[cfg(remote)]
    {
        input
            .strip_prefix(crate::http::azure_bearer_prefix())
            .is_some_and(|rest| rest.starts_with("https://") || rest.starts_with("http://"))
    }
    #[cfg(not(remote))]
    {
        let _ = input;
        false
    }
}

/// Whether `account` is an Azure Storage account name: 3 to 24 lowercase letters and digits.
pub(crate) fn valid_azure_account(account: &str) -> bool {
    (3..=24).contains(&account.len()) && account.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Why a name that looks like an Azure location is not one, for names [`Location::parse`] leaves
/// local because they would otherwise reach a host other than an Azure Blob Storage endpoint.
fn invalid_azure_location(name: &str) -> Option<String> {
    if name.starts_with("az://") || name.starts_with("/vsiaz/") {
        return Some(format!(
            "'{name}' is not a valid Azure location: the storage account (AZURE_STORAGE_ACCOUNT, else the first part of \
             the path) must be 3 to 24 lowercase letters and digits, and the blob must be on its endpoint"
        ));
    }
    name.starts_with("azure+").then(|| format!("'{}...' is not a dataset name: `azure+` marks terrio's own URLs", &name[..6]))
}

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
            // GDAL's `/vsizip/{archive}/member`: the braces delimit the archive, whatever its name.
            if let Some((archive, inner)) = rest.strip_prefix('{').and_then(|r| r.split_once('}')) {
                let inner = inner.trim_start_matches('/');
                return Location::Zip {
                    archive: Box::new(Location::parse_unarchived(archive)),
                    inner: (!inner.is_empty()).then(|| inner.to_string()),
                };
            }
            let rest = rest.to_string();
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
    pub(crate) fn parse_unarchived(input: &str) -> Self {
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
            let configured = crate::config::get("AZURE_STORAGE_ACCOUNT").map(|a| a.trim().to_string());
            let (account, path) = match &configured {
                Some(account) => (account.clone(), rest),
                None => {
                    let (account, path) = rest.split_once('/').unwrap_or((rest, ""));
                    (account.to_string(), path)
                }
            };
            let custom = custom_azure_endpoint();
            // GDAL takes the account from its configuration; one taken from the path must be an
            // account name, so that it cannot name another host (`az://attacker.example?x=/c/b`
            // would reach attacker.example). The URL built must stay on the endpoint.
            if custom.is_none() && !valid_azure_account(&account) {
                return Location::Local(PathBuf::from(input));
            }
            let endpoint = azure_endpoint(&account);
            let sas = crate::config::get("AZURE_STORAGE_SAS_TOKEN").unwrap_or_default();
            let sas = sas.trim_start_matches('?');
            let query = if sas.is_empty() { String::new() } else { format!("?{sas}") };
            let url = format!("{endpoint}/{path}{query}");
            if url_origin(&url).is_none() || url_origin(&url) != url_origin(&endpoint) {
                return Location::Local(PathBuf::from(input));
            }
            // With a bearer token configured (and no SAS), mark the blob so that its requests, and
            // only those of `az://` blobs, carry the token (see `http::azure_bearer_prefix`): only
            // when the account or the endpoint comes from the configuration, as GDAL's `/vsiaz/`
            // takes it, never from the path alone.
            #[cfg(remote)]
            let marker = if sas.is_empty() && crate::http::azure_token_configured() && (configured.is_some() || custom.is_some()) {
                crate::http::azure_bearer_prefix()
            } else {
                ""
            };
            #[cfg(not(remote))]
            let marker = "";
            Location::Url(format!("{marker}{url}"))
        } else if input.starts_with("http://") || input.starts_with("https://") || is_azure_bearer_url(input) {
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
    let mut file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let directory = check_directory(&mut file, &ZipLimits::from_config()).ok()?;
    let zip = open_archive(file, &directory).ok()?;
    main_member(&entry_names(&zip))
}

/// The main dataset among an archive's entry names: the first (by name) with the
/// highest-priority extension. A Zarr store is its directory, whose files are not candidates.
#[cfg(not(target_arch = "wasm32"))]
fn main_member(names: &[String]) -> Option<String> {
    let mut candidates: Vec<String> = names
        .iter()
        .filter(|n| !n.ends_with('/'))
        .map(|name| match name.split('/').position(|part| extension_of(part).as_deref() == Some("zarr")) {
            Some(i) => name.split('/').take(i + 1).collect::<Vec<_>>().join("/"),
            None => name.clone(),
        })
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    MAIN_DATASET_EXTENSIONS.iter().find_map(|ext| candidates.iter().find(|n| extension_of(n).as_deref() == Some(*ext))).cloned()
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
        Location::Local(p) => match invalid_azure_location(&p.to_string_lossy()) {
            Some(reason) => Err(Error::InvalidArgument(reason)),
            None => Ok(LocalPath::local(p.clone())),
        },
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
    /// How a refusal names a remote location: its URL without the query (which may hold a
    /// signature) and without the marker of an `az://` blob's token requests, or `s3://bucket/key`.
    #[cfg(remote)]
    pub(crate) fn shown(&self) -> String {
        match self {
            Location::Url(u) => {
                let u = u.strip_prefix(crate::http::azure_bearer_prefix()).unwrap_or(u);
                u.split(['?', '#']).next().unwrap_or(u).to_string()
            }
            Location::S3 { bucket, key } => format!("s3://{bucket}/{key}"),
            other => other.file_name(),
        }
    }

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
    // By range, and refused after its first bytes when they, or its size, are not what its name
    // says (see `formats::check_remote_head`).
    crate::http::download_checked(&remote, &path, &|head, len| crate::formats::check_remote_head(&name, &loc.shown(), head, len))?;
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

/// Limits on what reading a member of a zip archive may cost, for archives from untrusted
/// sources. GDAL's `/vsizip/` decompresses members as they are read; terrio copies the member
/// asked for, and the files that go with it, to a temporary directory, never the whole archive.
/// The file functions of [`crate::vsi`] keep the same limits on the zip and tar archives they
/// list and read. Set with these options (see [`crate::config`]):
///
/// - `TERRIO_ZIP_MAX_ENTRIES`: the most entries an archive may list (default 100,000);
/// - `TERRIO_ZIP_MAX_SIZE`: the most bytes extracted for one dataset, its files together
///   (default 4 GiB);
/// - `TERRIO_ZIP_MAX_RATIO`: the highest ratio of a member's size to its compressed size
///   (default 1,032, the most deflate reaches; GDAL refuses members above it too).
///
/// Members that are symbolic links are refused, whatever the limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipLimits {
    /// The most entries an archive may list.
    pub max_entries: u64,
    /// The most bytes extracted for one dataset.
    pub max_size: u64,
    /// The highest ratio of a member's size to its compressed size.
    pub max_ratio: u64,
}

impl Default for ZipLimits {
    fn default() -> Self {
        Self { max_entries: 100_000, max_size: 4 << 30, max_ratio: 1032 }
    }
}

impl ZipLimits {
    /// The limits the options set, the defaults for those unset or invalid.
    pub fn from_config() -> Self {
        let d = Self::default();
        let get = |key: &str, default: u64| crate::config::get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default);
        Self {
            max_entries: get("TERRIO_ZIP_MAX_ENTRIES", d.max_entries),
            max_size: get("TERRIO_ZIP_MAX_SIZE", d.max_size),
            max_ratio: get("TERRIO_ZIP_MAX_RATIO", d.max_ratio),
        }
    }

    /// Refuses an archive (`kind`: `zip`, `tar`) that lists `entries` entries, more than allowed.
    pub(crate) fn check_entries(&self, kind: &str, entries: u64) -> Result<()> {
        if entries > self.max_entries {
            return Err(Error::Unsupported(format!(
                "{kind}: the archive lists {entries} entries, more than the {} allowed (TERRIO_ZIP_MAX_ENTRIES)",
                self.max_entries
            )));
        }
        Ok(())
    }

    /// Refuses a member `name` declaring `size` bytes compressed to `packed`, a ratio above the one
    /// allowed.
    pub(crate) fn check_ratio(&self, kind: &str, name: &str, size: u64, packed: u64) -> Result<()> {
        if packed == 0 && size > 0 || packed > 0 && size / packed > self.max_ratio {
            return Err(Error::Unsupported(format!(
                "{kind}: '{name}' declares {size} bytes compressed to {packed}, a ratio above the {} allowed (TERRIO_ZIP_MAX_RATIO)",
                self.max_ratio
            )));
        }
        Ok(())
    }

    /// Refuses `total` bytes extracted for `member` and its files, more than allowed.
    pub(crate) fn check_size(&self, kind: &str, member: &str, total: u64) -> Result<()> {
        if total > self.max_size {
            return Err(Error::Unsupported(format!(
                "{kind}: '{member}' and its files hold more than the {} bytes allowed (TERRIO_ZIP_MAX_SIZE)",
                self.max_size
            )));
        }
        Ok(())
    }
}

/// The largest central directory read: far more than the entries allowed by default take.
pub(crate) const MAX_DIRECTORY_BYTES: u64 = 64 << 20;

/// The bytes at the end of an archive that hold its end of central directory record: the record
/// and a comment of up to 65,535 bytes.
#[cfg(not(target_arch = "wasm32"))]
const TAIL_BYTES: u64 = 22 + 0xFFFF;

#[cfg(not(target_arch = "wasm32"))]
fn zip_error(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("zip: {message}"))
}

/// Where an archive's central directory is, from its end record.
#[cfg(not(target_arch = "wasm32"))]
struct Directory {
    /// The directory's position in the file.
    start: u64,
    size: u64,
    /// The position in the file of the archive's start (non-zero with data prepended): passed to
    /// the zip crate, which otherwise scans the archive from its first byte to find it.
    archive_offset: u64,
}

/// Checks what an archive's end of central directory record (zip64's, when there is one)
/// declares, before the directory itself is read: its entry count against the limits, its size
/// against the archive's.
#[cfg(not(target_arch = "wasm32"))]
fn check_directory<R: Read + Seek>(r: &mut R, limits: &ZipLimits) -> Result<Directory> {
    let len = r.seek(SeekFrom::End(0))?;
    let tail_len = len.min(TAIL_BYTES);
    if tail_len < 22 {
        return Err(zip_error("not a zip archive"));
    }
    let mut tail = vec![0u8; tail_len as usize];
    r.seek(SeekFrom::Start(len - tail_len))?;
    r.read_exact(&mut tail)?;
    let at = (0..=tail.len() - 22).rev().find(|&i| tail[i..].starts_with(b"PK\x05\x06")).ok_or_else(|| zip_error("not a zip archive"))?;
    let u16_at = |b: &[u8], i: usize| u64::from(u16::from_le_bytes([b[i], b[i + 1]]));
    let u32_at = |b: &[u8], i: usize| u64::from(u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]));
    let u64_at = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or_default());
    let (mut entries, mut size, mut offset) = (u16_at(&tail, at + 10), u32_at(&tail, at + 12), u32_at(&tail, at + 16));
    // The directory ends where the end record starts, unless a zip64 record follows it.
    let mut archive_offset = (len - tail_len + at as u64).saturating_sub(size).saturating_sub(offset);
    // Zip64: a locator just before the record points to the zip64 record, which holds the counts.
    if (entries == 0xFFFF || size == 0xFFFF_FFFF || offset == 0xFFFF_FFFF) && at >= 20 && tail[at - 20..].starts_with(b"PK\x06\x07") {
        archive_offset = 0;
        let record = u64_at(&tail, at - 20 + 8);
        if record.checked_add(56).is_some_and(|end| end <= len) {
            let mut b = [0u8; 56];
            r.seek(SeekFrom::Start(record))?;
            r.read_exact(&mut b)?;
            if b.starts_with(b"PK\x06\x06") {
                (entries, size, offset) = (u64_at(&b, 32), u64_at(&b, 40), u64_at(&b, 48));
            }
        }
    }
    limits.check_entries("zip", entries)?;
    if size > len || size > MAX_DIRECTORY_BYTES {
        return Err(zip_error(format!("a central directory of {size} bytes in an archive of {len}")));
    }
    Ok(Directory { start: offset.saturating_add(archive_offset), size, archive_offset })
}

/// Opens an archive whose end record [`check_directory`] read.
#[cfg(not(target_arch = "wasm32"))]
fn open_archive<R: Read + Seek>(reader: R, directory: &Directory) -> Result<zip::ZipArchive<R>> {
    let config = zip::read::Config { archive_offset: zip::read::ArchiveOffset::Known(directory.archive_offset) };
    Ok(zip::ZipArchive::with_config(config, reader)?)
}

/// The names of an archive's entries, with `/` separators.
#[cfg(not(target_arch = "wasm32"))]
fn entry_names<R: Read + Seek>(zip: &zip::ZipArchive<R>) -> Vec<String> {
    (0..zip.len()).map(|i| zip.name_for_index(i).and_then(|n| n.ok()).map(|n| n.replace('\\', "/")).unwrap_or_default()).collect()
}

/// The entries to extract for the member `inner`: the member and the files beside it that share
/// its stem (a shapefile's `.shx`, `.dbf`, `.prj` and `.cpg`, a raster's world file or
/// `.aux.xml`), which drivers open next to a dataset as GDAL's do inside archives; or, when
/// `inner` names a directory (a Zarr store), the files under it. `None` when it is in neither.
#[cfg(not(target_arch = "wasm32"))]
fn select_entries(names: &[String], inner: &str) -> Option<Vec<usize>> {
    let split = |name: &str| -> (String, String) {
        match name.rsplit_once('/') {
            Some((dir, file)) => (dir.to_string(), file.to_ascii_lowercase()),
            None => (String::new(), name.to_ascii_lowercase()),
        }
    };
    if let Some(i) = names.iter().position(|n| n == inner) {
        let (dir, file) = split(inner);
        let stem = Path::new(&file).file_stem().and_then(|s| s.to_str()).unwrap_or(&file).to_string();
        let sidecar = |(d, f): (String, String)| d == dir && (f == stem || f.starts_with(&format!("{stem}.")));
        let siblings = names.iter().enumerate().filter(|&(j, n)| j != i && !n.ends_with('/') && sidecar(split(n))).map(|(j, _)| j);
        return Some(std::iter::once(i).chain(siblings).collect());
    }
    let prefix = format!("{inner}/");
    let under: Vec<usize> = names.iter().enumerate().filter(|(_, n)| n.starts_with(&prefix) && !n.ends_with('/')).map(|(i, _)| i).collect();
    (!under.is_empty()).then_some(under)
}

/// Copies the member `inner` of an archive (its main dataset when `None`) and the files that go
/// with it (see [`select_entries`]) to a temporary directory, within the [`ZipLimits`]. Remote
/// archives are read by range: their central directory, then the members' bytes.
#[cfg(not(target_family = "wasm"))]
fn extract(archive: &Location, inner: Option<&str>) -> Result<LocalPath> {
    let limits = ZipLimits::from_config();
    #[cfg(remote)]
    if let Some(remote) = archive.remote() {
        let reader = crate::http::RangeReader::open(&remote)?;
        let options = crate::http::HttpOptions::from_env();
        let window = (options.cache_size / 2).clamp(options.block_size.max(1), 8 << 20);
        let members = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let fetch = reader.clone();
        let source = RemoteArchive { r: reader, members: members.clone(), fetched: (0, 0), window };
        let prefetch = |ranges: &[(u64, u64)]| fetch.prefetch(ranges).map_err(Error::from);
        return extract_from(source, archive, inner, &limits, &prefetch, &|ranges| *members.borrow_mut() = ranges);
    }
    let local = resolve_location(archive)?;
    let file = std::io::BufReader::new(std::fs::File::open(local.path())?);
    extract_from(file, archive, inner, &limits, &|_| Ok(()), &|_| {})
}

/// Fetches byte ranges `(offset, length)` of an archive ahead of reading them.
#[cfg(not(target_family = "wasm"))]
type Prefetch<'a> = &'a dyn Fn(&[(u64, u64)]) -> Result<()>;

/// [`extract`] from an archive's bytes: `prefetch` fetches byte ranges ahead of reading them,
/// `plan` is told the byte ranges `(start, end)` of the members to be read.
#[cfg(not(target_family = "wasm"))]
fn extract_from<R: Read + Seek>(
    mut reader: R,
    archive: &Location,
    inner: Option<&str>,
    limits: &ZipLimits,
    prefetch: Prefetch,
    plan: &dyn Fn(Vec<(u64, u64)>),
) -> Result<LocalPath> {
    let len = reader.seek(SeekFrom::End(0))?;
    prefetch(&[(len - len.min(TAIL_BYTES), len.min(TAIL_BYTES))])?;
    let directory = check_directory(&mut reader, limits)?;
    prefetch(&[(directory.start, directory.size)])?;
    let mut zip = open_archive(reader, &directory)?;
    if zip.len() as u64 > limits.max_entries {
        return Err(Error::Unsupported(format!("zip: the archive lists more than the {} entries allowed", limits.max_entries)));
    }
    let names = entry_names(&zip);
    let member = match inner {
        Some(inner) => inner.trim_matches('/').to_string(),
        None => main_member(&names).ok_or_else(|| Error::Unsupported(format!("no dataset found in '{}'", archive.file_name())))?,
    };
    let selected = select_entries(&names, &member)
        .ok_or_else(|| Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("'{member}' is not in the archive"))))?;
    // Every entry is checked before any is read.
    let mut total = 0u64;
    let mut ranges = Vec::with_capacity(selected.len());
    for &i in &selected {
        let e = zip.by_index_data(i)?;
        let name = &names[i];
        if e.is_symlink() {
            return Err(Error::Unsupported(format!("zip: '{name}' is a symbolic link")));
        }
        if e.enclosed_name().is_none() {
            return Err(zip_error(format!("'{name}' is not a relative path within the archive")));
        }
        let (size, packed) = (e.size(), e.compressed_size());
        limits.check_ratio("zip", name, size, packed)?;
        total = total.saturating_add(size);
        limits.check_size("zip", &member, total)?;
        // The local header (its extra field may be longer than the central one) and the data.
        let start = e.header_start();
        ranges.push((start, start.saturating_add(packed).saturating_add(30 + name.len() as u64 + 1024).min(len)));
    }
    plan(ranges);
    let temp = tempfile::Builder::new().prefix("terrio-").tempdir()?;
    for &i in &selected {
        let mut e = zip.by_index(i)?;
        let path = temp.path().join(e.enclosed_name().ok_or_else(|| zip_error("unsafe member path"))?);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let declared = e.size();
        let mut out = std::fs::File::create(&path)?;
        // A member is cut at the size it declares, which the limits were checked against.
        let copied = std::io::copy(&mut (&mut e).take(declared.saturating_add(1)), &mut out)?;
        if copied > declared {
            return Err(zip_error(format!("'{}' holds more than the {declared} bytes it declares", names[i])));
        }
    }
    Ok(LocalPath { path: temp.path().join(&member), _temp: Some(temp) })
}

/// The Blob Storage endpoint `az://` and `/vsiaz/` paths resolve to, without a trailing slash, as
/// GDAL 3.12.1 builds it (`VSIAzureBlobHandleHelper`, `port/cpl_azure.cpp`): the `BlobEndpoint` of
/// `AZURE_STORAGE_CONNECTION_STRING`, else `CPL_AZURE_ENDPOINT` (Azurite, sovereign clouds), else
/// `https://<account>.blob.core.windows.net`, or `http://` with `CPL_AZURE_USE_HTTPS=NO`.
pub(crate) fn azure_endpoint(account: &str) -> String {
    azure_endpoint_with(account, &crate::config::get)
}

/// [`azure_endpoint`] with the options `get` returns (a request's captured options, say).
pub(crate) fn azure_endpoint_with(account: &str, get: &dyn Fn(&str) -> Option<String>) -> String {
    if let Some(endpoint) = custom_azure_endpoint_with(get) {
        return endpoint;
    }
    let https = get("CPL_AZURE_USE_HTTPS").is_none_or(|v| !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"));
    format!("{}://{account}.blob.core.windows.net", if https { "https" } else { "http" })
}

/// The endpoint `AZURE_STORAGE_CONNECTION_STRING`'s `BlobEndpoint` or `CPL_AZURE_ENDPOINT` sets,
/// without a trailing slash, when one does.
pub(crate) fn custom_azure_endpoint() -> Option<String> {
    custom_azure_endpoint_with(&crate::config::get)
}

/// [`custom_azure_endpoint`] with the options `get` returns.
pub(crate) fn custom_azure_endpoint_with(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let from_connection_string = get("AZURE_STORAGE_CONNECTION_STRING").and_then(|cs| {
        cs.split(';').find_map(|part| {
            let (key, value) = part.split_once('=')?;
            key.trim().eq_ignore_ascii_case("BlobEndpoint").then(|| value.trim().to_string())
        })
    });
    from_connection_string
        .or_else(|| get("CPL_AZURE_ENDPOINT"))
        .map(|e| e.trim().trim_end_matches('/').to_string())
        .filter(|e| !e.is_empty())
}

/// The Blob Storage endpoint an Azure bearer token may be sent to, under the options `get`
/// returns: the custom endpoint when one is set, else that of the configured
/// `AZURE_STORAGE_ACCOUNT`, when it is an account name. `None` when neither is configured: GDAL's
/// `/vsiaz/` takes the account from its configuration, never from the path.
pub(crate) fn azure_token_endpoint(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(endpoint) = custom_azure_endpoint_with(get) {
        return Some(endpoint);
    }
    let account = get("AZURE_STORAGE_ACCOUNT").map(|a| a.trim().to_string()).filter(|a| valid_azure_account(a))?;
    Some(azure_endpoint_with(&account, get))
}

/// The first `n` bytes of the member a zip location names (its main dataset when it names
/// none), read without extracting anything, for identifying the member's format before it is
/// opened. `None` when the location is not in an archive or the member cannot be read.
pub(crate) fn member_head(loc: &Location, n: u64) -> Option<Vec<u8>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Location::Zip { archive, inner } = loc else { return None };
        #[cfg(remote)]
        if let Some(remote) = archive.remote() {
            return head_from(crate::http::RangeReader::open(&remote).ok()?, inner.as_deref(), n).ok();
        }
        let Location::Local(p) = archive.as_ref() else { return None };
        head_from(std::io::BufReader::new(std::fs::File::open(p).ok()?), inner.as_deref(), n).ok()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (loc, n);
        None
    }
}

/// [`member_head`] from an archive's bytes, within the [`ZipLimits`].
#[cfg(not(target_arch = "wasm32"))]
fn head_from<R: Read + Seek>(mut reader: R, inner: Option<&str>, n: u64) -> Result<Vec<u8>> {
    let directory = check_directory(&mut reader, &ZipLimits::from_config())?;
    let mut zip = open_archive(reader, &directory)?;
    let names = entry_names(&zip);
    let member = match inner {
        Some(inner) => inner.trim_matches('/').to_string(),
        None => main_member(&names).ok_or_else(|| zip_error("no dataset in the archive"))?,
    };
    let i = names.iter().position(|name| *name == member).ok_or_else(|| zip_error(format!("'{member}' is not a file in the archive")))?;
    let mut head = Vec::new();
    zip.by_index(i)?.take(n).read_to_end(&mut head)?;
    Ok(head)
}

/// A remote archive read by range: the end of central directory and the directory fetched ahead
/// (see `extract_from`), then each member's bytes in windows as they are read, rather than block
/// after block.
#[cfg(remote)]
struct RemoteArchive {
    r: crate::http::RangeReader,
    /// The byte ranges `(start, end)` of the members to be read.
    members: std::rc::Rc<std::cell::RefCell<Vec<(u64, u64)>>>,
    /// The last window fetched, `(start, end)`.
    fetched: (u64, u64),
    window: u64,
}

#[cfg(remote)]
impl Read for RemoteArchive {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let pos = self.r.stream_position()?;
        if !(self.fetched.0..self.fetched.1).contains(&pos)
            && let Some(&(_, end)) = self.members.borrow().iter().find(|(s, e)| (*s..*e).contains(&pos))
        {
            let n = (end - pos).min(self.window);
            self.r.prefetch(&[(pos, n)])?;
            self.fetched = (pos, pos + n);
        }
        self.r.read(buf)
    }
}

#[cfg(remote)]
impl Seek for RemoteArchive {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.r.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL an `az://` path resolves to under the options GDAL 3.12.1 reads
    /// (`port/cpl_azure.cpp`), with the token marker, if any test set a token, left out.
    fn azure_url(options: &[(&str, &str)], path: &str) -> String {
        crate::config::push_scope(options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect());
        let location = Location::parse(path);
        crate::config::pop_scope();
        match location {
            Location::Url(url) => url.trim_start_matches(crate::http::azure_bearer_prefix()).to_string(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn azure_endpoint_options_build_gdals_url() {
        let account = [("AZURE_STORAGE_ACCOUNT", "acct")];
        assert_eq!(azure_url(&account, "/vsiaz/c/d/b.tif"), "https://acct.blob.core.windows.net/c/d/b.tif");
        let http = [("AZURE_STORAGE_ACCOUNT", "acct"), ("CPL_AZURE_USE_HTTPS", "NO")];
        assert_eq!(azure_url(&http, "/vsiaz/c/b.tif"), "http://acct.blob.core.windows.net/c/b.tif");
        let azurite = [("AZURE_STORAGE_ACCOUNT", "acct"), ("CPL_AZURE_ENDPOINT", "http://127.0.0.1:10000/devstoreaccount1/")];
        assert_eq!(azure_url(&azurite, "/vsiaz/c/b.tif"), "http://127.0.0.1:10000/devstoreaccount1/c/b.tif");
        let cs = [
            ("AZURE_STORAGE_ACCOUNT", "acct"),
            ("CPL_AZURE_ENDPOINT", "http://ignored.example"),
            ("AZURE_STORAGE_CONNECTION_STRING", "DefaultEndpointsProtocol=http;AccountName=a;BlobEndpoint=http://127.0.0.1:9/a;"),
        ];
        assert_eq!(azure_url(&cs, "/vsiaz/c/b.tif"), "http://127.0.0.1:9/a/c/b.tif", "BlobEndpoint wins");
    }

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
        assert_eq!(
            Location::parse("/vsizip/{/data/x.zip.d/archive.bin}/dir/b.shp"),
            Location::Zip { archive: Box::new(Location::Local("/data/x.zip.d/archive.bin".into())), inner: Some("dir/b.shp".into()) }
        );
    }

    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// What a decoy member holds: corrupted after writing, so that reading it fails its CRC.
    const DECOY: &[u8] = b"a decoy member that is never to be read";

    /// Writes a zip archive of `(name, bytes, deflated)` members, plus a corrupt decoy, into a
    /// temporary directory.
    fn archive(members: &[(&str, &[u8], bool)]) -> (tempfile::TempDir, String) {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buf);
        for (name, bytes, deflated) in members.iter().chain([&("decoy.bin", DECOY, false)]) {
            let method = if *deflated { zip::CompressionMethod::Deflated } else { zip::CompressionMethod::Stored };
            zip.start_file(*name, SimpleFileOptions::default().compression_method(method)).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        let mut bytes = buf.into_inner();
        let at = bytes.windows(DECOY.len()).position(|w| w == DECOY).unwrap();
        bytes[at] ^= 0xFF;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.zip");
        std::fs::write(&path, bytes).unwrap();
        (dir, path.to_str().unwrap().to_string())
    }

    fn with_options<T>(options: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        crate::config::push_scope(options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect());
        let out = f();
        crate::config::pop_scope();
        out
    }

    fn files_under(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(dir).unwrap().to_str().unwrap().replace('\\', "/"));
                }
            }
        }
        out.sort();
        out
    }

    /// Finding 7: extracting the whole archive would read the decoy and fail its CRC (or fill the
    /// disk with what else the archive holds); only the member and its sidecars are read.
    #[test]
    fn reads_only_the_member_and_its_sidecars() {
        let tif = vec![7u8; 5000];
        let (_d, zip) = archive(&[
            ("dir/a.tif", &tif, true),
            ("dir/A.TFW", b"1\n0\n0\n-1\n0\n0\n", false),
            ("dir/a.tif.aux.xml", b"<PAMDataset/>", true),
            ("dir/ab.tif", b"not a sidecar", false),
            ("other/a.tfw", b"another directory", false),
        ]);
        for path in [format!("/vsizip/{zip}/dir/a.tif"), format!("/vsizip/{{{zip}}}/dir/a.tif"), format!("{zip}!dir/a.tif")] {
            let local = resolve(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(std::fs::read(local.path()).unwrap(), tif, "{path}");
            let root = local.path().parent().unwrap().parent().unwrap();
            assert_eq!(files_under(root), ["dir/A.TFW", "dir/a.tif", "dir/a.tif.aux.xml"], "{path}");
        }
        let err = resolve(&format!("/vsizip/{zip}/decoy.bin")).unwrap_err().to_string();
        assert!(err.contains("checksum") || err.contains("CRC"), "the decoy is corrupt: {err}");
        let err = resolve(&format!("/vsizip/{zip}/../a.zip")).unwrap_err();
        assert!(matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound), "{err}");
    }

    #[test]
    fn reads_a_directory_member_and_the_main_dataset() {
        let (_d, zip) = archive(&[("store.zarr/.zgroup", b"{}", true), ("store.zarr/b/0.0", &[1, 2, 3], false), ("x.json", b"{}", true)]);
        let local = resolve(&format!("/vsizip/{zip}/store.zarr")).unwrap();
        assert_eq!(files_under(local.path()), [".zgroup", "b/0.0"]);
        // The archive alone: its main dataset (a shapefile before a Zarr store or a JSON file).
        let (_d, zip) = archive(&[("b.shp", b"shp", true), ("b.dbf", b"dbf", false), ("b.prj", b"prj", true), ("x.json", b"{}", true)]);
        let local = resolve(&zip).unwrap();
        assert_eq!(local.path().file_name().unwrap(), "b.shp");
        assert_eq!(files_under(local.path().parent().unwrap()), ["b.dbf", "b.prj", "b.shp"]);
        assert_eq!(Location::parse(&zip).dataset_name(), "b.shp");
    }

    /// A member whose compression ratio is above the limit is refused before it is
    /// read; a member declaring more than deflate can reach is refused by default, as by GDAL.
    #[test]
    fn a_zip_bomb_is_refused() {
        let zeros = vec![0u8; 1 << 20];
        let (_d, zip) = archive(&[("bomb.tif", &zeros, true)]);
        let path = format!("/vsizip/{zip}/bomb.tif");
        let err = with_options(&[("TERRIO_ZIP_MAX_RATIO", "100")], || resolve(&path)).unwrap_err().to_string();
        assert!(err.contains("ratio"), "{err}");
        assert_eq!(std::fs::read(resolve(&path).unwrap().path()).unwrap().len(), 1 << 20, "under the default ratio");
        // A central directory claiming 2^31 bytes for the same 1 KiB of deflate.
        let mut bytes = std::fs::read(&zip).unwrap();
        let cd = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        bytes[cd + 24..cd + 28].copy_from_slice(&(1u32 << 31).to_le_bytes());
        std::fs::write(&zip, bytes).unwrap();
        let err = resolve(&path).unwrap_err().to_string();
        assert!(err.contains("ratio"), "{err}");
    }

    #[test]
    fn zip_limits_and_symlinks() {
        let (_d, zip) = archive(&[("a.tif", &[1u8; 2000], false), ("b.tif", b"b", false), ("c.tif", b"c", false)]);
        let path = format!("/vsizip/{zip}/a.tif");
        let err = with_options(&[("TERRIO_ZIP_MAX_ENTRIES", "3")], || resolve(&path)).unwrap_err().to_string();
        assert!(err.contains("4 entries"), "{err}");
        let err = with_options(&[("TERRIO_ZIP_MAX_SIZE", "1999")], || resolve(&path)).unwrap_err().to_string();
        assert!(err.contains("TERRIO_ZIP_MAX_SIZE"), "{err}");
        assert!(with_options(&[("TERRIO_ZIP_MAX_ENTRIES", "4"), ("TERRIO_ZIP_MAX_SIZE", "2000")], || resolve(&path)).is_ok());

        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        w.add_symlink("link.tif", "/etc/passwd", SimpleFileOptions::default()).unwrap();
        w.finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.zip");
        std::fs::write(&link, buf.into_inner()).unwrap();
        let err = resolve(&format!("/vsizip/{}/link.tif", link.display())).unwrap_err().to_string();
        assert!(err.contains("symbolic link"), "{err}");
    }
}
