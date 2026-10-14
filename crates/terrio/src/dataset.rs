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
}

impl RasterDataset {
    /// Wraps a raster created in memory.
    pub fn new(raster: Raster, has_nodata: bool) -> Self {
        Self { raster, driver: RasterDriver::GTiff, has_nodata }
    }

    /// The CRS.
    pub fn crs(&self) -> SpatialRef {
        SpatialRef::from_raster_crs(&self.raster.crs)
    }

    /// The GDAL-style geotransform: `[x_origin, pixel_width, 0, y_origin, 0, -pixel_height]`.
    pub fn geo_transform(&self) -> [f64; 6] {
        geo_transform(&self.raster)
    }

    /// The nodata value, when the file declares one.
    pub fn nodata(&self) -> Option<f64> {
        self.has_nodata.then_some(self.raster.nodata)
    }
}

/// The GDAL-style geotransform of a raster.
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
}

/// Opens a dataset, raster or vector, detecting its format.
pub fn open(path: &str) -> Result<Dataset> {
    if let Some(result) = crate::drivers::open_subdataset(path) {
        return result.map(Dataset::Raster);
    }
    let local = vfs::resolve(path)?;
    open_local(local.as_str(), &VectorOpenOptions::default())
}

/// Opens a raster dataset.
pub fn open_raster(path: &str) -> Result<RasterDataset> {
    if let Some(result) = crate::drivers::open_subdataset(path) {
        return result;
    }
    let local = vfs::resolve(path)?;
    read_raster_local(local.as_str())
}

/// Opens a vector dataset.
pub fn open_vector(path: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    let local = vfs::resolve(path)?;
    match open_local(local.as_str(), options)? {
        Dataset::Vector(v) => Ok(v),
        Dataset::Raster(_) => Err(Error::Unsupported(format!("'{path}' is a raster, not a vector dataset"))),
    }
}

/// The layer names of a vector dataset, without reading the features when the format allows it.
pub fn list_layers(path: &str) -> Result<Vec<String>> {
    let local = vfs::resolve(path)?;
    if VectorDriver::for_extension(local.as_str()) == Some(VectorDriver::GPKG) {
        return Ok(wbvector::geopackage::list_layers(local.as_str())?);
    }
    Ok(open_vector(local.as_str(), &VectorOpenOptions::default())?.layers.into_iter().map(|l| l.name).collect())
}

fn open_local(path: &str, options: &VectorOpenOptions) -> Result<Dataset> {
    if !Path::new(path).exists() {
        return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("'{path}' does not exist"))));
    }
    let ext = extension(path).unwrap_or_default();
    // GeoPackages hold vector layers, raster tiles or both: prefer the vector layers.
    if ext == "gpkg" {
        let names = wbvector::geopackage::list_layers(path).unwrap_or_default();
        if names.is_empty() {
            return read_raster_local(path).map(Dataset::Raster);
        }
        return read_vector_local(path, VectorDriver::GPKG, options).map(Dataset::Vector);
    }
    if let Some(driver) = VectorDriver::for_extension(path) {
        return read_vector_local(path, driver, options).map(Dataset::Vector);
    }
    if RasterDriver::for_extension(path).is_some() || Path::new(path).is_dir() {
        return read_raster_local(path).map(Dataset::Raster);
    }
    // Unknown extension: let the drivers sniff the content.
    match read_raster_local(path) {
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
    let layers = if driver == VectorDriver::GPKG {
        let names = wbvector::geopackage::list_layers(path)?;
        let wanted: Vec<&String> =
            if options.layers.is_empty() { names.iter().collect() } else { names.iter().filter(|n| options.layers.contains(n)).collect() };
        if wanted.is_empty() && !options.layers.is_empty() {
            return Err(Error::InvalidArgument(format!("no layer named {} (layers: {})", options.layers.join(", "), names.join(", "))));
        }
        wanted.into_iter().map(|n| wbvector::geopackage::read_layer(path, n)).collect::<std::result::Result<Vec<_>, _>>()?
    } else if let Some(layers) = crate::drivers::vector::read(path, driver, options)? {
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
    let layers = layers
        .into_iter()
        .map(|mut l| {
            crate::drivers::vector::fix(&mut l, driver);
            fix_layer(l, path, driver)
        })
        .collect();
    Ok(VectorDataset { layers, driver })
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
    // Re-identify the EPSG code of stored WKT (such as a shapefile's ESRI .prj) the way GDAL does.
    if let Some(wkt) = layer.crs_wkt().map(str::to_string)
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
    /// default to `NONE`, COGs to `LZW`, as in GDAL. Zarr: `NONE`, `ZLIB`, `GZIP`.
    pub compression: Option<String>,
    /// Write GeoTIFF tiles of this size (COG tiles default to 512).
    pub tile_size: Option<u32>,
    /// Force BigTIFF.
    pub bigtiff: bool,
}

/// Writes a raster, declaring its nodata value.
pub fn write_raster(raster: &Raster, path: &str, options: &RasterWriteOptions) -> Result<()> {
    write_raster_with_nodata(raster, Some(raster.nodata), path, options)
}

/// Writes a raster dataset, declaring a nodata value only when the dataset has one.
pub fn write_raster_dataset(ds: &RasterDataset, path: &str, options: &RasterWriteOptions) -> Result<()> {
    write_raster_with_nodata(&ds.raster, ds.nodata(), path, options)
}

fn write_raster_with_nodata(raster: &Raster, nodata: Option<f64>, path: &str, options: &RasterWriteOptions) -> Result<()> {
    ensure_writable_location(path)?;
    let driver = match options.driver {
        Some(d) => d,
        None => RasterDriver::for_extension(path)
            .ok_or_else(|| Error::InvalidArgument(format!("cannot tell the output format of '{path}': give it explicitly")))?,
    };
    if !driver.info().write {
        return Err(Error::Unsupported(format!("the {} driver cannot write", driver.name())));
    }
    if matches!(driver, RasterDriver::GTiff | RasterDriver::Cog) {
        use crate::drivers::gtiff_write::{Compression, TiffOptions};
        let name = options.compression.as_deref().unwrap_or(if driver == RasterDriver::Cog { "LZW" } else { "NONE" });
        // terrio's writer handles the lossless compressions; wbraster the lossy ones.
        if let Some(compression) = Compression::parse(name) {
            let opts = TiffOptions {
                compression,
                tile_size: options.tile_size,
                bigtiff: options.bigtiff,
                cog: driver == RasterDriver::Cog,
                nodata,
                ..Default::default()
            };
            return crate::drivers::gtiff_write::write(raster, path, &opts);
        }
    }
    if let Some(result) = crate::drivers::write(raster, nodata, path, driver, options.compression.as_deref()) {
        return result;
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
}

/// Writes vector layers. GeoPackage, GPX, KML, KMZ and TopoJSON hold several layers in one file;
/// for the other formats, write one layer at a time.
pub fn write_vector(layers: &[Layer], path: &str, options: &VectorWriteOptions) -> Result<()> {
    ensure_writable_location(path)?;
    let driver = match options.driver {
        Some(d) => d,
        None => VectorDriver::for_extension(path)
            .ok_or_else(|| Error::InvalidArgument(format!("cannot tell the output format of '{path}': give it explicitly")))?,
    };
    if !driver.info().write {
        return Err(Error::Unsupported(format!("the {} driver cannot write", driver.name())));
    }
    match (driver, layers) {
        (_, []) => Err(Error::InvalidArgument("no layers to write".into())),
        (VectorDriver::GPKG, _) => crate::drivers::vector::geopackage::write(layers, path),
        (VectorDriver::GPX, _) => crate::drivers::vector::gpx::write_layers(layers, path),
        (VectorDriver::KML, _) => crate::drivers::vector::kml::write(layers, path),
        (VectorDriver::KMZ, _) => crate::drivers::vector::kml::write_kmz(layers, path),
        (VectorDriver::TopoJSON, _) => crate::drivers::vector::topojson::write_layers(layers, path),
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

fn ensure_writable_location(path: &str) -> Result<()> {
    if !vfs::Location::parse(path).is_local() {
        return Err(Error::Unsupported(format!("cannot write to '{path}': only local outputs are supported")));
    }
    Ok(())
}
