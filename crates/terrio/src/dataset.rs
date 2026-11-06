// SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2008-2012, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.1:
//     port/cpl_vsisimple.cpp
//
// Written to match the behavior of:
//   GDAL v3.12.1:
//     frmts/gtiff/gtiffdataset_write.cpp
// No code is translated from these sources.

//! Opening, reading and writing datasets.

use crate::crs::SpatialRef;
use crate::formats::{RasterDriver, VectorDriver, extension};
use crate::vfs;
use crate::{Error, Result};
use std::path::Path;
use wbraster::Raster;
use wbvector::Layer;

/// A raster dataset read into memory.
#[derive(Debug, Clone)]
pub struct RasterDataset {
    /// The pixels and georeferencing.
    pub raster: Raster,
    /// The driver that read it.
    pub driver: RasterDriver,
    /// Whether the file declares a nodata value (the raster always carries one).
    pub has_nodata: bool,
    /// Per-band nodata in band order, when the bands of a file declare different ones (GRIB, PAM);
    /// empty means the shared raster value applies. A nonempty vector overrides
    /// has_nodata/raster.nodata, including explicit None. Clear or update this vector when
    /// changing the shared nodata fields.
    pub band_nodata: Vec<Option<f64>>,
    /// Descriptive band properties (description, metadata, colour interpretation), in band order;
    /// empty entries use driver defaults.
    pub band_metadata: Vec<BandMetadata>,
    /// Ground control points in the raster CRS, retained instead of inventing an affine grid.
    pub gcps: Vec<Gcp>,
    /// The geotransform of a rotated or south-up raster, which the raster's origin and cell sizes
    /// cannot hold (they then hold the extent of its footprint); `None` for north-up rasters.
    /// Set it with [`RasterDataset::set_geo_transform`].
    pub affine: Option<[f64; 6]>,
    /// Whether the bands hold complex numbers, as GDAL's `CInt16`, `CInt32`, `CFloat32` and
    /// `CFloat64` bands: each complex band is two bands of `raster`, its real parts then its
    /// imaginary parts, of the parts' type (`Int16` for `CInt16`). Band `k` (from 0) of the
    /// dataset is bands `2k` and `2k + 1` of the raster.
    pub complex: bool,
    /// Geolocation arrays, as GDAL's `GEOLOCATION` metadata: for swaths and other rasters
    /// georeferenced by a longitude and a latitude per pixel rather than a geotransform.
    pub geolocation: Option<Geolocation>,
    /// Metadata domains other than the default one, as GDAL reports them (`IMAGE_STRUCTURE`), for
    /// drivers that report them (JPEG 2000): the band (from 0, `None` for the dataset), the domain
    /// and its items, in GDAL's order.
    pub metadata_domains: Vec<MetadataDomain>,
    /// The color table of the first band, `(red, green, blue, alpha)` entries from index 0, for
    /// drivers that read one (paletted PNGs and GeoTIFFs); empty otherwise. GeoTIFF and COG writes
    /// of the dataset carry it, as GDAL's `CreateCopy` does.
    pub color_table: Vec<[u8; 4]>,
}

/// A metadata domain other than the default one: the band (from 0, `None` for the dataset), the
/// domain and its items (see [`RasterDataset::metadata_domains`]).
pub type MetadataDomain = (Option<usize>, String, Vec<(String, String)>);

/// Where the longitude and latitude of each pixel of a raster are, as GDAL describes geolocation
/// arrays (its `GEOLOCATION` metadata domain, RFC 4): two rasters (`x_dataset`, `y_dataset`),
/// sampled every `pixel_step` columns from `pixel_offset` and every `line_step` rows from
/// `line_offset`.
#[derive(Debug, Clone, PartialEq)]
pub struct Geolocation {
    /// The CRS of the arrays' coordinates, as WKT (WGS 84 for swaths).
    pub srs: String,
    /// The dataset of the x coordinates (longitudes), as GDAL names it (`HDF5:"f.h5"://...`).
    pub x_dataset: String,
    /// Its band, from 1.
    pub x_band: usize,
    /// The dataset of the y coordinates (latitudes).
    pub y_dataset: String,
    /// Its band, from 1.
    pub y_band: usize,
    /// The column of the raster that the arrays' first column is for.
    pub pixel_offset: f64,
    /// The number of raster columns per column of the arrays.
    pub pixel_step: f64,
    /// The row of the raster that the arrays' first row is for.
    pub line_offset: f64,
    /// The number of raster rows per row of the arrays.
    pub line_step: f64,
    /// Whether the coordinates are those of pixel centers (`GEOREFERENCING_CONVENTION`
    /// `PIXEL_CENTER`) rather than of their top left corners.
    pub pixel_center: bool,
}

impl Geolocation {
    /// The metadata items GDAL lists in its `GEOLOCATION` domain, in `gdalinfo`'s order.
    pub fn items(&self) -> Vec<(String, String)> {
        let n = |v: f64| if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") };
        let mut items = vec![
            ("SRS".to_string(), self.srs.clone()),
            ("X_DATASET".to_string(), self.x_dataset.clone()),
            ("X_BAND".to_string(), self.x_band.to_string()),
            ("Y_DATASET".to_string(), self.y_dataset.clone()),
            ("Y_BAND".to_string(), self.y_band.to_string()),
            ("PIXEL_OFFSET".to_string(), n(self.pixel_offset)),
            ("PIXEL_STEP".to_string(), n(self.pixel_step)),
            ("LINE_OFFSET".to_string(), n(self.line_offset)),
            ("LINE_STEP".to_string(), n(self.line_step)),
        ];
        if self.pixel_center {
            items.push(("GEOREFERENCING_CONVENTION".to_string(), "PIXEL_CENTER".to_string()));
        }
        items
    }

    /// Geolocation from GDAL's `GEOLOCATION` metadata items; `None` without the datasets.
    pub fn from_items(items: &[(String, String)]) -> Option<Self> {
        let get = |k: &str| items.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim());
        let num = |k: &str, default: f64| get(k).and_then(|v| v.parse().ok()).unwrap_or(default);
        Some(Self {
            srs: get("SRS").unwrap_or_default().to_string(),
            x_dataset: get("X_DATASET")?.to_string(),
            x_band: num("X_BAND", 1.0) as usize,
            y_dataset: get("Y_DATASET")?.to_string(),
            y_band: num("Y_BAND", 1.0) as usize,
            pixel_offset: num("PIXEL_OFFSET", 0.0),
            pixel_step: num("PIXEL_STEP", 1.0),
            line_offset: num("LINE_OFFSET", 0.0),
            line_step: num("LINE_STEP", 1.0),
            pixel_center: get("GEOREFERENCING_CONVENTION").is_some_and(|v| v.eq_ignore_ascii_case("PIXEL_CENTER")),
        })
    }
}

impl RasterDataset {
    /// Wraps a raster created in memory.
    pub fn new(raster: Raster, has_nodata: bool) -> Self {
        Self::with_driver(raster, RasterDriver::GTiff, has_nodata)
    }

    /// The raster with this geotransform (see [`RasterDataset::set_geo_transform`]): its pixels.
    fn with_geo_transform(mut self, gt: [f64; 6]) -> Raster {
        self.set_geo_transform(gt);
        self.raster
    }

    /// Sets the geotransform: a north-up one goes to the raster's origin and cell sizes; a rotated
    /// or south-up one is kept in [`RasterDataset::affine`], the raster's origin and cell sizes
    /// then describing the bounding box of its footprint. Clears any inherited GCPs.
    pub fn set_geo_transform(&mut self, gt: [f64; 6]) {
        self.gcps.clear();
        let r = &mut self.raster;
        if is_north_up(&gt) {
            r.x_min = gt[0];
            r.cell_size_x = gt[1];
            r.cell_size_y = -gt[5];
            r.y_min = gt[3] + gt[5] * r.rows as f64;
            self.affine = None;
            return;
        }
        let (w, h) = (r.cols as f64, r.rows as f64);
        let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].map(|(c, l)| (gt[0] + c * gt[1] + l * gt[2], gt[3] + c * gt[4] + l * gt[5]));
        let min_x = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let max_x = corners.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = corners.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        r.x_min = min_x;
        r.y_min = min_y;
        r.cell_size_x = if r.cols > 0 { (max_x - min_x) / w } else { gt[1].hypot(gt[4]) };
        r.cell_size_y = if r.rows > 0 { (max_y - min_y) / h } else { gt[2].hypot(gt[5]) };
        self.affine = Some(gt);
    }

    /// Wraps a raster read by a driver: real-valued bands, without geolocation arrays.
    pub fn with_driver(raster: Raster, driver: RasterDriver, has_nodata: bool) -> Self {
        Self {
            raster,
            driver,
            has_nodata,
            band_nodata: Vec::new(),
            band_metadata: Vec::new(),
            gcps: Vec::new(),
            affine: None,
            complex: false,
            geolocation: None,
            metadata_domains: Vec::new(),
            color_table: Vec::new(),
        }
    }

    /// The number of bands as GDAL counts them: complex bands count once.
    pub fn band_count(&self) -> usize {
        if self.complex { self.raster.bands / 2 } else { self.raster.bands }
    }

    /// GDAL's name of the bands' data type: `Int16`, `Float32`... and `CInt16`, `CFloat32`...
    /// for complex bands.
    pub fn data_type_name(&self) -> String {
        let name = crate::info::gdal_type_name(self.raster.data_type);
        if self.complex { format!("C{name}") } else { name.to_string() }
    }

    /// The CRS.
    pub fn crs(&self) -> SpatialRef {
        SpatialRef::from_raster_crs(&self.raster.crs)
    }

    /// The GDAL-style geotransform: `[x_origin, pixel_width, row_rotation, y_origin,
    /// column_rotation, pixel_height]`, `[x_origin, pixel_width, 0, y_origin, 0, -pixel_height]` for
    /// north-up rasters.
    pub fn geo_transform(&self) -> [f64; 6] {
        self.affine.unwrap_or_else(|| geo_transform(&self.raster))
    }

    /// The nodata value of a band, numbered from one.
    pub fn band_nodata(&self, band: usize) -> Option<f64> {
        if band == 0 || band > self.raster.bands {
            return None;
        }
        self.band_nodata.get(band - 1).copied().unwrap_or_else(|| self.nodata())
    }

    /// The color interpretation of a one-based band, as GDAL names it.
    pub fn band_color_interp(&self, band: usize) -> &str {
        if band == 0 || band > self.band_count() {
            return "Undefined";
        }
        self.band_metadata.get(band - 1).and_then(|b| b.color_interp.as_deref()).unwrap_or({
            if self.raster.bands == 1 && matches!(self.driver, RasterDriver::GTiff | RasterDriver::Cog) { "Gray" } else { "Undefined" }
        })
    }

    /// The first band's nodata value. Explicit band_nodata takes precedence
    /// over has_nodata and raster.nodata.
    pub fn nodata(&self) -> Option<f64> {
        self.band_nodata.first().copied().unwrap_or_else(|| self.has_nodata.then_some(self.raster.nodata))
    }
}

/// Whether a geotransform is north-up: no rotation, a positive pixel width and a negative pixel
/// height.
pub fn is_north_up(gt: &[f64; 6]) -> bool {
    gt[2] == 0.0 && gt[4] == 0.0 && gt[1] > 0.0 && gt[5] < 0.0
}

/// The GDAL-style geotransform of a raster (north-up: see [`RasterDataset::geo_transform`] for
/// rotated ones).
pub fn geo_transform(r: &Raster) -> [f64; 6] {
    [r.x_min, r.cell_size_x, 0.0, r.y_max(), 0.0, -r.cell_size_y]
}

/// A vector dataset read into memory: one or more layers.
#[derive(Debug, Clone)]
pub struct VectorDataset {
    /// The layers.
    pub layers: Vec<Layer>,
    /// The driver that read it.
    pub driver: VectorDriver,
    /// The shared styles a KML or KMZ document defines, as GDAL's `GetStyleTable` gives them: each
    /// id with its OGR style string. Features using one have `@id` in their `OGR_STYLE` field.
    /// Empty for other formats. Hand it to [`VectorWriteOptions::style_table`] to write it back.
    pub style_table: Vec<(String, String)>,
}

impl VectorDataset {
    /// A dataset of layers, without a style table.
    pub fn new(layers: Vec<Layer>, driver: VectorDriver) -> Self {
        Self { layers, driver, style_table: Vec::new() }
    }

    /// Replaces each `@id` in the layers' `OGR_STYLE` fields with its definition in the style
    /// table, for output that has no style table of its own to refer to. References the table does
    /// not define stay as they are.
    pub fn resolve_styles(&mut self) {
        crate::drivers::vector::kml_style::resolve_references(&mut self.layers, &self.style_table);
    }

    /// The layers with their style references resolved: see [`Self::resolve_styles`].
    pub fn into_resolved_layers(mut self) -> Vec<Layer> {
        self.resolve_styles();
        self.layers
    }
}

/// A dataset of either kind.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Dataset {
    /// A raster.
    Raster(RasterDataset),
    /// A vector dataset.
    Vector(VectorDataset),
}

/// Options for opening vector datasets.
#[derive(Debug, Clone, Default)]
pub struct VectorOpenOptions {
    /// Only these layers (by name). Empty means all. A single layer also answers to the
    /// source file's stem; this alias does not rename it. Every requested name must match.
    pub layers: Vec<String>,
    /// How to read CSV files.
    pub csv: crate::drivers::vector::csv::CsvOptions,
    /// How to read shapefiles: how the rings of a polygon record make polygons.
    pub shapefile: crate::drivers::vector::shapefile::ShapefileOptions,
    /// A rectangle `(min_x, min_y, max_x, max_y)` in the layer's CRS: drivers that index their
    /// features (GeoParquet's bounding-box covering column and row group statistics, GeoPackage's
    /// R-tree) skip those whose bounding box is outside it. Callers still filter the features
    /// they get by their geometries.
    pub spatial_filter: Option<(f64, f64, f64, f64)>,
    /// How to read tile archives (PMTiles, MBTiles, MVT): the zoom level, clipping.
    pub tiles: crate::tiles::TileOpenOptions,
    /// Features to skip at the start of each layer, in file order (pyogrio's `skip_features`,
    /// GDAL's `SetNextByIndex`). The Shapefile driver reads past them without decoding them;
    /// the other local drivers read every feature and keep the range.
    pub skip_features: usize,
    /// At most this many features per layer after the skipped ones (pyogrio's
    /// `max_features`); `None` reads to the end.
    pub max_features: Option<usize>,
    /// Whether to read a feature-service query's following pages (`FEATURE_SERVER_PAGING`, as in
    /// GDAL's ESRIJSON driver). By default, unless its URL gives a `resultOffset`.
    pub feature_server_paging: Option<bool>,
    /// Native WFS paging and retained-data budgets.
    pub wfs: crate::drivers::vector::wfs::WfsOpenOptions,
    /// Native and portable OGC API Features options.
    pub oapif: crate::drivers::vector::oapif::OapifOpenOptions,
    /// How to read HEC-RAS geometry and plan HDF files: meshes and the stored values to join.
    pub hecras: crate::drivers::vector::hecras_options::HecRasOpenOptions,
    /// How to read XLSX workbooks: header rows, field types and geometry columns.
    pub xlsx: crate::drivers::vector::xlsx_options::XlsxOptions,
}

impl VectorOpenOptions {
    /// Sets a driver open option by its GDAL name (`KEY`, `VALUE`): the CSV options (see
    /// [`CsvOptions::set`](crate::CsvOptions::set)), the shapefile options
    /// ([`ShapefileOptions::set`](crate::ShapefileOptions::set)), the tile archive options
    /// ([`TileOpenOptions::set`](crate::tiles::TileOpenOptions::set)) and the HECRAS options
    /// ([`HecRasOpenOptions::set`](crate::HecRasOpenOptions::set)) and the XLSX options
    /// ([`XlsxOptions::set`](crate::XlsxOptions::set)). Returns `false` for a name none of them knows.
    ///
    /// ```
    /// let mut options = terrio::VectorOpenOptions::default();
    /// assert!(options.set("CELL_SUMMARIES", "Maximum Water Surface")?);
    /// assert!(!options.set("NO_SUCH_OPTION", "1")?);
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        if key.eq_ignore_ascii_case("OAPIF_MAX_FEATURES") || key.eq_ignore_ascii_case("OAPIF_SKIP_FEATURES") {
            let number = value.parse::<usize>().map_err(|_| Error::InvalidArgument(format!("{key} must be a nonnegative integer")))?;
            self.oapif.configured = true;
            if key.eq_ignore_ascii_case("OAPIF_MAX_FEATURES") {
                self.max_features = Some(number);
            } else {
                self.skip_features = number;
            }
            return Ok(true);
        }
        if key.eq_ignore_ascii_case("FEATURE_SERVER_PAGING") {
            self.feature_server_paging = Some(crate::tiles::parse_bool(value)?);
            return Ok(true);
        }
        // The XLSX options share the names of CSV's geometry options, and each format keeps its own.
        let known = self.csv.set(key, value)?
            || self.shapefile.set(key, value)?
            || self.tiles.set(key, value)?
            || self.hecras.set(key, value)?
            || self.wfs.set(key, value)?
            || self.oapif.set(key, value)?;
        Ok(self.xlsx.set(key, value)? || known)
    }

    /// Whether a feature range is asked for.
    pub(crate) fn has_range(&self) -> bool {
        self.skip_features > 0 || self.max_features.is_some()
    }

    /// Keeps the requested range of a layer's features.
    pub(crate) fn keep_range(&self, layer: &mut Layer) {
        if !self.has_range() {
            return;
        }
        let start = self.skip_features.min(layer.features.len());
        let end = self.max_features.map_or(layer.features.len(), |m| start.saturating_add(m).min(layer.features.len()));
        layer.features.truncate(end);
        layer.features.drain(..start);
        layer.extent = None;
    }
}

/// Opens a dataset, raster or vector, detecting its format.
///
/// With the `gdal` feature, paths starting with `GDAL:` are opened by GDAL, and so are those
/// terrio's own drivers cannot open (see `gdal_backend`).
///
/// ```no_run
/// match terrio::open("countries.gpkg")? {
///     terrio::Dataset::Raster(r) => println!("{} x {} pixels", r.raster.cols, r.raster.rows),
///     terrio::Dataset::Vector(v) => println!("{} layers", v.layers.len()),
/// }
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn open(path: &str) -> Result<Dataset> {
    crate::formats::check_allowed(path, None)?;
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::open(rest);
    }
    if let Some(result) = open_remote_vector(path, &VectorOpenOptions::default()) {
        return result.map(Dataset::Vector);
    }
    let result = open_native(path);
    #[cfg(feature = "gdal")]
    let result = gdal_fallback(path, result, crate::gdal_backend::open);
    result
}

pub(crate) fn open_native(path: &str) -> Result<Dataset> {
    if let Some(result) = crate::catalog::open_lazy(path) {
        return result?.read().map(Dataset::Raster);
    }
    if let Some(result) = crate::stac::open(path) {
        return result;
    }
    if let Some(result) = crate::drivers::open_subdataset(path) {
        return result.map(Dataset::Raster);
    }
    if let Some(result) = crate::drivers::open_in_place(path) {
        return result.map(Dataset::Raster);
    }
    // Tile archives, local or remote, are read in place.
    if is_tile_archive(path) {
        return open_tile_archive(path, &VectorOpenOptions::default());
    }
    #[cfg(remote)]
    check_remote_copy(path, true)?;
    let local = vfs::resolve(path)?;
    open_local(local.as_str(), &VectorOpenOptions::default()).map_err(|e| naming_dataset(e, path, local.as_str()))
}

/// Opens a raster dataset, reading it into memory.
///
/// With the `gdal` feature, paths starting with `GDAL:` are opened by GDAL, and so are those
/// terrio's own drivers cannot open.
///
/// ```no_run
/// let dem = terrio::open_raster("dem.tif")?;
/// println!("{:?} nodata {:?}", dem.geo_transform(), dem.nodata());
/// let elevation = dem.raster.band_view(0); // band-major pixels, as f64
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn open_raster(path: &str) -> Result<RasterDataset> {
    crate::formats::check_allowed(path, Some(crate::formats::Kind::Raster))?;
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::open_raster(rest);
    }
    // A missing file, or a directory, is named in the error.
    let result = open_raster_native(path).map_err(|e| e.naming_local_file(path));
    #[cfg(feature = "gdal")]
    let result = gdal_fallback(path, result, crate::gdal_backend::open_raster);
    result
}

/// Reads a native TIFF dataset into memory with an explicit decoder limit.
///
/// Use this for TIFF tiles or strips larger than the default 256 MiB. Local paths,
/// URLs and archive members are resolved as for [`open_raster`]. Both the initial
/// decoder and parallel chunk decoders use the selected limit. This TIFF-specific
/// entry point does not fall back to GDAL or dispatch to other raster formats.
pub fn open_raster_with_tiff_limits(path: &str, limits: crate::TiffReadLimits) -> Result<RasterDataset> {
    let local = vfs::resolve(path)?;
    crate::geotiff::check_tiff_allowed(local.path(), path)?;
    crate::geotiff::read_geotiff_with_limits(Path::new(local.as_str()), limits)
}

fn open_raster_native(path: &str) -> Result<RasterDataset> {
    if let Some(result) = crate::catalog::open_lazy(path) {
        return result?.read();
    }
    if let Some(result) = crate::stac::open_raster(path) {
        return match result? {
            Dataset::Raster(r) => Ok(r),
            Dataset::Vector(_) => Err(Error::Unsupported(format!("'{path}' is a vector, not a raster dataset"))),
        };
    }
    if let Some(result) = crate::drivers::open_subdataset(path) {
        return result;
    }
    if let Some(result) = crate::drivers::open_in_place(path) {
        return result;
    }
    if is_tile_archive(path) {
        return match open_tile_archive(path, &VectorOpenOptions::default())? {
            Dataset::Raster(r) => Ok(r),
            Dataset::Vector(_) => Err(Error::Unsupported(format!("'{path}' holds vector tiles, not a raster"))),
        };
    }
    #[cfg(remote)]
    check_remote_copy(path, false)?;
    let local = vfs::resolve(path)?;
    read_raster_local(local.as_str()).map_err(|e| naming_dataset(e, path, local.as_str()))
}

/// Refuses a remote file that [`vfs::resolve`] would download whole for the whole read of a raster
/// ([`read_raster_local`]) or, `generic`, of a dataset of either kind ([`open_local`]), when that
/// read finds the format by content alone ([`judged_by_content`]) and the file's first bytes show
/// none it takes under the file's name ([`crate::drivers::takes_lone_file`]). The bytes judged are
/// those the open reads anyway ([`crate::http::FIRST_READ`], from the process-wide cache); a file
/// they show to be taken is downloaded whole, as before. GDAL refuses such files after identifying
/// them from their first bytes (49,152 of them), where terrio downloaded them whole to refuse them
/// after: 49,999,930 bytes of a 50,000,000-byte GeoJSON document named `x`, `x.dat` or
/// `dir.json/x`, and the whole of a PNG, a JPEG or a STAC item so named, or of a TIFF for the whole
/// read of a raster (a lazy open reads a TIFF by range whatever its name). A file they show to be
/// taken by a driver the process-wide allow-list leaves out ([`crate::drivers::lone_file_driver`])
/// is refused too, as the read would refuse it once downloaded.
#[cfg(remote)]
fn check_remote_copy(path: &str, generic: bool) -> Result<()> {
    let loc = vfs::Location::parse(path);
    let Some(remote) = loc.remote() else { return Ok(()) };
    let name = loc.file_name();
    if !judged_by_content(&name, generic) {
        return Ok(());
    }
    let (head, len) = crate::http::head_and_len(&remote, crate::http::FIRST_READ)?;
    // Taken by its content (or, for an SRTM tile, its name and size): refused here, before the rest
    // is downloaded, when the process-wide allow-list leaves out the driver the read takes it with.
    if let Some(driver) = crate::drivers::lone_file_driver(&name, &head, len) {
        return crate::formats::check_driver_allowed(driver.name(), path.trim(), crate::formats::Kind::Raster);
    }
    let shown = match crate::formats::sniff(&head) {
        Some(format) if crate::formats::read_under_own_names(format, generic) => {
            format!("hold {format} data, which terrio does not read whole under this name")
        }
        Some(format) => format!("hold {format} data"),
        None => "show no known format".to_string(),
    };
    Err(Error::Unsupported(format!(
        "'{}' not recognized as being in a supported {} format: its first {} bytes {shown}",
        path.trim().split(['?', '#']).next().unwrap_or(path),
        if generic { "file" } else { "raster" },
        head.len()
    )))
}

/// Whether the whole read of a remote file named `name` ([`read_raster_local`], or, `generic`,
/// [`open_local`]) finds the file's format by content alone, so that its first bytes tell whether
/// the read takes it: under a name of no driver, and under those of the labelled raw rasters
/// (ENVI's `.dat`, EHdr's `.bil`), which terrio reads with their `.hdr`, never fetched for a remote
/// file. Not under a name of no driver with `TERRIO_WBRASTER_FALLBACK=YES` or, `generic`,
/// `TERRIO_WBVECTOR_FALLBACK=YES`: their detection reads the whole file. Names the generic read
/// takes by name (vector drivers', vector tiles', tile archives') are not judged.
#[cfg(remote)]
fn judged_by_content(name: &str, generic: bool) -> bool {
    let vector = generic
        && (VectorDriver::for_extension(name).is_some()
            || crate::tiles::archive_kind(name).is_some()
            || name.to_ascii_lowercase().ends_with(".mvt.gz"));
    match RasterDriver::for_extension(name) {
        Some(RasterDriver::ENVI | RasterDriver::EHdr) => !vector,
        Some(_) => false,
        None => !(vector || wbraster_fallback_allowed() || (generic && wbvector_fallback_allowed())),
    }
}

/// An error reading `local`, the copy of `path` that [`vfs::resolve`] made (a remote file
/// downloaded, an archive member extracted, to a temporary directory), naming the dataset as the
/// caller gave it rather than the copy, whose path means nothing to the caller and would show a
/// server's directories to the clients of a service that passes the message on. The copy's path
/// is replaced with the dataset's name (without its query, which may hold a signature); a message
/// that names neither starts with it. The variant is kept, but for the format libraries' errors,
/// which become [`Error::InvalidData`] (the Python class they already map to).
fn naming_dataset(e: Error, path: &str, local: &str) -> Error {
    if vfs::Location::parse(path).is_local() {
        return e;
    }
    let shown = path.trim().split(['?', '#']).next().unwrap_or(path);
    let named = |m: String| if m.contains(local) { m.replace(local, shown) } else { format!("'{shown}': {m}") };
    match e {
        Error::Unsupported(m) => Error::Unsupported(named(m)),
        Error::ResourceLimit(m) => Error::ResourceLimit(named(m)),
        Error::InvalidData(m) => Error::InvalidData(named(m)),
        Error::InvalidArgument(m) => Error::InvalidArgument(named(m)),
        Error::Raster(e) => Error::InvalidData(named(e.to_string())),
        Error::Vector(e) => Error::InvalidData(named(e.to_string())),
        Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), named(e.to_string()))),
        e => e,
    }
}

/// Opens a vector dataset, reading its layers into memory.
///
/// With the `gdal` feature, paths starting with `GDAL:` are opened by GDAL, and so are those
/// terrio's own drivers cannot open.
///
/// ```no_run
/// let options = terrio::VectorOpenOptions { layers: vec!["countries".into()], ..Default::default() };
/// let ds = terrio::open_vector("countries.gpkg", &options)?;
/// for feature in &ds.layers[0].features {
///     println!("{:?}", feature.attributes);
/// }
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn open_vector(path: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    options.oapif.validate_source(path)?;
    crate::formats::check_allowed(path, Some(crate::formats::Kind::Vector))?;
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::open_vector(rest, options);
    }
    if let Some(result) = open_remote_vector(path, options) {
        return result;
    }
    let result = open_vector_native(path, options);
    #[cfg(feature = "gdal")]
    let result = gdal_fallback(path, result, |p| crate::gdal_backend::open_vector(p, options));
    result
}

/// A vector response or feature-service query: opaque remote GeoJSON/GML names, and URLs with
/// the signs of a service that answers ESRI JSON or its protocol
/// buffer, or one named by the `ESRIJSON:` or `ESRIPBF:` prefix, which also names a local file. Read
/// before the other readers and before GDAL: a query's answer is made per request, and many ArcGIS
/// servers ignore ranges; and an answer recognized here and found wrong (a service's error, a page
/// with other fields, a service that ignores the offset) is an error, not something for GDAL to read
/// what it can of without a word. Other paths (`None`) go on as usual, as do URLs whose request
/// fails before their answer is recognized, which report their own errors.
pub(crate) fn open_remote_vector(path: &str, options: &VectorOpenOptions) -> Option<Result<VectorDataset>> {
    if let Some(target) = crate::drivers::vector::oapif::service_target(path) {
        #[cfg(remote)]
        return Some(crate::drivers::vector::oapif::open(target, options));
        #[cfg(not(remote))]
        {
            let _ = target;
            return Some(Err(crate::drivers::vector::oapif::transport_required()));
        }
    }
    if let Some(target) = crate::drivers::vector::wfs::service_target(path) {
        #[cfg(remote)]
        return Some(crate::drivers::vector::wfs::open(target, options));
        #[cfg(not(remote))]
        {
            let _ = target;
            return Some(Err(Error::disabled("remote")));
        }
    }
    let prefixed = [("ESRIJSON:", VectorDriver::ESRIJSON), ("ESRIPBF:", VectorDriver::ESRIPBF)]
        .into_iter()
        .find(|(prefix, _)| path.get(..prefix.len()).is_some_and(|p| p.eq_ignore_ascii_case(prefix)));
    let target = match prefixed {
        Some((prefix, _)) => path.get(prefix.len()..)?,
        None => path,
    };
    let named = prefixed.map(|(_, driver)| driver);
    if let Some(driver) = named
        && !(target.starts_with("http://") || target.starts_with("https://"))
    {
        return Some(vfs::resolve(target).and_then(|local| read_vector_local(local.as_str(), driver, options)));
    }
    if let Some(result) = open_query(target, named, options) {
        return Some(result);
    }
    #[cfg(remote)]
    if named.is_none() {
        return open_remote_vector_response(target, options);
    }
    None
}

/// A single remote GeoJSON/GML document under an opaque name. Reuse the normal bounded range
/// probe and cache so raster and unknown-format reads retain their existing I/O behavior.
#[cfg(remote)]
fn open_remote_vector_response(url: &str, options: &VectorOpenOptions) -> Option<Result<VectorDataset>> {
    let loc = vfs::Location::parse(url);
    let remote = loc.remote()?;
    if crate::stac::may_be_stac(url)
        || crate::drivers::vector::wfs::is_feature_request(url)
        || crate::drivers::vector::featureservice::is_query_url(url)
        || !judged_by_content(&loc.file_name(), true)
    {
        return None;
    }
    let (head, _) = match crate::http::head_and_len(&remote, crate::http::FIRST_READ) {
        Ok(header) => header,
        Err(error) if error.is_resource_refusal() => return Some(Err(error)),
        Err(_) => return None,
    };
    let driver = remote_vector_response_driver(&head)?;
    let result = crate::formats::check_driver_allowed(driver.name(), url, crate::formats::Kind::Vector)
        .and_then(|()| crate::http::get_file(&remote, crate::http::Redirects::Any))
        .and_then(|bytes| read_vector_answer(url, bytes, driver, options));
    #[cfg(feature = "gdal")]
    let result = gdal_fallback(url, result, |path| crate::gdal_backend::open_vector(path, options));
    Some(result)
}

#[cfg(remote)]
fn remote_vector_response_driver(head: &[u8]) -> Option<VectorDriver> {
    match crate::inspection::identify_bytes_unjudged(head, "response")?.driver.as_str() {
        "GeoJSON" => Some(VectorDriver::GeoJSON),
        "GML" => Some(VectorDriver::GML),
        _ => None,
    }
}

/// [`open_remote_vector`] for a URL, in a build that has no network: one named for a driver says
/// so.
#[cfg(not(remote))]
fn open_query(_url: &str, named: Option<VectorDriver>, _options: &VectorOpenOptions) -> Option<Result<VectorDataset>> {
    named.map(|_| Err(Error::disabled("remote")))
}

/// [`open_remote_vector`] for a URL.
#[cfg(remote)]
fn open_query(url: &str, named: Option<VectorDriver>, options: &VectorOpenOptions) -> Option<Result<VectorDataset>> {
    use crate::drivers::vector::{esrijson, esripbf, featureservice};
    use std::io::Read;
    let wfs = crate::drivers::vector::wfs::is_feature_request(url);
    if named.is_none() && !featureservice::is_query_url(url) && !wfs {
        return None;
    }
    let mut recognized = named;
    let answer = crate::http::with_reader_status(&crate::http::Remote::Url(url.to_string()), wfs, |status, reader| {
        let cap = crate::http::json_max_size();
        let mut bytes = Vec::new();
        let head_bytes = if wfs { crate::http::FIRST_READ } else { esrijson::HEAD_BYTES as u64 };
        (&mut *reader).take(head_bytes).read_to_end(&mut bytes)?;
        if wfs && !(200..300).contains(&status) {
            recognized = Some(VectorDriver::WFS);
            (&mut *reader).take(cap.saturating_sub(bytes.len() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > cap {
                return Err(crate::http::over_cap(url, cap));
            }
            return Err(if crate::drivers::vector::wfs::is_exception(&bytes) {
                crate::drivers::vector::wfs::feature_exception(&bytes)
            } else {
                Error::Http(format!("{}: status {status}", url.split(['?', '#']).next().unwrap_or(url)))
            });
        }
        // Asked for the protocol buffer, a service answers a query it refuses with nothing, or with
        // its error in JSON, and one for a count, ids or an extent with that answer, which the reader
        // refuses by name: all of them this driver's to report.
        let pbf = featureservice::asks_for_pbf(url);
        let refused = bytes.is_empty() && featureservice::is_query_path(url);
        recognized = recognized.or_else(|| {
            if wfs && crate::drivers::vector::wfs::is_exception(&bytes) {
                Some(VectorDriver::WFS)
            } else if pbf && (refused || esrijson::is_esrijson(&bytes) || esripbf::is_query_answer(&bytes)) {
                Some(VectorDriver::ESRIPBF)
            } else if esrijson::is_esrijson(&bytes) {
                Some(VectorDriver::ESRIJSON)
            } else if esripbf::is_esripbf(&bytes) {
                Some(VectorDriver::ESRIPBF)
            } else {
                remote_vector_response_driver(&bytes)
            }
        });
        if let Some(driver @ (VectorDriver::GeoJSON | VectorDriver::GML)) = recognized {
            crate::formats::check_driver_allowed(driver.name(), url, crate::formats::Kind::Vector)?;
        }
        if recognized.is_none() {
            return Ok(None);
        }
        reader.take(cap.saturating_sub(bytes.len() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > cap {
            return Err(crate::http::over_cap(url, cap));
        }
        Ok(Some(bytes))
    });
    match (answer, recognized) {
        // Its first bytes showed ESRI JSON's signs, but a root `type` further on can say GeoJSON:
        // read the same bytes as that, without asking again.
        (Ok(Some(bytes)), Some(VectorDriver::ESRIJSON | VectorDriver::ESRIPBF))
            if named.is_none()
                && esripbf::is_json(&bytes)
                && std::str::from_utf8(&bytes).is_ok_and(|text| !esrijson::is_esrijson_document(text)) =>
        {
            Some(read_vector_answer(url, bytes, VectorDriver::GeoJSON, options))
        }
        (Ok(Some(bytes)), Some(VectorDriver::ESRIPBF))
            if bytes.is_empty() && featureservice::is_query_path(url) && featureservice::asks_for_pbf(url) =>
        {
            Some(Err(Error::InvalidData("ESRIPBF: the answer is empty: a service refuses a query this way, and f=json shows why".into())))
        }
        (Ok(Some(bytes)), Some(driver @ (VectorDriver::GeoJSON | VectorDriver::GML))) => {
            Some(read_vector_answer(url, bytes, driver, options))
        }
        (Ok(Some(bytes)), Some(VectorDriver::WFS)) => Some(Err(crate::drivers::vector::wfs::feature_exception(&bytes))),
        (Ok(Some(bytes)), Some(driver)) => Some(read_feature_service(url, &bytes, driver, options)),
        (Err(e), _) if e.is_resource_refusal() => Some(Err(e)),
        (Err(e), Some(_)) => Some(Err(e)),
        _ => None,
    }
}

/// A URL's vector answer, from its bytes; keep signed queries out of layer names and diagnostics.
#[cfg(remote)]
fn read_vector_answer(url: &str, bytes: Vec<u8>, driver: VectorDriver, options: &VectorOpenOptions) -> Result<VectorDataset> {
    let shown = url.trim().split(['?', '#']).next().unwrap_or(url);
    let layers = crate::drivers::vector::read_bytes(bytes, shown, driver, options)?
        .ok_or_else(|| Error::Unsupported(format!("the {} driver cannot read from memory", driver.name())))?;
    let mut layers = select_layers(layers, shown, options)?;
    layers.iter_mut().for_each(|l| options.keep_range(l));
    Ok(finish_vector(layers, shown, driver))
}

/// A feature-service query from its first page's bytes, with the pages after it.
#[cfg(remote)]
fn read_feature_service(url: &str, first: &[u8], driver: VectorDriver, options: &VectorOpenOptions) -> Result<VectorDataset> {
    use crate::drivers::vector::{esrijson, esripbf, featureservice};
    let allowed = || crate::formats::check_driver_allowed(driver.name(), url, crate::formats::Kind::Vector);
    if driver != VectorDriver::ESRIPBF {
        allowed()?;
    }
    let decode = |page: &[u8]| {
        if driver != VectorDriver::ESRIPBF {
            return esrijson::page(page, driver.name());
        }
        // Each page by what it holds: `page` reports a service's JSON error under any list, and
        // checks JSON features as ESRI JSON.
        if !esripbf::is_json(page) {
            allowed()?;
        }
        esripbf::page(page, driver.name(), url)
    };
    let stop_at = options.max_features.map(|max| options.skip_features.saturating_add(max));
    let layer = featureservice::read_pages(url, first, options.feature_server_paging, stop_at, &decode)?;
    let mut layers = select_layers(vec![layer], url, options)?;
    layers.iter_mut().for_each(|l| options.keep_range(l));
    Ok(finish_vector(layers, url, driver))
}

pub(crate) fn open_vector_native(path: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    options.oapif.validate_source(path)?;
    if let Some(result) = crate::stac::open(path) {
        return match result? {
            Dataset::Vector(v) => Ok(v),
            Dataset::Raster(_) => Err(Error::Unsupported(format!("'{path}' is a raster, not a vector dataset"))),
        };
    }
    if is_tile_archive(path) {
        return match open_tile_archive(path, options)? {
            Dataset::Vector(v) => Ok(v),
            Dataset::Raster(_) => Err(Error::Unsupported(format!("'{path}' holds raster tiles, not vector tiles"))),
        };
    }
    let local = vfs::resolve(path)?;
    match open_local(local.as_str(), options)? {
        Dataset::Vector(v) => Ok(v),
        Dataset::Raster(_) => Err(Error::Unsupported(format!("'{path}' is a raster, not a vector dataset"))),
    }
}

/// The layer names of a vector dataset, without reading the features when the format allows it.
pub fn list_layers(path: &str) -> Result<Vec<String>> {
    crate::formats::check_allowed(path, Some(crate::formats::Kind::Vector))?;
    if let Some(target) = crate::drivers::vector::oapif::service_target(path) {
        #[cfg(remote)]
        return crate::drivers::vector::oapif::list_layers(target);
        #[cfg(not(remote))]
        {
            let _ = target;
            return Err(Error::disabled("remote"));
        }
    }
    if let Some(target) = crate::drivers::vector::wfs::service_target(path) {
        #[cfg(remote)]
        return crate::drivers::vector::wfs::list_layers(target);
        #[cfg(not(remote))]
        {
            let _ = target;
            return Err(Error::disabled("remote"));
        }
    }
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::list_layers(rest);
    }
    if let Some(result) = open_remote_vector(path, &VectorOpenOptions { feature_server_paging: Some(false), ..Default::default() }) {
        return result.map(|v| v.layers.into_iter().map(|l| l.name).collect());
    }
    let result = list_layers_native(path);
    #[cfg(feature = "gdal")]
    let result = gdal_fallback(path, result, crate::gdal_backend::list_layers);
    result
}

fn list_layers_native(path: &str) -> Result<Vec<String>> {
    if is_tile_archive(path) {
        #[cfg(feature = "tiles")]
        return crate::tiles::layer_names(path);
        #[cfg(not(feature = "tiles"))]
        return Err(Error::disabled("tiles"));
    }
    let local = vfs::resolve(path)?;
    if VectorDriver::for_extension(local.as_str()) == Some(VectorDriver::GPKG) {
        #[cfg(feature = "sqlite")]
        return crate::drivers::vector::gpkg_read::list_layers(local.as_str());
        #[cfg(not(feature = "sqlite"))]
        return Err(Error::disabled("sqlite"));
    }
    Ok(open_vector_native(local.as_str(), &VectorOpenOptions::default())?.layers.into_iter().map(|l| l.name).collect())
}

/// When terrio's own drivers could not open `path`, tries GDAL; if GDAL cannot either, terrio's
/// error is kept, except for GDAL's resource refusals. Missing local files and invalid arguments
/// are not retried, nor are network errors or resource-limit refusals.
#[cfg(feature = "gdal")]
pub(crate) fn gdal_fallback<T>(path: &str, result: Result<T>, gdal: impl FnOnce(&str) -> Result<T>) -> Result<T> {
    let Err(e) = result else { return result };
    let retry = match &e {
        Error::InvalidArgument(_) | Error::Http(_) => false,
        e if e.is_resource_refusal() => false,
        // A missing file, unless the path is one of GDAL's dataset names (`HDF5:"f.h5"://x`).
        Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => path.contains(':') || path.starts_with("/vsi"),
        _ => true,
    };
    if !retry {
        return Err(e);
    }
    gdal(path).map_err(|gdal_error| if gdal_error.is_resource_refusal() { gdal_error } else { e })
}

#[cfg(all(test, feature = "gdal"))]
mod fallback_limits {
    use super::*;

    #[test]
    fn resource_refusals_are_not_retried() {
        for error in
            [Error::DecodedLimitExceeded { resource: "VSI file", limit: 8 }, Error::ResourceLimit("document cells exceed budget".into())]
        {
            let result: Result<()> = gdal_fallback("input.tif", Err(error), |_| panic!("limit refusal retried through GDAL"));
            assert!(matches!(result, Err(Error::DecodedLimitExceeded { .. } | Error::ResourceLimit(_))));
        }
    }
    #[test]
    fn allocation_refusals_are_not_retried() {
        let allocation = fallible::filled(usize::MAX, 0u8, "a raster buffer").unwrap_err();
        assert_eq!(allocation.kind(), std::io::ErrorKind::OutOfMemory);
        let result: Result<()> = gdal_fallback("input.tif", Err(allocation.into()), |_| panic!("allocation refusal retried through GDAL"));
        assert!(matches!(result, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory));
    }

    #[test]
    fn gdal_resource_refusal_is_not_hidden_by_the_native_error() {
        let native = || Error::Unsupported("no native driver".into());
        let result: Result<()> = gdal_fallback("input", Err(native()), |_| Err(Error::ResourceLimit("GDAL budget".into())));
        assert!(matches!(result, Err(Error::ResourceLimit(_))));
        let result: Result<()> = gdal_fallback("input", Err(native()), |_| Ok(()));
        assert!(result.is_ok());
        let result: Result<()> = gdal_fallback("input", Err(native()), |_| Err(Error::InvalidData("GDAL parse error".into())));
        assert!(matches!(result, Err(Error::Unsupported(_))));
        let result: Result<()> = gdal_fallback("input", Err(native()), |_| Err(Error::InvalidArgument("no layer named missing".into())));
        assert!(matches!(result, Err(Error::Unsupported(_))));
    }
}

/// Whether a path names a PMTiles or MBTiles archive (not inside a zip archive).
fn is_tile_archive(path: &str) -> bool {
    let loc = vfs::Location::parse(path);
    !matches!(loc, vfs::Location::Zip { .. }) && crate::tiles::archive_kind(&loc.file_name()).is_some()
}

/// Opens a PMTiles or MBTiles archive, local or remote, in place.
#[cfg_attr(not(feature = "tiles"), allow(unused_variables))]
fn open_tile_archive(path: &str, options: &VectorOpenOptions) -> Result<Dataset> {
    #[cfg(feature = "tiles")]
    return crate::tiles::open_named_dataset(path, options);
    #[cfg(not(feature = "tiles"))]
    Err(Error::disabled("tiles"))
}

fn open_local(path: &str, options: &VectorOpenOptions) -> Result<Dataset> {
    if crate::tiles::archive_kind(path).is_some() {
        return open_tile_archive(path, options);
    }
    #[cfg(feature = "tiles")]
    if let Some(layers) = crate::tiles::open_mvt_local(path, options) {
        return layers.map(|l| Dataset::Vector(finish_vector(l, path, VectorDriver::MVT)));
    }
    if !Path::new(path).exists() {
        return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("'{path}' does not exist"))));
    }
    let ext = extension(path).unwrap_or_default();
    // GeoPackages hold vector layers, raster tiles or both: prefer the vector layers.
    if ext == "gpkg" {
        #[cfg(feature = "sqlite")]
        return crate::drivers::gpkg::open(path, options);
        #[cfg(not(feature = "sqlite"))]
        return Err(Error::disabled("sqlite"));
    }
    // City-model documents, ESRI JSON and protocol buffers with no recognized extension (a downloaded
    // query result) use a bounded root probe, as does `.pbf`, which a query's protocol buffer shares
    // with OpenStreetMap.
    #[cfg(not(target_family = "wasm"))]
    let pbf =
        VectorDriver::for_extension(path) == Some(VectorDriver::OSM) && ext == "pbf" && !path.to_ascii_lowercase().ends_with(".osm.pbf");
    #[cfg(not(target_family = "wasm"))]
    if (VectorDriver::for_extension(path).is_none() || pbf) && RasterDriver::for_extension(path).is_none() && !Path::new(path).is_dir() {
        let driver = crate::inspection::identify(path, false).ok().flatten().and_then(|identified| match identified.driver.as_str() {
            "ESRIPBF" => Some(VectorDriver::ESRIPBF),
            _ if pbf => None,
            "CityGML" => Some(VectorDriver::CityGML),
            "CityJSON" => Some(VectorDriver::CityJSON),
            // Its first bytes show ESRI JSON, but a root `type` further on can say GeoJSON: the JSON
            // reader decides on the whole document.
            "ESRIJSON" => Some(VectorDriver::GeoJSON),
            _ => None,
        });
        if let Some(driver) = driver {
            return read_vector_local(path, driver, options).map(Dataset::Vector);
        }
        // Its name also names this driver, which an allow-list may have let it through for: the
        // driver that reads it is the one to judge.
        if pbf {
            crate::formats::check_driver_allowed(VectorDriver::OSM.name(), path, crate::formats::Kind::Vector)?;
        }
    }
    if let Some(driver) = VectorDriver::for_extension(path) {
        return read_vector_local(path, driver, options).map(Dataset::Vector);
    }
    // HEC-RAS geometry and plan files are HDF5 files that hold vector layers: recognized by
    // content before the raster drivers claim them (`open_raster` still reads them as HDF5).
    #[cfg(feature = "hecras")]
    if crate::drivers::hecras::driver::is_hecras(path) {
        return read_vector_local(path, VectorDriver::HECRAS, options).map(Dataset::Vector);
    }
    if RasterDriver::for_extension(path).is_some() || Path::new(path).is_dir() {
        return read_raster_local(path).map(Dataset::Raster);
    }
    // Unknown extension: let the drivers sniff the content.
    let raster = read_raster_local(path);
    if raster.as_ref().is_err_and(Error::is_resource_refusal) {
        return raster.map(Dataset::Raster);
    }
    // GDAL recognizes formats more reliably than the vector library's guess (which reads any
    // SQLite database as a GeoPackage).
    #[cfg(feature = "gdal")]
    if raster.is_err()
        && let Ok(ds) = crate::gdal_backend::open_with(path, options)
    {
        return Ok(ds);
    }
    match raster {
        Ok(r) => Ok(Dataset::Raster(r)),
        Err(raster_err) if !wbvector_fallback_allowed() => Err(Error::Unsupported(format!(
            "'{path}': format not recognized ({raster_err}; TERRIO_WBVECTOR_FALLBACK=YES lets wbvector detect it)"
        ))),
        // GeoParquet by its magic number, with terrio's own driver: wbvector is built without it.
        #[cfg(feature = "parquet")]
        Err(_) if has_parquet_magic(path) => {
            crate::formats::check_driver_allowed(VectorDriver::Parquet.name(), path, crate::formats::Kind::Vector)?;
            read_vector_local(path, VectorDriver::Parquet, options).map(Dataset::Vector)
        }
        // A zip archive, found as KMZ: read only if the KMZ driver is allowed.
        Err(_) if is_wbvector_kmz(path) => {
            crate::formats::check_driver_allowed(VectorDriver::KMZ.name(), path, crate::formats::Kind::Vector)?;
            wbvector_dataset(read_wbvector_kmz(path)?, path, options)
        }
        Err(raster_err) => match wbvector::VectorFormat::detect(path) {
            Ok(format) => {
                // Found by its content (an XML element anywhere in its first 4 KiB, a SQLite
                // database as a GeoPackage...): read only if the driver of that format is allowed.
                let driver = VectorDriver::ALL.iter().find(|d| d.to_wb().as_ref() == Some(&format)).map_or("unknown", |d| d.name());
                crate::formats::check_driver_allowed(driver, path, crate::formats::Kind::Vector)?;
                wbvector_dataset(wbvector::read(path)?, path, options)
            }
            Err(_) => Err(Error::Unsupported(format!("'{path}': format not recognized ({raster_err})"))),
        },
    }
}

/// The dataset of a layer read by the `wbvector` fallback.
fn wbvector_dataset(layer: Layer, path: &str, options: &VectorOpenOptions) -> Result<Dataset> {
    let layers = select_layers(vec![layer], path, options)?;
    Ok(Dataset::Vector(VectorDataset::new(
        layers.into_iter().map(|l| fix_layer(l, path, VectorDriver::GeoJSON)).collect(),
        VectorDriver::GeoJSON,
    )))
}

/// Whether the `wbvector` fallback reads a file as KMZ: a zip archive, whatever its name (a zip
/// archive is never XML, even named `.xml`), unless it is a path without an extension beside a
/// `.shp` file, which `wbvector` reads as that shapefile.
fn is_wbvector_kmz(path: &str) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let zip = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"PK\x03\x04";
    let path = Path::new(path);
    zip && !(path.extension().is_none() && path.with_extension("shp").exists())
}

/// The layer of a KMZ archive the `wbvector` fallback found, from the member the KMZ driver reads:
/// its first member named `doc.kml` (in any case) at the archive's root, else its first `.kml`
/// member, decoded as the KMZ driver decodes it (UTF-8, else Latin-1) and read with `wbvector`'s
/// KML reader. terrio opens the archive: `wbvector` is built without its own KMZ reader, whose
/// `zip` 0.6 has known advisories.
fn read_wbvector_kmz(path: &str) -> Result<Layer> {
    use std::io::Read;
    let kmz = |message: String| Error::from(wbvector::GeoError::Kmz(message));
    let bytes = std::fs::read(path).map_err(wbvector::GeoError::Io)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| kmz(format!("invalid zip archive: {e}")))?;
    // zip keeps one entry per name, the last: of two members named `doc.kml`, the last is read.
    // Only names are compared, so no member is opened but the one read.
    let names: Vec<Vec<u8>> = archive.file_names_raw().map(|name| name.to_ascii_lowercase()).collect();
    let index = (names.iter().position(|name| name == b"doc.kml"))
        .or_else(|| names.iter().position(|name| name.ends_with(b".kml")))
        .ok_or_else(|| kmz("no .kml entry found in KMZ".into()))?;
    let mut document = Vec::new();
    archive
        .by_index(index)
        .map_err(|e| kmz(format!("failed opening KML entry {index}: {e}")))?
        .read_to_end(&mut document)
        .map_err(|e| kmz(format!("failed reading KML entry {index}: {e}")))?;
    Ok(wbvector::kml::parse_str(&crate::drivers::vector::xml::decode(&document))?)
}

/// Whether a file starts with Parquet's magic number.
#[cfg(feature = "parquet")]
fn has_parquet_magic(path: &str) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"PAR1"
}

/// Whether a raster that neither its extension nor terrio's own drivers recognise may be handed
/// to `wbraster`'s format detection: only with the option `TERRIO_WBRASTER_FALLBACK=YES` (see
/// [`crate::config`]). Unknown files fail closed otherwise, as with a driver allow-list: the
/// detection reaches formats no allow-list names (CSF `.map` rasters by content, Esri binary
/// grids, Zarr stores terrio's driver declines) through decoders with known advisories (`ruzstd`
/// 0.8, RUSTSEC-2026-0344).
fn wbraster_fallback_allowed() -> bool {
    option_enabled("TERRIO_WBRASTER_FALLBACK")
}

/// Whether a file that neither its extension nor terrio's own drivers recognise, raster or
/// vector, may be handed to `wbvector`'s format detection: only with the option
/// `TERRIO_WBVECTOR_FALLBACK=YES`, as for `wbraster`'s ([`wbraster_fallback_allowed`]). The
/// detection reads content no allow-list names: zip archives as KMZ (their KML document, with
/// `wbvector`'s reader), SQLite databases as GeoPackages, FlatGeobuf, GeoParquet, GML, GPX, KML.
fn wbvector_fallback_allowed() -> bool {
    option_enabled("TERRIO_WBVECTOR_FALLBACK")
}

/// Whether the option `key` is `YES` (or `TRUE`, `ON`, `1`), see [`crate::config`].
fn option_enabled(key: &str) -> bool {
    crate::config::get(key).is_some_and(|v| matches!(v.trim().to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1"))
}

fn read_raster_local(path: &str) -> Result<RasterDataset> {
    if RasterDriver::for_extension(path) == Some(RasterDriver::VRT) {
        return crate::ops::vrt::read_vrt(path);
    }
    if let Some(result) = crate::drivers::read(path) {
        return result;
    }
    if RasterDriver::for_extension(path) == Some(RasterDriver::GTiff) {
        return crate::geotiff::read_geotiff(Path::new(path));
    }
    let (raster, driver) = match RasterDriver::for_extension(path) {
        // No `wbraster` format: GRIB in a build with `gdal` and without `grib`, read by the GDAL backend.
        Some(driver) => match driver.to_wb() {
            Some(format) => (Raster::read_with_format(path, format)?, driver),
            None => return Err(Error::Unsupported(format!("'{path}': no {} reader in this build", driver.name()))),
        },
        None if wbraster_fallback_allowed() => {
            let fmt = wbraster::RasterFormat::detect(path)?;
            // Found by its content or the files beside it: read only if its driver is allowed.
            crate::drivers::check_recognised(RasterDriver::from_wb(fmt), path)?;
            (fmt.read(path)?, RasterDriver::from_wb(fmt))
        }
        None => {
            return Err(Error::Unsupported(format!(
                "'{path}': format not recognized by terrio's drivers (TERRIO_WBRASTER_FALLBACK=YES lets wbraster detect it)"
            )));
        }
    };
    let mut ds = crate::drivers::nodata::finish(path, driver, raster);
    crate::drivers::nodata::apply_pam(path, &mut ds);
    Ok(ds)
}

fn read_vector_local(path: &str, driver: VectorDriver, options: &VectorOpenOptions) -> Result<VectorDataset> {
    // `.json` is shared with GeoJSON. The root type may follow megabytes of vertices or
    // CityObjects, beyond the bounded identifier. These formats already require a whole
    // document read; inspect those same bytes rather than opening the file twice.
    let (driver, preloaded) = if matches!(driver, VectorDriver::GeoJSON | VectorDriver::GML) {
        let bytes =
            if driver == VectorDriver::GML { crate::drivers::vector::citygml::read_path_bytes(path, true)? } else { std::fs::read(path)? };
        let text = if driver == VectorDriver::GeoJSON {
            Some(std::str::from_utf8(&bytes).map_err(|e| Error::InvalidData(format!("JSON input is not UTF-8: {e}")))?)
        } else {
            None
        };
        let city = text.is_some_and(|text| {
            crate::inspection::root_json_type(text.trim_start_matches('\u{feff}').trim_start()).as_deref() == Some("CityJSON")
        });
        let driver = if city {
            VectorDriver::CityJSON
        } else if driver == VectorDriver::GML && crate::drivers::vector::citygml::looks_like(&bytes) {
            VectorDriver::CityGML
        } else if matches!(crate::formats::extension(path).as_deref(), Some("json") | None)
            && text.is_some_and(crate::drivers::vector::esrijson::is_esrijson_document)
        {
            VectorDriver::ESRIJSON
        } else {
            driver
        };
        crate::formats::check_driver_allowed(driver.name(), path, crate::formats::Kind::Vector)?;
        (driver, Some(bytes))
    } else {
        (driver, None)
    };
    driver.ensure_built()?;
    #[cfg(feature = "sqlite")]
    if driver == VectorDriver::GPKG {
        let mut layers = crate::drivers::vector::gpkg_read::read(path, &options.layers, options.spatial_filter)?;
        layers.iter_mut().for_each(|l| options.keep_range(l));
        return Ok(finish_vector(layers, path, driver));
    }
    if matches!(driver, VectorDriver::KML | VectorDriver::KMZ) {
        let (layers, style_table) = match preloaded {
            Some(bytes) => crate::drivers::vector::read_kml_bytes(bytes, path, driver)?,
            None if driver == VectorDriver::KML => crate::drivers::vector::kml::read_styled(path)?,
            None => crate::drivers::vector::kml::read_kmz_styled(path)?,
        };
        let mut layers = select_layers(layers, path, options)?;
        layers.iter_mut().for_each(|l| options.keep_range(l));
        return Ok(VectorDataset { style_table, ..finish_vector(layers, path, driver) });
    }
    let native = match preloaded {
        Some(bytes) if driver == VectorDriver::GML => Some(crate::drivers::vector::gml::parse_path_bytes(&bytes, path)?),
        Some(bytes) => crate::drivers::vector::read_bytes(bytes, path, driver, options)?,
        None => crate::drivers::vector::read(path, driver, options)?,
    };
    let mut layers = if let Some(layers) = native {
        select_layers(layers, path, options)?
    } else {
        let format = driver.to_wb().ok_or_else(|| Error::Unsupported(format!("the {} driver cannot read", driver.name())))?;
        let layer = format.read(path)?;
        select_layers(vec![layer], path, options)?
    };
    // The Shapefile driver reads only the range; the others read every feature.
    if driver != VectorDriver::Shapefile {
        layers.iter_mut().for_each(|l| options.keep_range(l));
    }
    Ok(finish_vector(layers, path, driver))
}

/// A vector dataset of layers a driver read, normalized.
pub(crate) fn finish_vector(layers: Vec<Layer>, path: &str, driver: VectorDriver) -> VectorDataset {
    let layers = layers.into_iter().map(|l| fix_layer(l, path, driver)).collect();
    VectorDataset::new(layers, driver)
}

/// Keeps the layers `options` asks for (all when it names none).
pub(crate) fn select_layers(layers: Vec<Layer>, path: &str, options: &VectorOpenOptions) -> Result<Vec<Layer>> {
    let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
    let selected = layer_selection(&options.layers, &names, Some(path), false)?;
    Ok(layers.into_iter().zip(selected).filter_map(|(layer, keep)| keep.then_some(layer)).collect())
}

/// Selects from the complete source inventory before features are read. Empty requests keep
/// all layers; a single layer also answers to the source file's stem, without being renamed.
/// Missing requests use one diagnostic across readers and operations. Unlike GDAL 3.13.3,
/// Terrio also accepts this file-stem alias. SQLite table lookup remains case-insensitive.
pub(crate) fn layer_selection(requested: &[String], available: &[&str], path: Option<&str>, ignore_case: bool) -> Result<Vec<bool>> {
    let stem = path.and_then(|p| Path::new(p).file_stem()).and_then(|s| s.to_str()).filter(|s| !s.is_empty());
    let matches = |layer: &str, name: &str| {
        layer == name || (ignore_case && layer.eq_ignore_ascii_case(name)) || (available.len() == 1 && stem == Some(name))
    };
    if let Some(missing) = requested.iter().find(|n| !available.iter().any(|l| matches(l, n))) {
        return Err(Error::InvalidArgument(format!("no layer named {missing} (layers: {})", available.join(", "))));
    }
    Ok(available.iter().map(|l| requested.is_empty() || requested.iter().any(|n| matches(l, n))).collect())
}

/// Normalizes what drivers leave inconsistent: the layer name and the CRS of formats that are
/// WGS 84 by definition.
pub(crate) fn fix_layer(mut layer: Layer, path: &str, driver: VectorDriver) -> Layer {
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    // A GeoPackage table or a worksheet may be named "layer"; other drivers use it as a placeholder.
    if layer.name.is_empty() || (layer.name == "layer" && !matches!(driver, VectorDriver::GPKG | VectorDriver::XLSX)) {
        layer.name = stem.to_string();
    }
    // Re-identify the EPSG code of stored WKT (such as a shapefile's ESRI .prj) the way GDAL does,
    // unless the WKT is the registry's own definition of the code the layer has.
    let registry = layer.crs_epsg().and_then(|c| wbprojection::to_ogc_wkt(c).ok());
    // HEC-RAS layers keep the stored projection text without an EPSG code: the engine never
    // asserts one, and the CRS of a hydraulic model is not re-identified.
    if driver != VectorDriver::HECRAS
        && let Some(wkt) = layer.crs_wkt().map(str::to_string)
        && registry.as_deref() != Some(wkt.as_str())
        && let Some(code) = crate::crs::identify_epsg(&wkt)
    {
        layer.set_crs_epsg(Some(code));
    }
    let wgs84_by_definition =
        matches!(driver, VectorDriver::GeoJSON | VectorDriver::TopoJSON | VectorDriver::KML | VectorDriver::KMZ | VectorDriver::GPX);
    if wgs84_by_definition && layer.crs_epsg().is_none() && layer.crs_wkt().is_none() {
        layer.set_crs_epsg(Some(4326));
    }
    layer
}

/// Options for writing rasters.
#[derive(Debug, Clone, Default)]
pub struct RasterWriteOptions {
    /// The output format. By default, chosen from the output path's extension.
    pub driver: Option<RasterDriver>,
    /// GeoTIFF/COG compression: `NONE`, `LZW`, `DEFLATE`, `ZSTD`, `PACKBITS`, `JPEG`, `WEBP`. GeoTIFFs
    /// default to `NONE`, COGs to `LZW`, as in GDAL. Zarr: `NONE`, `ZLIB` (v2), `GZIP`, `ZSTD`.
    /// GeoPackage tiles of Byte rasters: `PNG` (the default) or `JPEG`.
    pub compression: Option<String>,
    /// Write GeoTIFF tiles of this size (COG tiles default to 512).
    pub tile_size: Option<u32>,
    /// Force BigTIFF.
    pub bigtiff: bool,
    /// Format-specific creation options, as GDAL names them (`KEY`, `VALUE`): Zarr
    /// `FORMAT=ZARR_V2|ZARR_V3`; netCDF `FORMAT=NC|NC2|NC5`; GeoTIFF `TILED=YES`,
    /// `BLOCKXSIZE`, `BLOCKYSIZE`, `INTERLEAVE=PIXEL|BAND`, `PHOTOMETRIC=MINISBLACK|RGB|PALETTE`,
    /// `ALPHA=YES`, `BIGTIFF=YES`, `COMPRESS`, and terrio's `GEOREFERENCED=NO` (no geotransform).
    pub creation_options: Vec<(String, String)>,
    /// Descriptive metadata of each band (GeoTIFF: written to the `GDAL_METADATA` tag).
    pub band_metadata: Vec<BandMetadata>,
    /// A color table for a single-band Byte or UInt16 GeoTIFF: `(red, green, blue, alpha)`
    /// entries from index 0. Empty for none.
    pub color_table: Vec<[u8; 4]>,
    /// GeoTIFF overviews to build, as decimation factors such as `[2, 4, 8]`.
    pub overviews: Vec<usize>,
    /// The overview resampling, as GDAL names it: `nearest`, `average`, `rms`, `bilinear`,
    /// `cubic`, `cubicspline`, `lanczos` or `mode` (and terrio's `min` and `max` for GeoTIFF
    /// overviews). GeoTIFF overviews default to `nearest`; COGs to GDAL's COG driver's default,
    /// `cubic` (`nearest` for paletted rasters), and also take the `OVERVIEW_RESAMPLING` and
    /// `RESAMPLING` creation options, which come first.
    pub overview_resampling: Option<String>,
    /// The geotransform to write in place of the raster's own: a rotated or south-up one, which
    /// GeoTIFFs store in a `ModelTransformationTag`, as GDAL writes them.
    /// [`write_raster_dataset`] sets it from [`RasterDataset::affine`].
    pub geo_transform: Option<[f64; 6]>,
    /// Ground control points to georeference the raster with instead of a geotransform (GeoTIFF:
    /// tie points), in the raster's CRS.
    pub gcps: Vec<Gcp>,
    /// A validity mask of the whole raster, `cols * rows` values row by row, 0 for invalid
    /// pixels: GeoTIFFs store it as GDAL's internal mask (a 1-bit transparency mask image).
    pub mask: Option<Vec<u8>>,
    /// Rational polynomial coefficients, the 92 values of the GeoTIFF `RPCCoefficientTag` in its
    /// order (error bias and random error, offsets, scales, then the line numerator and
    /// denominator, sample numerator and denominator coefficients). Empty for none.
    pub rpc: Vec<f64>,
    /// The block size `(width, height)` of the raster written, when it is read by blocks: GDAL's
    /// COG driver computes overviews in blocks of that size when it is square (a power of two
    /// from 64 to 4096), of 128 otherwise, and the blocks set how its kernels round. Set by
    /// [`crate::LazyRaster::write`].
    pub source_block_size: Option<(usize, usize)>,
}

/// A ground control point: a pixel position and its georeferenced coordinates, as GDAL's
/// `GDAL_GCP`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gcp {
    /// An identifier.
    pub id: String,
    /// A description.
    pub info: String,
    /// The column, from the image's left edge (0.5 is the first pixel's centre).
    pub pixel: f64,
    /// The row, from the image's top edge.
    pub line: f64,
    /// The georeferenced X (easting or longitude).
    pub x: f64,
    /// The georeferenced Y (northing or latitude).
    pub y: f64,
    /// The elevation.
    pub z: f64,
}

/// The descriptive metadata of a band, as GDAL keeps it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BandMetadata {
    /// The band's description.
    pub description: Option<String>,
    /// The scale applied to pixel values to get physical values.
    pub scale: Option<f64>,
    /// The offset added to scaled pixel values.
    pub offset: Option<f64>,
    /// The unit of the physical values, such as `m`.
    pub unit: Option<String>,
    /// The color interpretation, as GDAL names it: `Gray`, `Red`, `Alpha`, `Palette`...
    pub color_interp: Option<String>,
    /// Metadata items (GDAL's band metadata, default domain).
    pub items: Vec<(String, String)>,
}

/// The ground control points of a GeoTIFF (local, remote or in an archive), as GDAL reads them:
/// tie points without a pixel scale or transformation matrix, in the file's CRS. Empty for
/// rasters georeferenced by a geotransform, and for other formats.
pub fn read_gcps(path: &str) -> Result<Vec<Gcp>> {
    let local = vfs::resolve(path)?;
    crate::geotiff::check_tiff_allowed(local.path(), path)?;
    Ok(crate::geotiff::read_geo_tags(local.path()).map(|t| t.gcps).unwrap_or_default())
}

/// Writes a raster, declaring its nodata value.
pub fn write_raster(raster: &Raster, path: &str, options: &RasterWriteOptions) -> Result<()> {
    write_raster_with_nodata(raster, Some(raster.nodata), false, path, options)
}

/// Writes a raster dataset, declaring a nodata value only when the dataset has one.
///
/// ```no_run
/// use terrio::formats::RasterDriver;
///
/// let dem = terrio::open_raster("dem.tif")?;
/// // The format from the extension, or given explicitly with creation options.
/// terrio::write_raster_dataset(&dem, "dem.asc", &Default::default())?;
/// let options = terrio::RasterWriteOptions {
///     driver: Some(RasterDriver::Zarr),
///     compression: Some("ZSTD".into()),
///     creation_options: vec![("FORMAT".into(), "ZARR_V3".into())],
///     ..Default::default()
/// };
/// terrio::write_raster_dataset(&dem, "dem.zarr", &options)?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn write_raster_dataset(ds: &RasterDataset, path: &str, options: &RasterWriteOptions) -> Result<()> {
    // PHOTOMETRIC=RGB and ALPHA=YES, as GDAL's CreateCopy takes them from the source bands.
    let derived = color_options(ds, path, options);
    let options = derived.as_ref().unwrap_or(options);
    let driver = options.driver.or_else(|| RasterDriver::for_extension(path));
    let options = options.inherited_from(ds, driver);
    let pam = needs_nodata_pam(ds) && matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog));
    let tag = if matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog)) { tiff_nodata(ds) } else { ds.nodata() };
    write_raster_with_nodata(&ds.raster, tag, ds.complex, path, &options)?;
    if matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog)) {
        let mut aux = std::ffi::OsString::from(path);
        aux.push(".aux.xml");
        if pam || crate::memfs::exists(Path::new(&aux)) {
            let values = if ds.band_nodata.is_empty() { vec![ds.nodata(); ds.raster.bands] } else { ds.band_nodata.clone() };
            crate::drivers::common::write_band_nodata_pam(Path::new(path), &values)?;
        }
    }
    Ok(())
}

/// TIFF's shared nodata tag cannot be overridden to "no nodata" by PAM.
pub(crate) fn tiff_nodata(ds: &RasterDataset) -> Option<f64> {
    if (1..=ds.raster.bands).any(|band| ds.band_nodata(band).is_none()) { None } else { ds.nodata() }
}

/// TIFF nodata that needs a PAM override to retain its per-band or raw Float32 value.
pub(crate) fn needs_nodata_pam(ds: &RasterDataset) -> bool {
    let distinct = !ds.band_nodata.is_empty()
        && ds.band_nodata.iter().any(|v| match (*v, ds.nodata()) {
            (Some(a), Some(b)) => a != b && !(a.is_nan() && b.is_nan()),
            (None, None) => false,
            _ => true,
        });
    // GDAL's TIFF reader rounds its Float32 nodata tag to that type. As GDAL
    // Translate does, keep a raw double that cannot be represented there in PAM.
    let raw_float32_nodata = ds.raster.data_type == wbraster::DataType::F32
        && (1..=ds.raster.bands).any(|band| ds.band_nodata(band).is_some_and(|value| value.is_finite() && value as f32 as f64 != value));
    distinct || raw_float32_nodata
}

/// The options a GeoTIFF or COG is written with from its source dataset, as GDAL's `CreateCopy`
/// takes them from the source bands: `PHOTOMETRIC=RGB` for red, green and blue first bands,
/// `ALPHA=YES` for an alpha last band besides the colour ones (it then weights the overviews);
/// each unless the caller gave it, and only when the caller gave no band colour interpretations.
/// The color table is inherited as `RasterWriteOptions::inherited_from` decides. `None` when
/// nothing is added, or for other formats.
pub(crate) fn color_options(ds: &RasterDataset, path: &str, options: &RasterWriteOptions) -> Option<RasterWriteOptions> {
    let driver = options.driver.or_else(|| RasterDriver::for_extension(path));
    if !matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog)) {
        return None;
    }
    let interps_given = options.band_metadata.iter().any(|b| b.color_interp.is_some());
    let given = |k: &str| options.creation_options.iter().find(|(o, _)| o.eq_ignore_ascii_case(k)).map(|(_, v)| v.to_ascii_uppercase());
    let ci = |b: usize| ds.band_metadata.get(b).and_then(|m| m.color_interp.as_deref()).map(str::to_ascii_lowercase);
    let bands = ds.raster.bands;
    let photometric = given("PHOTOMETRIC");
    let rgb = photometric.is_none() && bands >= 3 && (0..3).map(ci).eq(["red", "green", "blue"].map(|c| Some(c.to_string())));
    // The colour samples, as the writer resolves the photometric (3 Byte bands are RGB).
    let color_samples = match photometric.as_deref() {
        Some("RGB" | "YCBCR") => 3,
        Some(_) => 1,
        None if rgb || (bands == 3 && ds.raster.data_type == wbraster::DataType::U8) => 3,
        None => 1,
    };
    let mut extra = Vec::new();
    if rgb && !interps_given {
        extra.push(("PHOTOMETRIC".to_string(), "RGB".to_string()));
    }
    if !interps_given && given("ALPHA").is_none() && bands > color_samples && ci(bands - 1).as_deref() == Some("alpha") {
        extra.push(("ALPHA".to_string(), "YES".to_string()));
    }
    (!extra.is_empty()).then(|| {
        let mut o = options.clone();
        o.creation_options.extend(extra);
        o
    })
}

fn write_raster_with_nodata(raster: &Raster, nodata: Option<f64>, complex: bool, path: &str, options: &RasterWriteOptions) -> Result<()> {
    ensure_writable_location(path)?;
    let driver = match options.driver.or_else(|| RasterDriver::for_extension(path)) {
        Some(d) => d,
        None => {
            // With the GDAL backend, the GDAL driver of the extension (`.ntf`: NITF).
            #[cfg(feature = "gdal")]
            if let Some(d) = crate::gdal_backend::driver_for_path(path, false) {
                if !options.gcps.is_empty() || options.mask.is_some() || !options.rpc.is_empty() {
                    return Err(Error::Unsupported(format!("GCPs, masks and RPCs are not written through GDAL's {d} driver")));
                }
                let mut co = options.creation_options.clone();
                if let Some(c) = &options.compression
                    && !co.iter().any(|(k, _)| k.eq_ignore_ascii_case("COMPRESS"))
                {
                    co.push(("COMPRESS".into(), c.clone()));
                }
                return crate::gdal_backend::write_raster_gt(raster, nodata, complex, options.geo_transform, path, &d, &co);
            }
            return Err(Error::InvalidArgument(format!("cannot tell the output format of '{path}': give it explicitly")));
        }
    };
    driver.ensure_built()?;
    if !driver.info().write {
        return Err(Error::Unsupported(format!("the {} driver cannot write", driver.name())));
    }
    let tiff = matches!(driver, RasterDriver::GTiff | RasterDriver::Cog);
    if tiff {
        // terrio's writer handles the compressions it encodes; wbraster the others.
        if let Some(mut opts) = crate::drivers::gtiff_write::TiffOptions::from_write_options(options, driver == RasterDriver::Cog, nodata)?
        {
            // Complex bands as complex samples, as GDAL writes CInt16... CFloat64 GeoTIFFs.
            opts.complex = complex;
            return crate::drivers::gtiff_write::write(raster, path, &opts);
        }
    }
    // Complex samples: GeoTIFFs written above, and GDAL's HFA driver below.
    if complex && !(cfg!(feature = "gdal") && driver == RasterDriver::HFA) {
        return Err(Error::Unsupported(format!(
            "the {} writer cannot store complex bands (GeoTIFF can, with a compression terrio's writer encodes)",
            driver.name()
        )));
    }
    if options.geo_transform.is_some_and(|gt| !is_north_up(&gt))
        || !options.gcps.is_empty()
        || options.mask.is_some()
        || !options.rpc.is_empty()
    {
        return Err(Error::Unsupported(format!(
            "the {} writer cannot store rotated or south-up geotransforms, GCPs, masks or RPCs (GeoTIFF can, with a lossless compression)",
            driver.name()
        )));
    }
    let north_up;
    let raster = match options.geo_transform {
        Some(gt) if gt != geo_transform(raster) => {
            north_up = RasterDataset::with_driver(raster.clone(), driver, false).with_geo_transform(gt);
            &north_up
        }
        _ => raster,
    };
    if let Some(result) = crate::drivers::write(raster, nodata, path, driver, options) {
        return result;
    }
    // Formats terrio reads but cannot write itself.
    #[cfg(feature = "gdal")]
    if driver == RasterDriver::HFA {
        let mut co = options.creation_options.clone();
        if let Some(c) = &options.compression {
            co.push(("COMPRESSED".into(), if c.eq_ignore_ascii_case("NONE") { "NO" } else { "YES" }.into()));
        }
        return crate::gdal_backend::write_raster_with(raster, nodata, complex, path, "HFA", &co);
    }
    #[cfg(feature = "gdal")]
    if driver == RasterDriver::GRIB {
        return crate::gdal_backend::write_raster_with(raster, nodata, complex, path, "GRIB", &options.creation_options);
    }
    let compression = options.compression.as_deref().map(parse_compression).transpose()?;
    match driver {
        RasterDriver::Cog => {
            let opts = wbraster::CogWriteOptions { compression, bigtiff: Some(options.bigtiff), tile_size: options.tile_size };
            raster.write_cog_with_options(path, &opts)?;
        }
        RasterDriver::GTiff => {
            let layout = options.tile_size.map(|t| wbraster::GeoTiffLayout::Tiled { tile_width: t, tile_height: t });
            let opts = wbraster::GeoTiffWriteOptions { compression, bigtiff: Some(options.bigtiff), layout };
            raster.write_geotiff_with_options(path, &opts)?;
        }
        RasterDriver::VRT => {
            return Err(Error::Unsupported("a VRT references other files: build one with `terrio mosaic` (ops::mosaic::build_vrt)".into()));
        }
        d => raster.write(path, d.to_wb().ok_or_else(|| Error::Unsupported(format!("the {} driver cannot write", d.name())))?)?,
    }
    Ok(())
}

/// Writes rasters into one netCDF-4 file, each in its group: `/science/grid` (groups created as
/// needed) or `/` for the root, with its own dimensions, coordinate variables and grid mapping,
/// as the netCDF library lays out groups. A single raster goes in a group with the `GROUP`
/// creation option of [`write_raster_dataset`]. The creation options are the netCDF driver's
/// (`FORMAT` defaults to `NC4`, which groups need; `COMPRESS=DEFLATE`, `ZLEVEL`, `CHUNKING`).
///
/// ```no_run
/// let dem = terrio::open_raster("dem.tif")?;
/// let slope = terrio::open_raster("slope.tif")?;
/// terrio::write_netcdf_groups(&[("/terrain/dem", &dem), ("/terrain/slope", &slope)], "terrain.nc", &Default::default())?;
/// // Read back by GDAL's names: NETCDF:"terrain.nc":/terrain/slope/Band1
/// # Ok::<(), terrio::Error>(())
/// ```
#[cfg_attr(not(feature = "netcdf-hdf5"), allow(unused_variables))]
pub fn write_netcdf_groups(rasters: &[(&str, &RasterDataset)], path: &str, options: &RasterWriteOptions) -> Result<()> {
    ensure_writable_location(path)?;
    if rasters.is_empty() {
        return Err(Error::InvalidArgument("no rasters to write".into()));
    }
    #[cfg(feature = "netcdf-hdf5")]
    return crate::drivers::write_netcdf_groups(rasters, path, options);
    #[cfg(not(feature = "netcdf-hdf5"))]
    Err(Error::disabled("netcdf-hdf5"))
}

fn parse_compression(s: &str) -> Result<wbraster::GeoTiffCompression> {
    use wbraster::GeoTiffCompression as C;
    Ok(match s.to_ascii_uppercase().as_str() {
        "JPEG" => C::Jpeg,
        "WEBP" => C::WebP,
        other => {
            return Err(Error::InvalidArgument(format!(
                "unknown compression '{other}' (NONE, LZW, DEFLATE, ZSTD, PACKBITS, LERC, LERC_DEFLATE, LERC_ZSTD, JXL, JPEG, WEBP)"
            )));
        }
    })
}

/// Options for writing vector datasets.
#[derive(Debug, Clone, Default)]
pub struct VectorWriteOptions {
    /// The output format. By default, chosen from the output path's extension.
    pub driver: Option<VectorDriver>,
    /// CityJSON output version (1.1 or 2.0). The default is 2.0.
    pub cityjson_version: Option<crate::CityJsonVersion>,
    /// CityGML XML version (2.0 or 3.0). The default is 3.0.
    pub citygml_version: Option<crate::CityGmlVersion>,
    /// CityGML 2.0 building LoD 0 property when the flat input has no source hint.
    pub citygml_lod0_surface: Option<crate::CityGmlLod0Surface>,
    /// GeoPackage: retain each layer's declared base geometry type, including Unknown,
    /// when creating or replacing layers. By default the feature types are inferred.
    /// Unknown declarations allow optional Z. Required/optional source dimensional
    /// hints are not represented by Layer and cannot be reproduced by this option.
    pub preserve_declared_geometry_type: bool,
    /// How to write CSV files.
    pub csv: crate::drivers::vector::csv::CsvOptions,
    /// How to write GeoJSON files.
    pub geojson: crate::drivers::vector::geojson::GeoJsonOptions,
    /// How to write vector tiles (PMTiles, MBTiles, MVT): zoom levels, buffer, simplification.
    pub tiles: crate::tiles::TileWriteOptions,
    /// How to write KMZ archives: the files, such as icon images, packed with the documents.
    pub kmz: crate::drivers::vector::kml::KmzOptions,
    /// How to write XLSX workbooks: the geometry columns, a macro-enabled workbook.
    pub xlsx: crate::drivers::vector::xlsx_options::XlsxOptions,
    /// KML and KMZ: shared styles to write as `<Style id>` elements, each id with its OGR style
    /// string, for the features whose `OGR_STYLE` is `@id`. See [`VectorDataset::style_table`].
    /// Layers do not hold the table: without it, `@id` is written as it is, as a `styleUrl` with no
    /// definition in KML. Elsewhere use [`VectorDataset::resolve_styles`] first.
    pub style_table: Vec<(String, String)>,
}

/// Writes vector layers. GeoPackage, GPX, KML, KMZ, TopoJSON and XLSX hold several layers in one file;
/// for the other formats, write one layer at a time.
///
/// With the `gdal` feature, an extension terrio has no driver for is written by the GDAL driver
/// that claims it (`.dxf`, `.gdb`; see `gdal_backend::write_vector`).
///
/// ```no_run
/// let ds = terrio::open_vector("countries.shp", &Default::default())?;
/// terrio::write_vector(&ds.layers, "countries.gpkg", &Default::default())?;
/// let mut options = terrio::VectorWriteOptions::default();
/// options.geojson.set("RFC7946", "YES")?;
/// terrio::write_vector(&ds.layers, "countries.geojson", &options)?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn write_vector(layers: &[Layer], path: &str, options: &VectorWriteOptions) -> Result<()> {
    ensure_writable_location(path)?;
    let driver = match options.driver.or_else(|| VectorDriver::for_extension(path)) {
        Some(d) => d,
        None => {
            // With the GDAL backend, the GDAL driver of the extension (`.gdb`...).
            #[cfg(feature = "gdal")]
            if let Some(d) = crate::gdal_backend::driver_for_path(path, true) {
                ensure_vector_options_apply(options, None)?;
                return crate::gdal_backend::write_vector(layers, path, &d, &[], &[]);
            }
            return Err(Error::InvalidArgument(format!("cannot tell the output format of '{path}': give it explicitly")));
        }
    };
    ensure_vector_options_apply(options, Some(driver))?;
    driver.ensure_built()?;
    if !driver.info().write {
        return Err(Error::Unsupported(format!("the {} driver cannot write", driver.name())));
    }
    match (driver, layers) {
        (_, []) => Err(Error::InvalidArgument("no layers to write".into())),
        #[cfg(feature = "sqlite")]
        (VectorDriver::GPKG, _) => {
            if options.preserve_declared_geometry_type {
                std::fs::write(path, crate::drivers::vector::geopackage::to_bytes_with_geometry_types(layers, true)?)?;
                Ok(())
            } else {
                crate::drivers::vector::geopackage::write(layers, path)
            }
        }
        #[cfg(feature = "cad")]
        (d @ (VectorDriver::DXF | VectorDriver::DWG), _) => {
            std::fs::write(path, crate::drivers::vector::cad::to_bytes(layers, d)?)?;
            Ok(())
        }
        #[cfg(feature = "xlsx")]
        (VectorDriver::XLSX, _) => {
            let bytes = crate::drivers::vector::xlsx::to_bytes(layers, &options.xlsx.for_name(path))?;
            crate::vsi::write_local_atomic(std::path::Path::new(path), |out| {
                use std::io::Write;
                out.write_all(&bytes)?;
                Ok(())
            })
        }
        (VectorDriver::GPX, _) => crate::drivers::vector::gpx::write_layers(layers, path),
        (VectorDriver::KML, _) => crate::drivers::vector::kml::write_styled(layers, &options.style_table, path),
        (VectorDriver::KMZ, _) => crate::drivers::vector::kml::write_kmz_styled(layers, &options.style_table, path, &options.kmz),
        (VectorDriver::TopoJSON, _) => crate::drivers::vector::topojson::write_layers(layers, path),
        (VectorDriver::CityGML, _) => {
            let bytes = crate::drivers::vector::citygml_write::to_bytes(
                layers,
                options.citygml_version.unwrap_or_default(),
                options.citygml_lod0_surface,
            )?;
            crate::vsi::write_local_atomic(std::path::Path::new(path), |out| {
                use std::io::Write;
                out.write_all(&bytes)?;
                Ok(())
            })
        }
        (VectorDriver::CityJSON, _) => {
            let bytes = crate::drivers::vector::cityjson_write::to_bytes(layers, options.cityjson_version.unwrap_or_default())?;
            crate::vsi::write_local_atomic(std::path::Path::new(path), |out| {
                use std::io::Write;
                out.write_all(&bytes)?;
                Ok(())
            })
        }
        #[cfg(feature = "tiles")]
        (VectorDriver::MBTiles | VectorDriver::PMTiles | VectorDriver::MVT, _) => {
            crate::tiles::write_vector(layers, path, driver, &options.tiles)
        }
        (d, [layer]) => {
            if !crate::drivers::vector::write(layer, path, d, options)? {
                let format = d.to_wb().ok_or_else(|| Error::Unsupported(format!("the {} driver cannot write", d.name())))?;
                wbvector::write(layer, path, format)?;
            }
            Ok(())
        }
        (d, _) => Err(Error::InvalidArgument(format!("the {} format holds one layer: choose a layer, or write a GeoPackage", d.name()))),
    }
}

/// Refuses format-specific vector options on other output formats rather than dropping them. `driver` is `None` for a format only GDAL writes.
pub(crate) fn ensure_vector_options_apply(options: &VectorWriteOptions, driver: Option<VectorDriver>) -> Result<()> {
    if (options.citygml_version.is_some() || options.citygml_lod0_surface.is_some()) && driver != Some(VectorDriver::CityGML) {
        return Err(Error::InvalidArgument("citygml_version/citygml_lod0_surface applies to CityGML output".into()));
    }
    if options.cityjson_version.is_some() && driver != Some(VectorDriver::CityJSON) {
        return Err(Error::InvalidArgument("cityjson_version applies to CityJSON output".into()));
    }
    if options.preserve_declared_geometry_type && driver != Some(VectorDriver::GPKG) {
        return Err(Error::InvalidArgument("preserve_declared_geometry_type applies to GPKG output".into()));
    }
    let xlsx = &options.xlsx;
    if (xlsx.geometry != crate::CsvGeometry::None || xlsx.macro_enabled || xlsx.has_layout()) && driver != Some(VectorDriver::XLSX) {
        return Err(Error::InvalidArgument("the XLSX write options apply to XLSX output".into()));
    }
    if !options.style_table.is_empty() && !driver.is_some_and(|d| d.holds_style_table()) {
        return Err(Error::InvalidArgument("style_table applies to KML and KMZ output".into()));
    }
    if options.kmz.resources.is_empty() || driver == Some(VectorDriver::KMZ) {
        return Ok(());
    }
    Err(Error::InvalidArgument("kmz_resources applies to KMZ output".into()))
}

pub(crate) fn ensure_writable_location(path: &str) -> Result<()> {
    if !vfs::Location::parse(path).is_local() {
        return Err(Error::Unsupported(format!("cannot write to '{path}': only local outputs are supported")));
    }
    Ok(())
}

impl RasterWriteOptions {
    /// Metadata inheritance shared by whole-image and row-streamed TIFF writers.
    pub(crate) fn inherited_from(&self, ds: &RasterDataset, driver: Option<RasterDriver>) -> Self {
        let co = |key: &str| self.creation_options.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
        let compression = self.compression.as_deref().or_else(|| co("COMPRESS")).unwrap_or_default();
        let palette_allowed = !compression.eq_ignore_ascii_case("JPEG")
            && !compression.eq_ignore_ascii_case("WEBP")
            && co("PHOTOMETRIC").is_none_or(|p| p.eq_ignore_ascii_case("PALETTE"));
        let color_table = if self.color_table.is_empty() && palette_allowed { ds.color_table.clone() } else { self.color_table.clone() };
        let mut band_metadata = if self.band_metadata.is_empty() { ds.band_metadata.clone() } else { self.band_metadata.clone() };
        // A caller requesting grayscale/unsupported palette compression writes indices
        // as gray values. Do not retain an inherited Palette color-interpretation role.
        if color_table.is_empty() && !ds.color_table.is_empty() && self.band_metadata.is_empty() {
            for b in &mut band_metadata {
                if b.color_interp.as_deref() == Some("Palette") {
                    b.color_interp = Some("Gray".into());
                }
            }
        }
        Self {
            geo_transform: self.geo_transform.or(ds.affine),
            gcps: self.inherited_gcps(ds, driver),
            color_table,
            band_metadata,
            ..self.clone()
        }
    }
    pub(crate) fn inherited_gcps(&self, ds: &RasterDataset, driver: Option<RasterDriver>) -> Vec<Gcp> {
        if matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog))
            && self.gcps.is_empty()
            && self.geo_transform.or(ds.affine).is_none()
        {
            ds.gcps.clone()
        } else {
            self.gcps.clone()
        }
    }
}

/// Buffers sized from what a file declares, allocated fallibly. Rust ends the process when an
/// allocation fails, where GDAL and numpy report an error; a size taken from a header that no
/// allocator can satisfy must fail the read of that file, not the worker serving it.
pub(crate) mod fallible {
    use wbraster::DataType;
    use wbraster::raster::RasterData;

    /// The error of an allocation of `bytes` (`None`: more than the address space) for `what`.
    pub(crate) fn out_of_memory(bytes: Option<usize>, what: &str) -> std::io::Error {
        let size = bytes.map_or_else(|| "more bytes than the address space holds".to_string(), |b| format!("{b} bytes"));
        std::io::Error::new(std::io::ErrorKind::OutOfMemory, format!("cannot allocate {size} for {what}"))
    }

    /// The product of a buffer's dimensions, or an error when it overflows.
    pub(crate) fn len(dims: &[usize], what: &str) -> std::io::Result<usize> {
        dims.iter().try_fold(1usize, |n, &d| n.checked_mul(d)).ok_or_else(|| out_of_memory(None, what))
    }

    /// Reserves room for `extra` more elements, or an error.
    pub(crate) fn reserve<T>(v: &mut Vec<T>, extra: usize, what: &str) -> std::io::Result<()> {
        v.try_reserve_exact(extra).map_err(|_| out_of_memory(extra.checked_mul(size_of::<T>()), what))
    }

    /// `len` copies of `value`. Zeros come from the allocator zeroed (`calloc`), their pages
    /// untouched until written, once a reservation of the same size has succeeded; other values
    /// fill the reserved buffer.
    pub(crate) fn filled<T: Clone + Default + PartialEq>(len: usize, value: T, what: &str) -> std::io::Result<Vec<T>> {
        let mut v = Vec::new();
        reserve(&mut v, len, what)?;
        if value == T::default() {
            drop(v);
            return Ok(vec![value; len]);
        }
        v.resize(len, value);
        Ok(v)
    }

    /// `len` copies of `value` written now, into a reserved buffer: for buffers decoders fill whole
    /// (chunks), allocated on several threads at once.
    pub(crate) fn filled_now<T: Clone>(len: usize, value: T, what: &str) -> std::io::Result<Vec<T>> {
        let mut v = Vec::new();
        reserve(&mut v, len, what)?;
        v.resize(len, value);
        Ok(v)
    }

    /// The bytes of memory the process can use, as GDAL 3.12.1 counts them
    /// (`CPLGetUsablePhysicalRAM`): the physical memory (`MemTotal`), less under the memory limit
    /// of the process's cgroup or of one above it (`memory.max`, or `memory.limit_in_bytes` in
    /// cgroup version 1), and less under its address-space or resident-set limit (`RLIMIT_AS`,
    /// `RLIMIT_RSS`). `None` where none of them can be read (systems without `/proc`).
    ///
    /// Where the system overcommits memory, an allocation of more succeeds and the process is
    /// killed as its pages are written: a buffer sized from a file is checked against this first.
    pub fn usable_memory() -> Option<u64> {
        static PHYSICAL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let physical = *PHYSICAL.get_or_init(|| physical_memory(&|path| std::fs::read_to_string(path).ok()));
        usable_within(physical, &std::fs::read_to_string("/proc/self/limits").unwrap_or_default())
    }

    /// [`usable_memory`] read from the files `read` gives (the file's text, `None` when it cannot
    /// be read): those of [`physical_memory`] and `/proc/self/limits`. Nothing is read from the
    /// machine running the test.
    #[cfg(test)]
    pub(crate) fn usable_memory_from(read: &dyn Fn(&str) -> Option<String>) -> Option<u64> {
        usable_within(physical_memory(read), &read("/proc/self/limits").unwrap_or_default())
    }

    /// The least of `physical` and the address-space and resident-set limits in `limits`, the
    /// text of `/proc/self/limits`.
    fn usable_within(physical: Option<u64>, limits: &str) -> Option<u64> {
        // "Max address space   4294967296   4294967296   bytes": the soft limit, unless unlimited.
        let limit = |name: &str| {
            limits.lines().find_map(|l| l.strip_prefix(name)).and_then(|l| l.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok())
        };
        [physical, limit("Max address space"), limit("Max resident set")].into_iter().flatten().min()
    }

    /// GDAL's `CPLGetPhysicalRAM` on Linux: `MemTotal`, and the memory limits of the cgroups from
    /// the process's up to the root, the files read with `read` (the file's text, `None` when it
    /// cannot be read).
    pub(crate) fn physical_memory(read: &dyn Fn(&str) -> Option<String>) -> Option<u64> {
        let meminfo = read("/proc/meminfo")?;
        let total = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
        let mut most = total.checked_mul(1024)?;
        let groups = read("/proc/self/cgroup").unwrap_or_default();
        // Version 1 names the memory controller's group, read up to the root's own limit; version 2
        // has one group, read up to the one below the root.
        let (v1, group) = match groups.lines().find_map(|l| l.split_once(":memory:")) {
            Some((_, g)) => (true, g),
            None => (false, groups.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or_default()),
        };
        let mut g = group;
        while !group.is_empty() {
            let path =
                if v1 { format!("/sys/fs/cgroup/memory/{g}/memory.limit_in_bytes") } else { format!("/sys/fs/cgroup/{g}/memory.max") };
            // "max" when a version 2 group has no limit.
            if let Some(limit) = read(&path).and_then(|v| v.trim().parse::<u64>().ok()) {
                most = most.min(limit);
            }
            match g.rfind('/') {
                Some(0) if v1 && !g.is_empty() => g = "",
                Some(i) if i > 0 => g = &g[..i],
                _ => break,
            }
        }
        Some(most)
    }

    /// Buffers of at most this many bytes are not checked against [`usable_memory`].
    const UNCHECKED: usize = 1 << 24;

    /// Refuses a buffer of `bytes` (`None`: more than the address space holds) that the process
    /// cannot hold, more than its [`usable_memory`], before anything is allocated: the error is
    /// `refusal`'s message, completed with the sizes.
    pub(crate) fn check_usable(bytes: Option<usize>, refusal: impl FnOnce() -> String) -> Result<usize, String> {
        let Some(b) = bytes else {
            return Err(format!("{}: more bytes than the address space holds", refusal()));
        };
        if b <= UNCHECKED {
            return Ok(b);
        }
        match usable_memory() {
            Some(most) if b as u64 > most => {
                Err(format!("{}: {b} bytes, more than the {most} bytes of memory the process can use", refusal()))
            }
            _ => Ok(b),
        }
    }

    /// [`RasterData::new_filled`], fallibly.
    pub(crate) fn raster_data(data_type: DataType, len: usize, value: f64, what: &str) -> std::io::Result<RasterData> {
        Ok(match data_type {
            DataType::U8 => RasterData::U8(filled(len, value as u8, what)?),
            DataType::I8 => RasterData::I8(filled(len, value as i8, what)?),
            DataType::U16 => RasterData::U16(filled(len, value as u16, what)?),
            DataType::I16 => RasterData::I16(filled(len, value as i16, what)?),
            DataType::U32 => RasterData::U32(filled(len, value as u32, what)?),
            DataType::I32 => RasterData::I32(filled(len, value as i32, what)?),
            DataType::U64 => RasterData::U64(filled(len, value as u64, what)?),
            DataType::I64 => RasterData::I64(filled(len, value as i64, what)?),
            DataType::F32 => RasterData::F32(filled(len, value as f32, what)?),
            DataType::F64 => RasterData::F64(filled(len, value, what)?),
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_misspelled_layer_name_beside_a_valid_one_is_refused() {
        let options =
            |names: &[&str]| crate::VectorOpenOptions { layers: names.iter().map(|n| n.to_string()).collect(), ..Default::default() };
        let layers = || vec![crate::Layer::new("roads"), crate::Layer::new("rivers")];
        let kept = super::select_layers(layers(), "/data/map.kml", &options(&["rivers"])).unwrap();
        assert_eq!(kept.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["rivers"]);
        let error = super::select_layers(layers(), "/data/map.kml", &options(&["roads", "rivres"])).unwrap_err();
        assert_eq!(error.to_string(), "no layer named rivres (layers: roads, rivers)");
        // A single layer also answers to the file's name.
        let single = super::select_layers(vec![crate::Layer::new("layer")], "/data/map.geojson", &options(&["map"])).unwrap();
        assert_eq!(single.len(), 1);
        assert!(super::layer_selection(&["roads".into(), "x".into()], &["roads"], None, false).is_err());
    }

    #[test]
    fn layer_selection_preserves_order_and_rejects_aliases_for_multiple_layers() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let available = ["roads", "rivers"];
        let select = |n: &[&str]| super::layer_selection(&names(n), &available, Some("map.kml"), false);
        assert_eq!(select(&[]).unwrap(), [true, true]);
        assert_eq!(select(&["rivers", "roads", "roads"]).unwrap(), [true, true]);
        assert_eq!(select(&["rivers"]).unwrap(), [false, true]);
        assert_eq!(select(&["roads", "map"]).unwrap_err().to_string(), "no layer named map (layers: roads, rivers)");
        assert!(super::layer_selection(&names(&[""]), &["roads"], None, false).is_err());
        for selector in [crate::ops::render::select_layers, crate::ops::vector_filter::select_layers] {
            let layers = || available.iter().map(|n| crate::Layer::new(*n)).collect();
            assert_eq!(selector(layers(), &[]).unwrap().len(), 2);
            assert_eq!(selector(layers(), &names(&["rivers"])).unwrap()[0].name, "rivers");
            assert_eq!(
                selector(layers(), &names(&["roads", "missing"])).unwrap_err().to_string(),
                "no layer named missing (layers: roads, rivers)"
            );
        }
    }

    #[test]
    fn resource_errors_name_the_remote_source_without_cached_paths_or_queries() {
        let local = "/private/cache/input.hdf";
        let error = super::naming_dataset(
            crate::Error::ResourceLimit(format!("budget exceeded at {local}")),
            "https://example.com/input.hdf?signature=secret",
            local,
        );
        assert!(matches!(&error, crate::Error::ResourceLimit(_)));
        assert_eq!(error.to_string(), "budget exceeded at https://example.com/input.hdf");
    }

    #[cfg(feature = "grib")]
    #[test]
    fn extensionless_raster_resource_refusals_are_terminal() {
        if std::env::var("TERRIO_GRIB_MAX_POINTS").as_deref() != Ok("1") {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "dataset::tests::extensionless_raster_resource_refusals_are_terminal"])
                .env("TERRIO_GRIB_MAX_POINTS", "1")
                .output()
                .unwrap();
            assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stdout));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/grib/temperature.grib2"), &path).unwrap();
        let result = crate::open(path.to_str().unwrap());
        assert!(matches!(result, Err(crate::Error::ResourceLimit(_))), "{result:?}");
    }

    /// The memory a process can use counts its cgroup's limit and those of the groups above it
    /// (version 2's `memory.max`, version 1's `memory.limit_in_bytes`), as GDAL's
    /// `CPLGetPhysicalRAM` does: in a 2 GiB cgroup on a machine of 32 GiB without an address-space
    /// limit (a container), the cgroup's limit is what refuses a window between the two. Read from
    /// files given here, not the machine's.
    #[test]
    fn a_cgroup_limit_is_part_of_the_usable_memory() {
        use super::fallible::physical_memory;
        let machine = |files: &[(&str, &str)]| -> Option<u64> {
            let files: std::collections::HashMap<String, String> =
                files.iter().map(|(path, text)| (path.to_string(), text.to_string())).collect();
            // The groups' paths as the reader joins them, a slash doubled or not.
            physical_memory(&|path: &str| files.get(&path.replace("//", "/")).cloned())
        };
        let meminfo = ("/proc/meminfo", "MemTotal:       33554432 kB\nMemFree:         1048576 kB\n");
        let v2 = |group: &str, parent: &str| {
            machine(&[
                meminfo,
                ("/proc/self/cgroup", "0::/user.slice/app.slice/run-1.scope\n"),
                ("/sys/fs/cgroup/user.slice/app.slice/run-1.scope/memory.max", group),
                ("/sys/fs/cgroup/user.slice/app.slice/memory.max", parent),
                ("/sys/fs/cgroup/user.slice/memory.max", "max\n"),
            ])
        };
        assert_eq!(v2("2147483648\n", "max\n"), Some(2 << 30), "the process's group");
        assert_eq!(v2("max\n", "1073741824\n"), Some(1 << 30), "a group above it");
        assert_eq!(v2("max\n", "max\n"), Some(32 << 30), "no limit: the machine's memory");
        assert_eq!(v2("68719476736\n", "max\n"), Some(32 << 30), "a limit past the machine's memory");
        let v1 = machine(&[
            meminfo,
            ("/proc/self/cgroup", "12:cpu,cpuacct:/docker/1\n4:memory:/docker/1\n0::/\n"),
            ("/sys/fs/cgroup/memory/docker/1/memory.limit_in_bytes", "536870912\n"),
            ("/sys/fs/cgroup/memory/docker/memory.limit_in_bytes", "9223372036854771712\n"),
            ("/sys/fs/cgroup/memory/memory.limit_in_bytes", "9223372036854771712\n"),
        ]);
        assert_eq!(v1, Some(512 << 20), "cgroup version 1");
        assert_eq!(machine(&[meminfo]), Some(32 << 30), "no cgroup file");
        assert_eq!(machine(&[]), None, "no /proc");
    }

    /// A raster only `wbraster`'s detection recognises (an Esri ASCII grid named
    /// `.txt`) is refused unless the option allows the fallback.
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn unknown_formats_do_not_reach_wbraster() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grid.txt");
        std::fs::write(&path, "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\nNODATA_value -9999\n1 2\n3 4\n").unwrap();
        let path = path.to_str().unwrap();
        let err = crate::open_raster(path).unwrap_err().to_string();
        assert!(err.contains("TERRIO_WBRASTER_FALLBACK"), "{err}");
        crate::config::push_scope([("TERRIO_WBRASTER_FALLBACK".to_string(), Some("YES".to_string()))].into());
        let opened = crate::open_raster(path);
        crate::config::pop_scope();
        let ds = opened.expect("wbraster reads the grid when the option allows it");
        assert_eq!((ds.raster.cols, ds.raster.rows), (2, 2));
    }

    /// GeoParquet without its extension is found by its magic number when the option allows the
    /// fallback, and read by terrio's driver.
    #[cfg(all(feature = "parquet", not(feature = "gdal")))]
    #[test]
    fn geoparquet_is_detected_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("places.payload");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/parquet_zstd/zstd_v1.parquet"), &path).unwrap();
        let path = path.to_str().unwrap();
        assert!(crate::open(path).is_err());
        crate::config::push_scope([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))].into());
        let opened = crate::open(path);
        let options = crate::VectorOpenOptions { spatial_filter: Some((-5.5, -2.5, 5.5, 2.5)), ..Default::default() };
        let filtered = super::open_local(path, &options);
        crate::config::pop_scope();
        let crate::Dataset::Vector(v) = opened.unwrap() else { panic!("not a vector") };
        assert_eq!((v.driver, v.layers[0].len()), (crate::formats::VectorDriver::Parquet, 500));
        let crate::Dataset::Vector(v) = filtered.unwrap() else { panic!("not a vector") };
        assert_eq!(v.layers[0].len(), 30);
    }

    /// A vector file only `wbvector`'s detection recognises (GPX named `.payload`) is refused
    /// unless the option allows the fallback.
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn unknown_formats_do_not_reach_wbvector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.payload");
        std::fs::write(
            &path,
            r#"<?xml version="1.0"?><gpx version="1.1" creator="t" xmlns="http://www.topografix.com/GPX/1/1"><wpt lat="1.5" lon="2.5"><name>a</name></wpt></gpx>"#,
        )
        .unwrap();
        let path = path.to_str().unwrap();
        let err = crate::open(path).unwrap_err().to_string();
        assert!(err.contains("TERRIO_WBVECTOR_FALLBACK"), "{err}");
        crate::config::push_scope([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))].into());
        let opened = crate::open(path);
        crate::config::pop_scope();
        match opened.expect("wbvector reads the GPX file when the option allows it") {
            crate::Dataset::Vector(v) => assert_eq!(v.layers[0].features.len(), 1),
            crate::Dataset::Raster(_) => panic!("a raster"),
        }
    }

    /// `open_raster` (eager or lazy) and `validate_cog` (with either reader) name a local file
    /// that is missing, or a directory, in their errors, once: the operating system's messages
    /// name no file. A missing file stays an I/O error of kind `NotFound` (`NotFoundError` in
    /// Python), and a directory an I/O error of `validate_cog`, which reads it as a file. A VRT
    /// whose source is missing is not named: the error is the source's.
    #[test]
    fn local_open_errors_name_the_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("directory.tif")).unwrap();
        let gdal = crate::CogValidationOptions { profile: crate::CogValidationProfile::Gdal, ..Default::default() };
        // A lazy open of a missing `.tif` falls back to the eager open, which names it; a missing
        // `.vrt` is opened by the VRT reader.
        for name in ["missing.tif", "missing.vrt", "directory.tif"] {
            let path = dir.path().join(name);
            let path = path.to_str().unwrap();
            let errors = [
                ("open_raster", crate::open_raster(path).err()),
                ("open_raster_lazy", crate::open_raster_lazy(path).err()),
                ("validate_cog", crate::validate_cog(path, false).err()),
                ("validate_cog, GDAL profile", crate::validate_cog_with_options(path, &gdal).err()),
            ];
            for (entry, error) in errors {
                let error = error.unwrap_or_else(|| panic!("{entry}('{path}') succeeded"));
                assert_eq!(error.to_string().matches(path).count(), 1, "{entry}: {error}");
                if name.starts_with("missing") {
                    assert!(matches!(&error, crate::Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound), "{entry}: {error:?}");
                } else if entry.starts_with("validate_cog") {
                    assert!(matches!(error, crate::Error::Io(_)), "{entry}: {error:?}");
                }
            }
        }
        let vrt = dir.path().join("source.vrt");
        let source = r#"<SourceFilename relativeToVRT="1">absent.tif</SourceFilename><SourceBand>1</SourceBand>
            <SrcRect xOff="0" yOff="0" xSize="4" ySize="4"/><DstRect xOff="0" yOff="0" xSize="4" ySize="4"/>"#;
        let band = format!(r#"<VRTRasterBand dataType="Byte" band="1"><SimpleSource>{source}</SimpleSource></VRTRasterBand>"#);
        std::fs::write(&vrt, format!(r#"<VRTDataset rasterXSize="4" rasterYSize="4">{band}</VRTDataset>"#)).unwrap();
        let error = crate::open_raster(vrt.to_str().unwrap()).err().unwrap().to_string();
        assert!(error.contains("absent.tif") && !error.contains("source.vrt"), "{error}");
    }
}

/// The allow-lists and the vector and tile formats terrio finds by their content or by a file
/// beside them, whatever driver the name points to: each is read only if its driver is allowed.
/// The list is set with `TERRIO_ALLOWED_DRIVERS` on the test's thread (with the options given).
#[cfg(all(test, feature = "tiles"))]
mod content_allow_list_tests {
    use super::*;

    fn with<T>(options: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        crate::config::push_scope(options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect());
        let out = f();
        crate::config::pop_scope();
        out
    }

    fn vector(path: &str, drivers: &str) -> std::result::Result<usize, String> {
        with(&[("TERRIO_ALLOWED_DRIVERS", drivers)], || open_vector(path, &VectorOpenOptions::default()))
            .map(|v| v.layers.iter().map(|l| l.features.len()).sum())
            .map_err(|e| e.to_string())
    }

    fn assert_refused<T: std::fmt::Debug>(result: std::result::Result<T, String>, driver: &str, what: &str) {
        let refused =
            result.as_ref().is_err_and(|e| e.contains(&format!("holds {driver} data, and the {driver} driver is not allowed (allowed: ")));
        assert!(refused, "{what}: {result:?}");
    }

    /// Two points, written as an MVT directory (a `metadata.json` beside `z/x/y.pbf` tiles) and as
    /// an MBTiles archive.
    fn tiles(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let geojson = dir.join("places.geojson");
        let points = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"n":1},"geometry":{"type":"Point","coordinates":[10.5,47.5]}},
            {"type":"Feature","properties":{"n":2},"geometry":{"type":"Point","coordinates":[11.5,46.5]}}]}"#;
        std::fs::write(&geojson, points).unwrap();
        let layer = open_vector(geojson.to_str().unwrap(), &VectorOpenOptions::default()).unwrap().layers.remove(0);
        let (mvt, mbtiles) = (dir.join("places_dir"), dir.join("places.mbtiles"));
        for (path, driver) in [(&mvt, Some(VectorDriver::MVT)), (&mbtiles, None)] {
            let mut o = VectorWriteOptions { driver, ..Default::default() };
            o.tiles.max_zoom = Some(2);
            write_vector(std::slice::from_ref(&layer), path.to_str().unwrap(), &o).unwrap();
        }
        (mvt, mbtiles)
    }

    /// An MVT directory is found by its `metadata.json` whatever its name (`d.geojson`), and an MVT
    /// tile under OpenStreetMap's `.pbf` by its first bytes, which `sniff` does not name: read only
    /// with MVT allowed. An MBTiles archive named `.pmtiles` is found by its SQLite header, which
    /// `sniff` calls GPKG: read only with MBTiles allowed, from a file or bytes.
    #[test]
    fn tiles_found_by_content_or_metadata_are_read_only_with_their_driver_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let (mvt, mbtiles) = tiles(dir.path());
        let named_dir = dir.path().join("d.geojson");
        std::fs::rename(&mvt, &named_dir).unwrap();
        let tile =
            std::fs::read_dir(named_dir.join("2")).unwrap().flatten().find_map(|x| std::fs::read_dir(x.path()).ok()?.flatten().next());
        let pbf = dir.path().join("t.pbf");
        std::fs::copy(tile.unwrap().path(), &pbf).unwrap();
        let pmtiles = dir.path().join("v.pmtiles");
        std::fs::copy(&mbtiles, &pmtiles).unwrap();
        let (named_dir, pbf, pmtiles) = (named_dir.to_str().unwrap(), pbf.to_str().unwrap(), pmtiles.to_str().unwrap());
        assert_refused(vector(named_dir, "GeoJSON"), "MVT", "an MVT directory named d.geojson");
        assert!(vector(named_dir, "GeoJSON,MVT").is_ok_and(|n| n > 0));
        assert_refused(vector(pbf, "OSM"), "MVT", "an MVT tile named t.pbf");
        assert!(vector(pbf, "OSM,MVT").is_ok_and(|n| n > 0));
        assert_refused(vector(pmtiles, "PMTiles,GPKG"), "MBTiles", "an MBTiles archive named v.pmtiles");
        assert!(vector(pmtiles, "PMTiles,GPKG,MBTiles").is_ok_and(|n| n > 0));
        let bytes = |drivers: &str| {
            let b = std::fs::read(pmtiles).unwrap();
            with(&[("TERRIO_ALLOWED_DRIVERS", drivers)], || crate::memory::open_raster_lazy_bytes(b, "v.pmtiles"))
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        assert_refused(bytes("PMTiles,GPKG"), "MBTiles", "MBTiles bytes named v.pmtiles");
        let read = bytes("PMTiles,GPKG,MBTiles");
        assert!(read.as_ref().is_err_and(|e| !e.contains("not allowed")), "read as the vector archive it is: {read:?}");
    }

    /// With `TERRIO_WBVECTOR_FALLBACK=YES`, `wbvector` detects a file named for no driver by its
    /// content, as KML when `<kml` is anywhere in its first 4 KiB, where `sniff` saw a GeoJSON
    /// document: the format it finds is read only if its driver is allowed.
    #[test]
    fn the_wbvector_fallback_reads_only_allowed_drivers() {
        let dir = tempfile::tempdir().unwrap();
        let geojson = dir.path().join("q.dat2");
        std::fs::write(&geojson, r#"{"type":"FeatureCollection","features":[],"name":"<kml"}"#).unwrap();
        let kml = dir.path().join("k.dat2");
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?><kml xmlns="http://www.opengis.net/kml/2.2"><Document>
            <Placemark><name>a</name><Point><coordinates>10,47</coordinates></Point></Placemark></Document></kml>"#;
        std::fs::write(&kml, doc).unwrap();
        let open_with = |path: &Path, drivers: &str| {
            let options = [("TERRIO_ALLOWED_DRIVERS", drivers), ("TERRIO_WBVECTOR_FALLBACK", "YES")];
            with(&options, || open(path.to_str().unwrap())).map(|d| matches!(d, Dataset::Vector(_))).map_err(|e| e.to_string())
        };
        assert_refused(open_with(&geojson, "GeoJSON"), "KML", "a GeoJSON document holding '<kml'");
        assert_eq!(open_with(&kml, "KML"), Ok(true));
    }

    /// With `TERRIO_WBVECTOR_FALLBACK=YES`, a zip archive named for no driver is read as KMZ: its
    /// `doc.kml` in any case, else its first `.kml` member, its members stored or deflated. A zip
    /// archive is never XML, so one named `.xml` is read as KMZ too, even with `<kml` in its first
    /// 4 KiB: only a path without an extension beside a `.shp` file is read as the shapefile.
    // Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn the_wbvector_fallback_reads_zip_archives_as_kmz() {
        use std::io::Write as _;
        let kml = |name: &str, points: &str| {
            format!(r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><name>{name}</name>{points}</Document></kml>"#)
        };
        let point = |xy: &str| format!("<Placemark><Point><coordinates>{xy}</coordinates></Point></Placemark>");
        let archive = |method: zip::CompressionMethod, members: &[(&str, String)]| {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default().compression_method(method);
            for (member, text) in members {
                zip.start_file(*member, options).unwrap();
                zip.write_all(text.as_bytes()).unwrap();
            }
            zip.finish().unwrap().into_inner()
        };
        let main = kml("main", &[point("10.5,47.25"), point("11.5,46.5")].concat());
        let members = [("notes.txt", "-".to_string()), ("a.kml", kml("first", &point("1,2"))), ("DOC.KML", main)];
        let (stored, deflated) = (archive(zip::CompressionMethod::Stored, &members), archive(zip::CompressionMethod::Deflated, &members));
        let shows_kml = |bytes: &[u8]| bytes[..bytes.len().min(4096)].windows(4).any(|w| w == b"<kml");
        assert_eq!((shows_kml(&stored), shows_kml(&deflated)), (true, false), "<kml in the first 4 KiB: (stored, deflated)");
        let dir = tempfile::tempdir().unwrap();
        let [payload, xml, deflated_xml, bare] =
            ["places.payload", "places.xml", "deflated.xml", "places"].map(|name| dir.path().join(name));
        for (path, bytes) in [(&payload, &stored), (&xml, &stored), (&deflated_xml, &deflated), (&bare, &stored)] {
            std::fs::write(path, bytes).unwrap();
        }
        let mut beside = Layer::new("places").with_geom_type(wbvector::GeometryType::Point).with_epsg(4326);
        beside.add_feature(Some(wbvector::Geometry::point(5.0, 6.0)), &[]).unwrap();
        write_vector(&[beside], dir.path().join("places.shp").to_str().unwrap(), &VectorWriteOptions::default()).unwrap();
        let open_with = |path: &Path, options: &[(&str, &str)]| with(options, || open(path.to_str().unwrap())).map_err(|e| e.to_string());
        let geometries = |v: &VectorDataset| v.layers[0].features.iter().map(|f| f.geometry.clone()).collect::<Vec<_>>();
        let refused = open_with(&payload, &[]).map(|_| ());
        assert!(refused.as_ref().is_err_and(|e| e.contains("TERRIO_WBVECTOR_FALLBACK")), "{refused:?}");
        let fallback = [("TERRIO_WBVECTOR_FALLBACK", "YES")];
        for path in [&payload, &xml, &deflated_xml] {
            let read = open_with(path, &fallback);
            let Ok(Dataset::Vector(v)) = read else { panic!("{path:?} not read as KMZ: {read:?}") };
            let points = [Some(wbvector::Geometry::point(10.5, 47.25)), Some(wbvector::Geometry::point(11.5, 46.5))];
            assert_eq!(geometries(&v), points, "{path:?}");
            assert_eq!((v.layers.len(), v.layers[0].name.as_str(), v.driver), (1, "main", VectorDriver::GeoJSON), "{path:?}");
        }
        // The fallback's own allow-list check (`open` checks the list first, on the name and content).
        let listed = |drivers: &str| {
            let options = [("TERRIO_WBVECTOR_FALLBACK", "YES"), ("TERRIO_ALLOWED_DRIVERS", drivers)];
            with(&options, || open_local(payload.to_str().unwrap(), &VectorOpenOptions::default())).map(|_| ()).map_err(|e| e.to_string())
        };
        assert_refused(listed("GeoJSON"), "KMZ", "a zip archive found by the fallback");
        assert_eq!(listed("KMZ"), Ok(()));
        // The shapefile beside a path without an extension is read instead.
        let Ok(Dataset::Vector(v)) = open_with(&bare, &fallback) else { panic!("the shapefile not read") };
        assert_eq!(geometries(&v), [Some(wbvector::Geometry::point(5.0, 6.0))]);
    }

    /// A member of [`raw_zip`]: its name, its bytes and its general purpose flags.
    #[cfg(not(feature = "gdal"))]
    type Member<'a> = (&'a str, &'a [u8], u16);

    /// A zip archive of stored members written byte by byte (APPNOTE 6.3.10, 4.3.7, 4.3.12 and
    /// 4.3.16), for what the zip writer does not write: two members with one name, a member flagged
    /// encrypted (4.4.4, bit 0) without being encrypted.
    #[cfg(not(feature = "gdal"))]
    fn raw_zip(members: &[Member]) -> Vec<u8> {
        let (mut zip, mut directory) = (Vec::new(), Vec::new());
        for &(name, data, flags) in members {
            let crc =
                !data.iter().fold(!0u32, |c, &b| (0..8).fold(c ^ u32::from(b), |c, _| (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg())));
            let (size, length) = ((data.len() as u32).to_le_bytes(), (name.len() as u16).to_le_bytes());
            let fields =
                [&20u16.to_le_bytes()[..], &flags.to_le_bytes(), &[0; 6], &crc.to_le_bytes(), &size, &size, &length, &[0; 2]].concat();
            directory.extend([&b"PK\x01\x02\x14\x00"[..], &fields, &[0; 10], &(zip.len() as u32).to_le_bytes(), name.as_bytes()].concat());
            zip.extend([&b"PK\x03\x04"[..], &fields, name.as_bytes(), data].concat());
        }
        let (count, size, start) =
            ((members.len() as u16).to_le_bytes(), (directory.len() as u32).to_le_bytes(), (zip.len() as u32).to_le_bytes());
        [zip, directory, [&b"PK\x05\x06"[..], &[0; 4], &count, &count, &size, &start, &[0; 2]].concat()].concat()
    }

    /// With `TERRIO_WBVECTOR_FALLBACK=YES`, a zip archive named for no driver is read from the
    /// member the KMZ driver reads from the same archive named `.kmz`: its first member named
    /// `doc.kml` (in any case) at the archive's root, else its first `.kml` member. An archive with
    /// two `doc.kml` members reads the last one (zip keeps one entry per name, the last), and a
    /// member that cannot be opened (encrypted) stops the read only if it is the one read. The
    /// document is decoded as the KMZ driver decodes it: Latin-1 when it is not UTF-8.
    #[cfg(not(feature = "gdal"))]
    #[test]
    fn the_wbvector_fallback_reads_the_kmz_member_the_kmz_driver_reads() {
        let kml = |name: &str| {
            let point = "<Placemark><Point><coordinates>1,2</coordinates></Point></Placemark>";
            format!(r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><name>{name}</name>{point}</Document></kml>"#)
        };
        let [first, last, upper, a, b] = ["first", "last", "upper", "a", "b"].map(kml);
        let (first, last, upper, a, b) = (first.as_bytes(), last.as_bytes(), upper.as_bytes(), a.as_bytes(), b.as_bytes());
        // A document in Latin-1 (é is the byte 0xE9), not UTF-8.
        let latin1: Vec<u8> =
            format!(r#"<?xml version="1.0" encoding="ISO-8859-1"?>{}"#, kml("caf\u{e9}")).chars().map(|c| c as u8).collect();
        let cases: [(&str, &[Member], &str); 7] = [
            ("twice", &[("doc.kml", first, 0), ("doc.kml", last, 0)], "last"),
            ("cases", &[("DOC.KML", upper, 0), ("doc.kml", last, 0)], "upper"),
            ("no_doc", &[("notes.txt", b"-", 0), ("b.kml", b, 0), ("a.kml", a, 0)], "b"),
            ("nested", &[("b.kml", b, 0), ("files/doc.kml", a, 0)], "b"),
            ("locked_before", &[("secret.txt", b"-", 1), ("doc.kml", last, 0)], "last"),
            ("locked_after", &[("doc.kml", last, 0), ("secret.txt", b"-", 1)], "last"),
            ("latin1", &[("doc.kml", &latin1, 0)], "caf\u{e9}"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let read = cases.map(|(name, members, _)| {
            let bytes = raw_zip(members);
            let read = ["payload", "kmz"].map(|extension| {
                let path = dir.path().join(format!("{name}.{extension}"));
                std::fs::write(&path, &bytes).unwrap();
                match with(&[("TERRIO_WBVECTOR_FALLBACK", "YES")], || open(path.to_str().unwrap())) {
                    Ok(Dataset::Vector(v)) => Ok(v.layers[0].name.clone()),
                    other => Err(format!("{other:?}")),
                }
            });
            (name, read)
        });
        let expected = cases.map(|(name, _, layer)| (name, [Ok(layer.to_string()), Ok(layer.to_string())]));
        assert_eq!(read, expected, "the layer read from each archive: [by the fallback, by the KMZ driver]");
    }
}
