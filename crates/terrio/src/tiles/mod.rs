//! Tile archives: PMTiles (v3) and MBTiles, holding raster tiles (PNG, JPEG, WebP) or Mapbox
//! Vector Tiles, in the Web Mercator tiling scheme (EPSG:3857, 256-pixel tiles).
//!
//! They open as GDAL's MBTiles, PMTiles and MVT drivers open them:
//!
//! - raster tiles as a raster in EPSG:3857 at the resolution of the highest zoom level (or of
//!   [`TileOpenOptions::zoom_level`]), over the archive's bounds, with the lower zoom levels as
//!   overviews (read by window through [`crate::LazyRaster`], fetching only the tiles needed);
//! - vector tiles as one vector layer per MVT layer, with the features of every tile of the zoom
//!   level, their geometries in EPSG:3857 and clipped to their tile (as GDAL's `CLIP` option
//!   does), and the fields the archive's metadata declares (`vector_layers`, `tilestats`).
//!
//! PMTiles archives, local or remote (HTTP(S), `s3://`, `gs://`, `az://`), are read in place: the
//! header and root directory first, then only the leaf directories and tiles needed, by range
//! requests. MBTiles files are SQLite databases, read by terrio's own SQLite reader.
//!
//! Rasters are written as GDAL writes them (reprojected to EPSG:3857 at the zoom level whose
//! resolution is closest to the source's), with lower zoom levels as overviews down to the one
//! whose tiles cover the raster in one tile; vector layers as MVT tiles over a range of zoom
//! levels, clipped to each tile (with a buffer) and quantized to the tile grid, as GDAL's MVT
//! writer does. See [`TileWriteOptions`].

pub(crate) mod clip;
pub(crate) mod mbtiles;
pub(crate) mod mvt;
pub(crate) mod pmtiles;
pub(crate) mod raster;
pub(crate) mod source;
pub(crate) mod valid;
pub(crate) mod vector;

use crate::{Error, Result};
use source::ByteSource;

/// Half the width of the Web Mercator world, in metres: `6378137 * pi`.
pub(crate) const MAX_GM: f64 = 6378137.0 * std::f64::consts::PI;

/// Longitude and latitude (degrees) to Web Mercator, as GDAL computes them.
pub(crate) fn lonlat_to_mercator(lon: f64, lat: f64) -> (f64, f64) {
    let r = 6378137.0;
    (r * lon / 180.0 * std::f64::consts::PI, r * (std::f64::consts::FRAC_PI_4 + 0.5 * lat / 180.0 * std::f64::consts::PI).tan().ln())
}

/// Web Mercator to longitude and latitude (degrees), as GDAL computes them.
pub(crate) fn mercator_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let r = 6378137.0;
    (x / r * 180.0 / std::f64::consts::PI, (2.0 * (y / r).exp().atan() - std::f64::consts::FRAC_PI_2) * 180.0 / std::f64::consts::PI)
}

/// The kind of tiles an archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileFormat {
    /// Mapbox Vector Tiles.
    Mvt,
    /// PNG images.
    Png,
    /// JPEG images.
    Jpeg,
    /// WebP images.
    Webp,
    /// AVIF images (not decoded).
    Avif,
    /// Not declared.
    Unknown,
}

impl TileFormat {
    /// The name archives use (`pbf`, `png`, `jpg`, `webp`, `avif`).
    pub fn name(self) -> &'static str {
        match self {
            TileFormat::Mvt => "pbf",
            TileFormat::Png => "png",
            TileFormat::Jpeg => "jpg",
            TileFormat::Webp => "webp",
            TileFormat::Avif => "avif",
            TileFormat::Unknown => "unknown",
        }
    }

    fn from_mbtiles(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "pbf" | "mvt" => TileFormat::Mvt,
            "png" => TileFormat::Png,
            "jpg" | "jpeg" => TileFormat::Jpeg,
            "webp" => TileFormat::Webp,
            "avif" => TileFormat::Avif,
            _ => TileFormat::Unknown,
        }
    }

    /// The format of a tile from its first bytes.
    fn sniff(b: &[u8]) -> Self {
        if b.starts_with(b"\x89PNG") {
            TileFormat::Png
        } else if b.starts_with(&[0xff, 0xd8]) {
            TileFormat::Jpeg
        } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
            TileFormat::Webp
        } else if b.starts_with(&[0x1f, 0x8b]) || b.first() == Some(&0x1a) {
            TileFormat::Mvt
        } else {
            TileFormat::Unknown
        }
    }
}

/// How tile archives are opened, as GDAL's open options of the MBTiles, PMTiles and MVT drivers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TileOpenOptions {
    /// The zoom level read as full resolution (rasters) or whose tiles give the features
    /// (vectors). By default, the archive's highest (`ZOOM_LEVEL`).
    pub zoom_level: Option<u8>,
    /// Whether to clip vector features to their tile (`CLIP`, default yes).
    pub clip: Option<bool>,
}

impl TileOpenOptions {
    /// Sets an option by GDAL's name (`ZOOM_LEVEL`, `CLIP`). Returns whether the key is one of them.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        match key.to_ascii_uppercase().as_str() {
            "ZOOM_LEVEL" => {
                self.zoom_level = Some(value.trim().parse().map_err(|_| Error::InvalidArgument(format!("invalid ZOOM_LEVEL '{value}'")))?)
            }
            "CLIP" => self.clip = Some(parse_bool(value)?),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

pub(crate) fn parse_bool(v: &str) -> Result<bool> {
    match v.trim().to_ascii_uppercase().as_str() {
        "YES" | "TRUE" | "ON" | "1" => Ok(true),
        "NO" | "FALSE" | "OFF" | "0" => Ok(false),
        _ => Err(Error::InvalidArgument(format!("invalid boolean '{v}'"))),
    }
}

/// How rasters and vector layers are written as tiles: GDAL's creation options of the MBTiles,
/// PMTiles and MVT drivers.
#[derive(Debug, Clone, PartialEq)]
pub struct TileWriteOptions {
    /// The lowest zoom level written. Rasters: by default, the one whose single tile covers the
    /// raster; vectors: 0.
    pub min_zoom: Option<u8>,
    /// The highest zoom level. Rasters: by default, the one closest to the source's resolution
    /// (see [`TileWriteOptions::zoom_level_strategy`]); vectors: 5, as in GDAL.
    pub max_zoom: Option<u8>,
    /// Rasters: `AUTO` (the zoom level whose resolution is closest), `LOWER` or `UPPER`.
    pub zoom_level_strategy: String,
    /// Rasters: the resampling of the reprojection and overviews (`near`, `bilinear` (default),
    /// `cubic`, `average`...).
    pub resampling: Option<String>,
    /// Rasters: the tile format, `PNG` (default), `JPEG` or `WEBP` (lossless).
    pub tile_format: Option<String>,
    /// Rasters: JPEG quality (default 75).
    pub quality: u8,
    /// The tileset name (by default, the file name without its extension).
    pub name: Option<String>,
    /// The tileset description.
    pub description: Option<String>,
    /// `overlay` (default) or `baselayer`.
    pub tileset_type: String,
    /// Vectors: the tile extent in integer units (default 4096).
    pub extent: u32,
    /// Vectors: the buffer around tiles, in tile units (default 80, i.e. 5 pixels of 256).
    pub buffer: u32,
    /// Vectors: the simplification tolerance, in tile units, for lines and polygons (default 0).
    pub simplification: f64,
    /// Vectors: the simplification at the highest zoom level (default: `simplification`).
    pub simplification_max_zoom: Option<f64>,
    /// Vectors: the largest compressed tile size in bytes (default 500000); larger tiles are
    /// encoded at lower resolution, then lose their smallest features, as in GDAL.
    pub max_size: usize,
    /// Vectors: the most features in a tile (default 200000).
    pub max_features: usize,
}

impl Default for TileWriteOptions {
    fn default() -> Self {
        Self {
            min_zoom: None,
            max_zoom: None,
            zoom_level_strategy: "AUTO".into(),
            resampling: None,
            tile_format: None,
            quality: 75,
            name: None,
            description: None,
            tileset_type: "overlay".into(),
            extent: 4096,
            buffer: 80,
            simplification: 0.0,
            simplification_max_zoom: None,
            max_size: 500_000,
            max_features: 200_000,
        }
    }
}

impl TileWriteOptions {
    /// Sets an option by GDAL's name (`MINZOOM`, `MAXZOOM`, `ZOOM_LEVEL_STRATEGY`, `RESAMPLING`,
    /// `TILE_FORMAT`, `QUALITY`, `NAME`, `DESCRIPTION`, `TYPE`, `EXTENT`, `BUFFER`,
    /// `SIMPLIFICATION`, `SIMPLIFICATION_MAX_ZOOM`, `MAX_SIZE`, `MAX_FEATURES`). Returns whether the
    /// key is one of them.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        let num = |v: &str| v.trim().parse::<f64>().map_err(|_| Error::InvalidArgument(format!("invalid value '{v}' for {key}")));
        let zoom = |v: &str| {
            v.trim().parse::<u8>().ok().filter(|z| *z <= 30).ok_or_else(|| Error::InvalidArgument(format!("invalid zoom level '{v}'")))
        };
        match key.to_ascii_uppercase().as_str() {
            "MINZOOM" => self.min_zoom = Some(zoom(value)?),
            "MAXZOOM" => self.max_zoom = Some(zoom(value)?),
            "ZOOM_LEVEL_STRATEGY" => self.zoom_level_strategy = value.to_ascii_uppercase(),
            "RESAMPLING" => self.resampling = Some(value.to_string()),
            "TILE_FORMAT" => self.tile_format = Some(value.to_ascii_uppercase()),
            "QUALITY" => self.quality = num(value)?.clamp(1.0, 100.0) as u8,
            "NAME" => self.name = Some(value.to_string()),
            "DESCRIPTION" => self.description = Some(value.to_string()),
            "TYPE" => self.tileset_type = value.to_string(),
            "EXTENT" => self.extent = num(value)?.max(1.0) as u32,
            "BUFFER" => self.buffer = num(value)?.max(0.0) as u32,
            "SIMPLIFICATION" => self.simplification = num(value)?,
            "SIMPLIFICATION_MAX_ZOOM" => self.simplification_max_zoom = Some(num(value)?),
            "MAX_SIZE" => self.max_size = num(value)?.max(1.0) as usize,
            "MAX_FEATURES" => self.max_features = num(value)?.max(1.0) as usize,
            // GDAL options that make no difference here.
            "FORMAT" | "COMPRESS" | "BLOCKSIZE" | "ZLEVEL" | "BOUNDS" | "WRITE_BOUNDS" => {}
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// The archive formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// PMTiles v3.
    PMTiles,
    /// MBTiles.
    MBTiles,
}

/// Whether a path names a tile archive, by its extension.
pub(crate) fn archive_kind(path: &str) -> Option<ArchiveKind> {
    let lower = path.split(['?', '#']).next().unwrap_or(path).to_ascii_lowercase();
    if lower.ends_with(".pmtiles") {
        Some(ArchiveKind::PMTiles)
    } else if lower.ends_with(".mbtiles") {
        Some(ArchiveKind::MBTiles)
    } else {
        None
    }
}

enum Inner {
    Pm(Box<pmtiles::Reader>),
    Mb(mbtiles::MbTiles),
}

/// A PMTiles or MBTiles archive opened for reading tiles.
pub struct TileArchive {
    inner: Inner,
    /// Keeps a local copy (of an archive member) alive.
    _local: Option<crate::vfs::LocalPath>,
}

impl std::fmt::Debug for TileArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileArchive")
            .field("kind", &self.kind())
            .field("format", &self.format())
            .field("zooms", &self.zoom_range())
            .finish_non_exhaustive()
    }
}

impl TileArchive {
    /// Opens an archive: a local path, or a URL, `s3://`, `gs://` or `az://` object. PMTiles are
    /// read in place (only the header and root directory now); MBTiles are read whole.
    pub fn open(path: &str) -> Result<Self> {
        let kind = archive_kind(&crate::vfs::Location::parse(path).file_name()).or_else(|| archive_kind(path));
        match kind {
            Some(ArchiveKind::MBTiles) => {
                let local = crate::vfs::resolve(path)?;
                let bytes = std::fs::read(local.path())?;
                Ok(Self { inner: Inner::Mb(mbtiles::MbTiles::from_bytes(bytes)?), _local: None })
            }
            _ => {
                let (mut source, local) = ByteSource::open(path)?;
                let head = source.read_at(0, source.len().min(16))?;
                if head.starts_with(b"SQLite format 3\0") {
                    drop(source);
                    let local = crate::vfs::resolve(path)?;
                    return Ok(Self { inner: Inner::Mb(mbtiles::MbTiles::from_bytes(std::fs::read(local.path())?)?), _local: None });
                }
                Ok(Self { inner: Inner::Pm(Box::new(pmtiles::Reader::open(source)?)), _local: local })
            }
        }
    }

    /// Opens an archive held in memory (PMTiles or MBTiles, told apart by their content).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        if bytes.starts_with(b"SQLite format 3\0") {
            return Ok(Self { inner: Inner::Mb(mbtiles::MbTiles::from_bytes(bytes)?), _local: None });
        }
        Ok(Self { inner: Inner::Pm(Box::new(pmtiles::Reader::open(ByteSource::Memory(bytes))?)), _local: None })
    }

    /// PMTiles or MBTiles.
    pub fn kind(&self) -> ArchiveKind {
        match self.inner {
            Inner::Pm(_) => ArchiveKind::PMTiles,
            Inner::Mb(_) => ArchiveKind::MBTiles,
        }
    }

    /// Whether the archive is read in place from a remote file.
    pub fn is_remote(&self) -> bool {
        matches!(&self.inner, Inner::Pm(r) if r.is_remote())
    }

    /// The tiles' format.
    pub fn format(&self) -> TileFormat {
        match &self.inner {
            Inner::Pm(r) => match r.header.tile_type {
                pmtiles::TILE_MVT => TileFormat::Mvt,
                pmtiles::TILE_PNG => TileFormat::Png,
                pmtiles::TILE_JPEG => TileFormat::Jpeg,
                pmtiles::TILE_WEBP => TileFormat::Webp,
                pmtiles::TILE_AVIF => TileFormat::Avif,
                _ => TileFormat::Unknown,
            },
            Inner::Mb(m) => match m.item("format").map(TileFormat::from_mbtiles) {
                Some(f) if f != TileFormat::Unknown => f,
                _ => m.zoom_range().and_then(|(_, z)| m.any_tile(z)).map(TileFormat::sniff).unwrap_or(TileFormat::Unknown),
            },
        }
    }

    /// Whether the tiles are vector tiles.
    pub fn is_vector(&self) -> bool {
        self.format() == TileFormat::Mvt
    }

    /// The lowest and highest zoom levels.
    pub fn zoom_range(&self) -> (u8, u8) {
        match &self.inner {
            Inner::Pm(r) => (r.header.min_zoom.min(30), r.header.max_zoom.min(30)),
            Inner::Mb(m) => m.zoom_range().unwrap_or((0, 0)),
        }
    }

    /// The bounds `(min_lon, min_lat, max_lon, max_lat)` the archive declares.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        match &self.inner {
            Inner::Pm(r) => Some(r.header.bounds()),
            Inner::Mb(m) => {
                let v: Vec<f64> = m.item("bounds")?.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                // GDAL ignores bounds it finds invalid.
                let ok = v.len() == 4
                    && v[0].abs() <= 180.0
                    && v[2].abs() <= 180.0
                    && v[1].abs() < 89.99
                    && v[3].abs() < 89.99
                    && v[0] <= v[2]
                    && v[1] <= v[3];
                ok.then(|| [v[0], v[1], v[2], v[3]])
            }
        }
    }

    /// The metadata as `(key, value)` pairs: MBTiles' `metadata` table, or the top-level items of
    /// PMTiles' JSON metadata (objects and arrays as JSON text).
    pub fn metadata(&self) -> Vec<(String, String)> {
        match &self.inner {
            Inner::Mb(m) => m.metadata.clone(),
            Inner::Pm(r) => match &r.metadata {
                serde_json::Value::Object(o) => o
                    .iter()
                    .map(|(k, v)| {
                        let s = match v {
                            serde_json::Value::String(s) => s.clone(),
                            v => v.to_string(),
                        };
                        (k.clone(), s)
                    })
                    .collect(),
                _ => Vec::new(),
            },
        }
    }

    /// The JSON metadata describing vector tiles: PMTiles' metadata, or MBTiles' `json` item.
    pub(crate) fn json(&self) -> serde_json::Value {
        match &self.inner {
            Inner::Pm(r) => {
                let mut v = r.metadata.clone();
                // Tippecanoe's archives nest vector_layers in a "json" string, as in MBTiles.
                if v.get("vector_layers").is_none()
                    && let Some(inner) = v.get("json").and_then(|j| j.as_str()).and_then(|s| serde_json::from_str(s).ok())
                {
                    v = inner;
                }
                v
            }
            Inner::Mb(m) => m.item("json").and_then(|s| serde_json::from_str(s).ok()).unwrap_or(serde_json::Value::Null),
        }
    }

    /// A tile's bytes (decompressed when the archive compresses tiles), `None` when the archive
    /// does not have it. `y` counts rows from the top, as in XYZ tile URLs.
    pub fn tile(&mut self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        match &mut self.inner {
            Inner::Pm(r) => r.tile(z, x, y),
            Inner::Mb(m) => Ok(m.tile(z, x, y).map(|b| b.to_vec())),
        }
    }

    /// Several tiles, fetched in as few requests as possible.
    pub(crate) fn tiles(&mut self, wanted: &[(u8, u32, u32)]) -> Result<Vec<Option<Vec<u8>>>> {
        match &mut self.inner {
            Inner::Pm(r) => r.tiles(wanted),
            Inner::Mb(m) => Ok(wanted.iter().map(|&(z, x, y)| m.tile(z, x, y).map(|b| b.to_vec())).collect()),
        }
    }

    /// The tiles `(x, y)` the archive has at zoom level `z`.
    pub fn tiles_at_zoom(&mut self, z: u8) -> Result<Vec<(u32, u32)>> {
        match &mut self.inner {
            Inner::Pm(r) => r.tiles_at_zoom(z),
            Inner::Mb(m) => Ok(m.tiles_at_zoom(z)),
        }
    }

    /// The driver name, as GDAL names it.
    pub fn driver_name(&self) -> &'static str {
        match self.kind() {
            ArchiveKind::PMTiles => "PMTiles",
            ArchiveKind::MBTiles => "MBTiles",
        }
    }
}

/// Whether a path (or URL) names a PMTiles or MBTiles archive.
pub fn is_archive(path: &str) -> bool {
    let loc = crate::vfs::Location::parse(path);
    !matches!(loc, crate::vfs::Location::Zip { .. }) && archive_kind(&loc.file_name()).is_some()
}

/// Opens a tile archive as a dataset: a raster for image tiles (at the zoom level of
/// `options.tiles`), vector layers for vector tiles (those of `options.layers`, from the tiles
/// that intersect `options.spatial_filter`, in EPSG:3857).
pub fn open_dataset(path: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    let archive = TileArchive::open(path)?;
    let name = crate::vfs::Location::parse(path).file_name();
    archive_dataset(archive, &name, options)
}

/// A dataset of an opened archive.
pub(crate) fn archive_dataset(mut archive: TileArchive, name: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    if archive.is_vector() {
        let layers = vector::read_layers(&mut archive, &options.tiles, &options.layers, options.spatial_filter)?;
        let driver = match archive.kind() {
            ArchiveKind::PMTiles => crate::formats::VectorDriver::PMTiles,
            ArchiveKind::MBTiles => crate::formats::VectorDriver::MBTiles,
        };
        let names: Vec<String> = layers.iter().map(|l| l.name.clone()).collect();
        let mut ds = crate::dataset::finish_vector(layers, name, driver);
        // Every layer keeps the name the tiles give it.
        for (l, n) in ds.layers.iter_mut().zip(names) {
            l.name = n;
        }
        return Ok(crate::Dataset::Vector(ds));
    }
    let mut r = raster::TileRaster::new(archive, options.tiles.zoom_level)?;
    Ok(crate::Dataset::Raster(r.read_all()?))
}

/// Opens a tile archive held in memory as a dataset.
pub(crate) fn open_dataset_bytes(bytes: Vec<u8>, name: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    archive_dataset(TileArchive::from_bytes(bytes)?, name, options)
}

// ─── Writing ────────────────────────────────────────────────────────────────────────────────────

/// Encodes a tile pyramid of image tiles as an archive, with the metadata GDAL writes.
pub(crate) fn write_raster_archive(rt: &raster::RasterTiles, o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    let b = rt.bounds;
    let name = o.name.clone().unwrap_or_else(|| name.to_string());
    let format = rt.format.name();
    let md: Vec<(String, String)> = vec![
        ("name".into(), name.clone()),
        ("type".into(), o.tileset_type.clone()),
        ("description".into(), o.description.clone().unwrap_or(name)),
        ("version".into(), "1.1".into()),
        ("format".into(), format.into()),
        ("bounds".into(), format!("{},{},{},{}", g17(b[0]), g17(b[1]), g17(b[2]), g17(b[3]))),
        ("minzoom".into(), rt.min_zoom.to_string()),
        ("maxzoom".into(), rt.max_zoom.to_string()),
    ];
    match kind {
        ArchiveKind::MBTiles => mbtiles::write(&md, rt.tiles.clone()),
        ArchiveKind::PMTiles => {
            let json: serde_json::Map<String, serde_json::Value> = md.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
            let tile_type = match rt.format {
                TileFormat::Jpeg => pmtiles::TILE_JPEG,
                TileFormat::Webp => pmtiles::TILE_WEBP,
                _ => pmtiles::TILE_PNG,
            };
            let tiles = rt.tiles.iter().map(|(z, x, y, d)| pmtiles::OutTile { z: *z, x: *x, y: *y, data: d.clone() }).collect();
            let params = pmtiles::ArchiveParams {
                tile_type,
                tile_compression: pmtiles::COMPRESSION_NONE,
                min_zoom: rt.min_zoom,
                max_zoom: rt.max_zoom,
                bounds: b,
                center: ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0, rt.max_zoom),
            };
            pmtiles::write_archive(tiles, serde_json::Value::Object(json).to_string().as_bytes(), &params)
        }
    }
}

/// Formats a number as C's `%.17g` does (enough digits to round-trip).
fn g17(v: f64) -> String {
    format!("{v}")
}

/// Encodes vector tiles as an archive, with the metadata GDAL writes: MBTiles' items and `json`
/// (`vector_layers`, `tilestats`); for PMTiles, the same items as JSON members.
pub(crate) fn write_vector_archive(vt: &vector::VectorTiles, o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    let mut md = vector::metadata_items(vt, o, name);
    let gz = |d: &[u8]| pmtiles::compress(d, pmtiles::COMPRESSION_GZIP);
    match kind {
        ArchiveKind::MBTiles => {
            md.push(("scheme".into(), "tms".into()));
            md.push(("json".into(), serde_json::to_string_pretty(&vt.json).unwrap_or_default()));
            let mut tiles = Vec::with_capacity(vt.tiles.len());
            for (z, x, y, d) in &vt.tiles {
                tiles.push((*z, *x, *y, gz(d)?));
            }
            mbtiles::write(&md, tiles)
        }
        ArchiveKind::PMTiles => {
            let mut json: serde_json::Map<String, serde_json::Value> =
                md.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
            if let serde_json::Value::Object(o) = &vt.json {
                for (k, v) in o {
                    json.insert(k.clone(), v.clone());
                }
            }
            let mut tiles = Vec::with_capacity(vt.tiles.len());
            for (z, x, y, d) in &vt.tiles {
                tiles.push(pmtiles::OutTile { z: *z, x: *x, y: *y, data: gz(d)? });
            }
            let b = vt.bounds;
            let params = pmtiles::ArchiveParams {
                tile_type: pmtiles::TILE_MVT,
                tile_compression: pmtiles::COMPRESSION_GZIP,
                min_zoom: vt.min_zoom,
                max_zoom: vt.max_zoom,
                bounds: b,
                center: ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0, vt.min_zoom),
            };
            pmtiles::write_archive(tiles, serde_json::Value::Object(json).to_string().as_bytes(), &params)
        }
    }
}

/// Writes a raster as a PMTiles or MBTiles archive (see [`TileWriteOptions`]).
pub(crate) fn raster_to_bytes(ds: &crate::RasterDataset, o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    let rt = raster::make_tiles(ds, o)?;
    write_raster_archive(&rt, o, name, kind)
}

/// Writes vector layers as a PMTiles or MBTiles archive of vector tiles.
pub(crate) fn vector_to_bytes(layers: &[wbvector::Layer], o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    let vt = vector::make_tiles(layers, o)?;
    write_vector_archive(&vt, o, name, kind)
}

/// Writes vector layers as a directory of MVT tiles (`z/x/y.pbf`, gzipped) with a
/// `metadata.json`, as GDAL's MVT driver does.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_mvt_dir(layers: &[wbvector::Layer], o: &TileWriteOptions, path: &std::path::Path) -> Result<()> {
    let vt = vector::make_tiles(layers, o)?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("tiles");
    if path.exists() {
        return Err(Error::InvalidArgument(format!("'{}' already exists", path.display())));
    }
    std::fs::create_dir_all(path)?;
    let mut md: serde_json::Map<String, serde_json::Value> =
        vector::metadata_items(&vt, o, name).into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
    md.insert("json".into(), serde_json::Value::String(serde_json::to_string_pretty(&vt.json).unwrap_or_default()));
    std::fs::write(path.join("metadata.json"), serde_json::to_string_pretty(&serde_json::Value::Object(md)).unwrap_or_default())?;
    for (z, x, y, d) in &vt.tiles {
        let dir = path.join(z.to_string()).join(x.to_string());
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{y}.pbf")), pmtiles::compress(d, pmtiles::COMPRESSION_GZIP)?)?;
    }
    Ok(())
}

/// Reads a directory of MVT tiles (`z/x/y.pbf` with a `metadata.json`), as GDAL's MVT driver
/// does: the tiles of one zoom level (the highest by default).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read_mvt_dir(path: &std::path::Path, options: &crate::VectorOpenOptions) -> Result<Vec<wbvector::Layer>> {
    let text = std::fs::read_to_string(path.join("metadata.json"))?;
    let mut md: serde_json::Value = serde_json::from_str(&text).map_err(|e| Error::InvalidData(format!("metadata.json: {e}")))?;
    let zooms: Vec<u8> = std::fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
        .collect();
    let (min_z, max_z) = (zooms.iter().copied().min().unwrap_or(0), zooms.iter().copied().max().unwrap_or(0));
    let mut tiles = Vec::new();
    let z = options.tiles.zoom_level.unwrap_or(max_z);
    let zdir = path.join(z.to_string());
    if zdir.is_dir() {
        for xe in std::fs::read_dir(&zdir)?.filter_map(|e| e.ok()) {
            let Some(x) = xe.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            for ye in std::fs::read_dir(xe.path())?.filter_map(|e| e.ok()) {
                let name = ye.file_name();
                let Some(y) = name.to_str().and_then(|s| s.split('.').next()).and_then(|s| s.parse::<u32>().ok()) else { continue };
                tiles.push((z, x, y, std::fs::read(ye.path())?));
            }
        }
    }
    // An MBTiles file in memory gives the same reading path as archives.
    let mut items: Vec<(String, String)> = Vec::new();
    if let serde_json::Value::Object(o) = &mut md {
        for (k, v) in o.iter() {
            items.push((k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())));
        }
    }
    items.retain(|(k, _)| k != "minzoom" && k != "maxzoom");
    items.push(("minzoom".into(), min_z.to_string()));
    items.push(("maxzoom".into(), max_z.to_string()));
    let mut archive = TileArchive::from_bytes(mbtiles::write(&items, tiles)?)?;
    vector::read_layers(&mut archive, &options.tiles, &options.layers, options.spatial_filter)
}

/// Parses raster creation options (and the compression, as the tile format) into tile options.
pub(crate) fn raster_write_options(options: &crate::RasterWriteOptions) -> Result<TileWriteOptions> {
    let mut o = TileWriteOptions::default();
    if let Some(c) = &options.compression {
        o.tile_format = Some(c.to_ascii_uppercase());
    }
    for (k, v) in &options.creation_options {
        if !o.set(k, v)? {
            return Err(Error::InvalidArgument(format!("unknown creation option '{k}' for tile archives")));
        }
    }
    Ok(o)
}

/// The file name without its extension, as GDAL names tilesets.
fn stem(path: &str) -> String {
    std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("tiles").to_string()
}

/// Writes a raster as a PMTiles or MBTiles archive.
pub(crate) fn write_raster(
    ds: &crate::RasterDataset,
    path: &str,
    driver: crate::formats::RasterDriver,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    let kind = if driver == crate::formats::RasterDriver::MBTiles { ArchiveKind::MBTiles } else { ArchiveKind::PMTiles };
    let bytes = raster_to_bytes(ds, &raster_write_options(options)?, &stem(path), kind)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Writes vector layers as a PMTiles or MBTiles archive, or a directory of MVT tiles.
pub(crate) fn write_vector(
    layers: &[wbvector::Layer],
    path: &str,
    driver: crate::formats::VectorDriver,
    o: &TileWriteOptions,
) -> Result<()> {
    use crate::formats::VectorDriver as V;
    let kind = match driver {
        V::MBTiles => ArchiveKind::MBTiles,
        V::PMTiles => ArchiveKind::PMTiles,
        _ => {
            #[cfg(not(target_arch = "wasm32"))]
            return write_mvt_dir(layers, o, std::path::Path::new(path));
            #[cfg(target_arch = "wasm32")]
            return Err(Error::Unsupported("directories of MVT tiles cannot be written in WebAssembly builds".into()));
        }
    };
    std::fs::write(path, vector_to_bytes(layers, o, &stem(path), kind)?)?;
    Ok(())
}

/// Whether bytes look like a Mapbox Vector Tile (gzipped, or starting with a layer).
pub(crate) fn is_mvt(b: &[u8]) -> bool {
    b.starts_with(&[0x1f, 0x8b]) || b.first() == Some(&0x1a)
}

/// Reads a local MVT tile (`.mvt`, `.mvt.gz`, or a `.pbf` that is not OpenStreetMap data) or a
/// directory of tiles with a `metadata.json`. `None` when the path is neither.
pub(crate) fn open_mvt_local(path: &str, options: &crate::VectorOpenOptions) -> Option<Result<Vec<wbvector::Layer>>> {
    let p = std::path::Path::new(path);
    #[cfg(not(target_arch = "wasm32"))]
    if p.is_dir() {
        if p.join("metadata.json").is_file() {
            return Some(read_mvt_dir(p, options));
        }
        // A zoom level directory of such a directory.
        let zoom = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u8>().ok());
        if let (Some(z), Some(parent)) = (zoom, p.parent())
            && parent.join("metadata.json").is_file()
        {
            let mut o = options.clone();
            o.tiles.zoom_level = Some(z);
            return Some(read_mvt_dir(parent, &o));
        }
    }
    let lower = path.to_ascii_lowercase();
    if !(lower.ends_with(".mvt") || lower.ends_with(".mvt.gz") || (lower.ends_with(".pbf") && !lower.ends_with(".osm.pbf"))) || !p.is_file()
    {
        return None;
    }
    let bytes = match std::fs::read(p) {
        Ok(b) => b,
        Err(e) => return Some(Err(e.into())),
    };
    if !is_mvt(&bytes) {
        return None;
    }
    Some(vector::read_tile_file(&bytes, path, &options.tiles))
}

/// The layer names of a vector tile archive, from its metadata (without reading tiles).
pub(crate) fn layer_names(path: &str) -> Result<Vec<String>> {
    let a = TileArchive::open(path)?;
    let names: Vec<String> = a
        .json()
        .get("vector_layers")
        .and_then(|v| v.as_array())
        .map(|v| v.iter().filter_map(|l| l.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect())
        .unwrap_or_default();
    if names.is_empty() {
        return match archive_dataset(a, path, &Default::default())? {
            crate::Dataset::Vector(v) => Ok(v.layers.into_iter().map(|l| l.name).collect()),
            crate::Dataset::Raster(_) => Ok(Vec::new()),
        };
    }
    Ok(names)
}
