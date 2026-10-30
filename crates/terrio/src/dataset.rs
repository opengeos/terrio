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
    /// Per-band nodata in band order; empty means the shared raster value applies.
    /// A nonempty vector overrides has_nodata/raster.nodata, including explicit None.
    /// Clear or update this vector when changing the shared nodata fields.
    pub band_nodata: Vec<Option<f64>>,
    /// Descriptive band properties, in band order; empty entries use driver defaults.
    pub band_metadata: Vec<BandMetadata>,
    /// RGBA palette entries of a single-band indexed image, empty for direct colors.
    pub color_table: Vec<[u8; 4]>,
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
}

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
            color_table: Vec::new(),
            gcps: Vec::new(),
            affine: None,
            complex: false,
            geolocation: None,
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
    /// Only these layers (by name). Empty means all.
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
    /// How to read HEC-RAS geometry and plan HDF files: meshes and the stored values to join.
    pub hecras: crate::drivers::vector::hecras_options::HecRasOpenOptions,
}

impl VectorOpenOptions {
    /// Sets a driver open option by its GDAL name (`KEY`, `VALUE`): the CSV options (see
    /// [`CsvOptions::set`](crate::CsvOptions::set)), the shapefile options
    /// ([`ShapefileOptions::set`](crate::ShapefileOptions::set)), the tile archive options
    /// ([`TileOpenOptions::set`](crate::tiles::TileOpenOptions::set)) and the HECRAS options
    /// ([`HecRasOpenOptions::set`](crate::HecRasOpenOptions::set)). Returns `false` for a name
    /// none of them knows.
    ///
    /// ```
    /// let mut options = terrio::VectorOpenOptions::default();
    /// assert!(options.set("CELL_SUMMARIES", "Maximum Water Surface")?);
    /// assert!(!options.set("NO_SUCH_OPTION", "1")?);
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        Ok(self.csv.set(key, value)? || self.shapefile.set(key, value)? || self.tiles.set(key, value)? || self.hecras.set(key, value)?)
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
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::open(rest);
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
    let local = vfs::resolve(path)?;
    open_local(local.as_str(), &VectorOpenOptions::default())
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
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::open_raster(rest);
    }
    let result = open_raster_native(path);
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
    crate::geotiff::read_geotiff_with_limits(Path::new(local.as_str()), limits)
}

fn open_raster_native(path: &str) -> Result<RasterDataset> {
    if let Some(result) = crate::catalog::open_lazy(path) {
        return result?.read();
    }
    if let Some(result) = crate::stac::open(path) {
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
    let local = vfs::resolve(path)?;
    read_raster_local(local.as_str())
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
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::open_vector(rest, options);
    }
    let result = open_vector_native(path, options);
    #[cfg(feature = "gdal")]
    let result = gdal_fallback(path, result, |p| crate::gdal_backend::open_vector(p, options));
    result
}

pub(crate) fn open_vector_native(path: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
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
    #[cfg(feature = "gdal")]
    if let Some(rest) = crate::gdal_backend::strip_prefix(path) {
        return crate::gdal_backend::list_layers(rest);
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
/// error is kept. Missing local files and invalid arguments are not retried, nor are network
/// errors.
#[cfg(feature = "gdal")]
pub(crate) fn gdal_fallback<T>(path: &str, result: Result<T>, gdal: impl FnOnce(&str) -> Result<T>) -> Result<T> {
    let Err(e) = result else { return result };
    let retry = match &e {
        Error::InvalidArgument(_) | Error::Http(_) => false,
        // A missing file, unless the path is one of GDAL's dataset names (`HDF5:"f.h5"://x`).
        Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => path.contains(':') || path.starts_with("/vsi"),
        _ => true,
    };
    if !retry {
        return Err(e);
    }
    gdal(path).map_err(|_| e)
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
    return crate::tiles::open_dataset(path, options);
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
        Err(raster_err) => match wbvector::VectorFormat::detect(path) {
            Ok(_) => {
                let layer = wbvector::read(path)?;
                Ok(Dataset::Vector(VectorDataset {
                    layers: vec![fix_layer(layer, path, VectorDriver::GeoJSON)],
                    driver: VectorDriver::GeoJSON,
                }))
            }
            Err(_) => Err(Error::Unsupported(format!("'{path}': format not recognized ({raster_err})"))),
        },
    }
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
        Some(driver) => (Raster::read_with_format(path, driver.to_wb())?, driver),
        None => {
            let fmt = wbraster::RasterFormat::detect(path)?;
            (fmt.read(path)?, RasterDriver::from_wb(fmt))
        }
    };
    let mut ds = crate::drivers::nodata::finish(path, driver, raster);
    crate::drivers::nodata::apply_pam(path, &mut ds);
    Ok(ds)
}

fn read_vector_local(path: &str, driver: VectorDriver, options: &VectorOpenOptions) -> Result<VectorDataset> {
    driver.ensure_built()?;
    #[cfg(feature = "sqlite")]
    if driver == VectorDriver::GPKG {
        let layers = crate::drivers::vector::gpkg_read::read(path, &options.layers, options.spatial_filter)?;
        return Ok(finish_vector(layers, path, driver));
    }
    let layers = if let Some(layers) = crate::drivers::vector::read(path, driver, options)? {
        select_layers(layers, path, options)?
    } else {
        let format = driver.to_wb().ok_or_else(|| Error::Unsupported(format!("the {} driver cannot read", driver.name())))?;
        let layer = format.read(path)?;
        if !options.layers.is_empty() && !options.layers.contains(&layer.name) {
            let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            if !options.layers.iter().any(|l| l == stem) {
                return Err(Error::InvalidArgument(format!("no layer named {}", options.layers.join(", "))));
            }
        }
        vec![layer]
    };
    Ok(finish_vector(layers, path, driver))
}

/// A vector dataset of layers a driver read, normalized.
pub(crate) fn finish_vector(layers: Vec<Layer>, path: &str, driver: VectorDriver) -> VectorDataset {
    let layers = layers.into_iter().map(|l| fix_layer(l, path, driver)).collect();
    VectorDataset { layers, driver }
}

/// Keeps the layers `options` asks for (all when it names none).
fn select_layers(layers: Vec<Layer>, path: &str, options: &VectorOpenOptions) -> Result<Vec<Layer>> {
    if options.layers.is_empty() {
        return Ok(layers);
    }
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let single = layers.len() == 1;
    let names: Vec<String> = layers.iter().map(|l| l.name.clone()).collect();
    let kept: Vec<Layer> =
        layers.into_iter().filter(|l| options.layers.contains(&l.name) || single && options.layers.iter().any(|n| n == stem)).collect();
    if kept.is_empty() {
        return Err(Error::InvalidArgument(format!("no layer named {} (layers: {})", options.layers.join(", "), names.join(", "))));
    }
    Ok(kept)
}

/// Normalizes what drivers leave inconsistent: the layer name and the CRS of formats that are
/// WGS 84 by definition.
pub(crate) fn fix_layer(mut layer: Layer, path: &str, driver: VectorDriver) -> Layer {
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    if layer.name.is_empty() || (layer.name == "layer" && driver != VectorDriver::GPKG) {
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
    /// Overview resampling: `nearest`, `average`, `min`, `max` or `mode`. Defaults to `nearest`
    /// for GeoTIFF and `average` for COG. COG levels reduce full-resolution blocks independently:
    /// nearest takes their center sample; averages skip nodata and weight colors by alpha when
    /// `ALPHA=YES` (alpha itself is averaged without that weight).
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
        d => raster.write(path, d.to_wb())?,
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
            return Err(Error::InvalidArgument(format!("unknown compression '{other}' (NONE, LZW, DEFLATE, ZSTD, PACKBITS, JPEG, WEBP)")));
        }
    })
}

/// Options for writing vector datasets.
#[derive(Debug, Clone, Default)]
pub struct VectorWriteOptions {
    /// The output format. By default, chosen from the output path's extension.
    pub driver: Option<VectorDriver>,
    /// How to write CSV files.
    pub csv: crate::drivers::vector::csv::CsvOptions,
    /// How to write GeoJSON files.
    pub geojson: crate::drivers::vector::geojson::GeoJsonOptions,
    /// How to write vector tiles (PMTiles, MBTiles, MVT): zoom levels, buffer, simplification.
    pub tiles: crate::tiles::TileWriteOptions,
    /// How to write KMZ archives: the files, such as icon images, packed with the documents.
    pub kmz: crate::drivers::vector::kml::KmzOptions,
}

/// Writes vector layers. GeoPackage, GPX, KML, KMZ and TopoJSON hold several layers in one file;
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
                ensure_kmz_resources_apply(options, None)?;
                return crate::gdal_backend::write_vector(layers, path, &d, &[], &[]);
            }
            return Err(Error::InvalidArgument(format!("cannot tell the output format of '{path}': give it explicitly")));
        }
    };
    ensure_kmz_resources_apply(options, Some(driver))?;
    driver.ensure_built()?;
    if !driver.info().write {
        return Err(Error::Unsupported(format!("the {} driver cannot write", driver.name())));
    }
    match (driver, layers) {
        (_, []) => Err(Error::InvalidArgument("no layers to write".into())),
        #[cfg(feature = "sqlite")]
        (VectorDriver::GPKG, _) => crate::drivers::vector::geopackage::write(layers, path),
        #[cfg(feature = "cad")]
        (d @ (VectorDriver::DXF | VectorDriver::DWG), _) => {
            std::fs::write(path, crate::drivers::vector::cad::to_bytes(layers, d)?)?;
            Ok(())
        }
        (VectorDriver::GPX, _) => crate::drivers::vector::gpx::write_layers(layers, path),
        (VectorDriver::KML, _) => crate::drivers::vector::kml::write(layers, path),
        (VectorDriver::KMZ, _) => crate::drivers::vector::kml::write_kmz(layers, path, &options.kmz),
        (VectorDriver::TopoJSON, _) => crate::drivers::vector::topojson::write_layers(layers, path),
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

/// KMZ resources are packed into KMZ archives only, so any other output refuses them rather than
/// dropping them. `driver` is `None` for a format only GDAL writes.
pub(crate) fn ensure_kmz_resources_apply(options: &VectorWriteOptions, driver: Option<VectorDriver>) -> Result<()> {
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
