// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/mbtiles/mbtilesdataset.cpp; ogr/ogrsf_frmts/mvt/ogrmvtdataset.cpp; frmts/pmtiles/
// No code is translated from these sources.

//! Tile archives: PMTiles (v3) and MBTiles, holding raster tiles (PNG, JPEG, WebP, AVIF) or Mapbox
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
//!
//! Without the `tiles` feature, only the options remain, and the drivers fail with "built
//! without the `tiles` feature".

#[cfg(feature = "tiles")]
mod archive;
#[cfg(feature = "tiles")]
mod boxes;
#[cfg(feature = "tiles")]
pub(crate) mod clip;
#[cfg(feature = "tiles")]
pub(crate) mod mbtiles;
#[cfg(feature = "tiles")]
pub(crate) mod mvt;
#[cfg(feature = "tiles")]
mod overlay_envelope;
#[cfg(feature = "tiles")]
pub(crate) mod pmtiles;
#[cfg(feature = "tiles")]
pub(crate) mod pyramid;
#[cfg(feature = "tiles")]
pub(crate) mod raster;
#[cfg(feature = "tiles")]
mod repair;
#[cfg(all(test, feature = "tiles"))]
mod repair_checks;
#[cfg(feature = "tiles")]
pub(crate) mod source;
#[cfg(feature = "tiles")]
mod validity;
#[cfg(feature = "tiles")]
pub(crate) mod vector;

#[cfg(feature = "tiles")]
pub use archive::*;

use crate::{Error, Result};

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
    /// AVIF images.
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
        } else if is_avif(b) {
            TileFormat::Avif
        } else if b.starts_with(&[0x1f, 0x8b]) || b.first() == Some(&0x1a) {
            TileFormat::Mvt
        } else {
            TileFormat::Unknown
        }
    }
}

/// Whether bytes start like an AVIF file: an `ftyp` box naming the `avif` brand.
pub(crate) fn is_avif(b: &[u8]) -> bool {
    if b.len() < 16 || &b[4..8] != b"ftyp" {
        return false;
    }
    let size = (u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).min(b.len());
    // The major brand, then the compatible brands after the minor version.
    std::iter::once(&b[8..12])
        .chain(b.get(16..size).unwrap_or_default().as_chunks::<4>().0.iter().map(|c| &c[..]))
        .any(|brand| brand == b"avif" || brand == b"avis")
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
