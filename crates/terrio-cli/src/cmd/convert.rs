//! `terrio convert`.

use terrio::crs::SpatialRef;
use terrio::formats::{RasterDriver, VectorDriver};
use terrio::{Dataset, RasterWriteOptions, VectorOpenOptions, VectorWriteOptions, Window, WindowOptions};

#[derive(clap::Args)]
pub struct Args {
    /// The input dataset.
    input: String,
    /// The output path.
    output: String,
    /// The output format's short name (`GTiff`, `COG`, `GPKG`, `ESRI Shapefile`...). By default,
    /// chosen from the output's extension. Builds with the GDAL backend also write with any GDAL
    /// driver: rasters (`NITF`, `KEA`, `GRIB`...) and vector layers (`DXF`, `OpenFileGDB`,
    /// `MapInfo File`, `SQLite`...), and choose GDAL's driver for extensions terrio has none for.
    #[arg(short = 'f', long = "format")]
    format: Option<String>,
    /// Only these layers of a vector dataset.
    #[arg(short, long = "layer")]
    layers: Vec<String>,
    /// Open the input with this driver: `gdal` uses the GDAL backend (builds with the `gdal`
    /// feature), as a `GDAL:` prefix on the path does.
    #[arg(long, value_name = "DRIVER")]
    driver: Option<String>,
    /// Reproject into this CRS.
    #[arg(long = "t-srs", value_name = "CRS")]
    t_srs: Option<String>,
    /// Compression: GeoTIFF/COG NONE, LZW, DEFLATE, ZSTD, PACKBITS, JPEG, WEBP; Zarr NONE, ZLIB (v2),
    /// GZIP, ZSTD; GeoPackage tiles PNG, JPEG.
    #[arg(long)]
    compress: Option<String>,
    /// GeoTIFF/COG tile size.
    #[arg(long)]
    tile_size: Option<u32>,
    /// Write BigTIFF.
    #[arg(long)]
    bigtiff: bool,
    /// Convert only this window of a raster: first column, first row, width and height (in the
    /// overview's pixels with --overview). GeoTIFFs, local or remote, are read in place.
    #[arg(long, num_args = 4, value_names = ["XOFF", "YOFF", "XSIZE", "YSIZE"])]
    srcwin: Option<Vec<usize>>,
    /// Convert a raster's overview N (0 is the largest) instead of its full resolution.
    #[arg(long, value_name = "N")]
    overview: Option<usize>,
    /// An open option, as GDAL names it: `KEY=VALUE` (CSV: X_POSSIBLE_NAMES, Y_POSSIBLE_NAMES,
    /// GEOM_POSSIBLE_NAMES, AUTODETECT_TYPE, KEEP_GEOM_COLUMNS, SEPARATOR; PMTiles, MBTiles and MVT:
    /// ZOOM_LEVEL, CLIP).
    #[arg(long = "oo", value_name = "KEY=VALUE")]
    open_options: Vec<String>,
    /// A vector layer creation option, as GDAL names it: `KEY=VALUE` (CSV: GEOMETRY=AS_WKT|AS_XY;
    /// GeoJSON: RFC7946=YES; vector tiles: MINZOOM, MAXZOOM, EXTENT, BUFFER, SIMPLIFICATION,
    /// SIMPLIFICATION_MAX_ZOOM, NAME, DESCRIPTION, TYPE).
    #[arg(long = "lco", value_name = "KEY=VALUE")]
    creation_options: Vec<String>,
    /// A dataset creation option of a GDAL vector driver (`ogr2ogr -dsco`): `KEY=VALUE`. Builds
    /// with the GDAL backend only; GDAL drivers also take `--lco` as GDAL defines it.
    #[arg(long = "dsco", value_name = "KEY=VALUE")]
    dataset_options: Vec<String>,
    /// A raster creation option, as GDAL names it: `KEY=VALUE` (Zarr: FORMAT=ZARR_V2|ZARR_V3;
    /// netCDF: FORMAT=NC|NC2|NC5; PMTiles and MBTiles: TILE_FORMAT, RESAMPLING,
    /// ZOOM_LEVEL_STRATEGY, MINZOOM, MAXZOOM, QUALITY, NAME, DESCRIPTION, TYPE).
    #[arg(long = "co", value_name = "KEY=VALUE")]
    raster_options: Vec<String>,
    /// Tile archives (PMTiles, MBTiles, MVT): the lowest zoom level written.
    #[arg(long)]
    minzoom: Option<u8>,
    /// Tile archives (PMTiles, MBTiles, MVT): the highest zoom level written.
    #[arg(long)]
    maxzoom: Option<u8>,
    #[command(flatten)]
    vector: super::vector_opts::VectorArgs,
    #[command(flatten)]
    points: super::points::PointArgs,
    #[command(flatten)]
    point_write: super::points::PointWriteArgs,
}

/// Splits `KEY=VALUE` options.
pub(super) fn key_values(options: &[String]) -> terrio::Result<Vec<(&str, &str)>> {
    options
        .iter()
        .map(|o| o.split_once('=').ok_or_else(|| terrio::Error::InvalidArgument(format!("option '{o}' is not KEY=VALUE"))))
        .collect()
}

/// The options of a vector conversion that reads and writes the features as streams, a chunk
/// at a time (see `terrio::stream`), when the formats and options allow it. `None` otherwise.
fn stream_options(a: &Args) -> terrio::Result<Option<terrio::stream::ConvertOptions>> {
    use terrio::ops::vector_filter::WriteMode;
    let driver = match a.format.as_deref() {
        Some(f) => match VectorDriver::from_name(f) {
            Some(d) => Some(d),
            None => return Ok(None),
        },
        None => None,
    };
    if a.srcwin.is_some() || a.overview.is_some() || a.vector.write_mode() != WriteMode::Create || terrio::tiles::is_archive(&a.input) {
        return Ok(None);
    }
    let t_srs = a.t_srs.as_deref().map(SpatialRef::parse).transpose()?;
    let translate = a.vector.to_options(t_srs)?;
    let mut open = VectorOpenOptions { layers: a.layers.clone(), spatial_filter: a.vector.spatial_hint(&a.input), ..Default::default() };
    for (k, v) in key_values(&a.open_options)? {
        if !open.csv.set(k, v)? && !open.tiles.set(k, v)? {
            return Err(terrio::Error::InvalidArgument(format!("unknown open option '{k}'")));
        }
    }
    let mut write = VectorWriteOptions { driver, ..Default::default() };
    for (k, v) in key_values(&a.creation_options)? {
        if !(write.csv.set(k, v)? || write.geojson.set(k, v)?) {
            return Ok(None);
        }
    }
    let options = terrio::stream::ConvertOptions { open, translate, write, ..Default::default() };
    Ok(terrio::stream::can_convert_stream(&a.input, &a.output, &options).then_some(options))
}

/// A GeoTIFF converted to a GeoTIFF or COG with no option that needs the raster whole: the input
/// opened lazily, to be written a strip of rows at a time (see `terrio::LazyRaster::write`), and
/// the write options. `None` otherwise.
fn streamed_raster(a: &Args) -> terrio::Result<Option<(terrio::LazyRaster, RasterWriteOptions)>> {
    let driver = match a.format.as_deref() {
        Some(f) => match RasterDriver::from_name(f) {
            Some(d) => Some(d),
            None => return Ok(None),
        },
        None => None,
    };
    let tiff_out = matches!(driver.or_else(|| RasterDriver::for_extension(&a.output)), Some(RasterDriver::GTiff | RasterDriver::Cog));
    let plain = a.srcwin.is_none()
        && a.overview.is_none()
        && a.t_srs.is_none()
        && !a.vector.is_set()
        && a.minzoom.is_none()
        && a.maxzoom.is_none()
        && a.layers.is_empty()
        && a.open_options.is_empty();
    if !tiff_out || !plain || RasterDriver::for_extension(&a.input) != Some(RasterDriver::GTiff) {
        return Ok(None);
    }
    let r = super::quiet(|| terrio::open_raster_lazy(&a.input))?;
    let creation_options = key_values(&a.raster_options)?.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let options = RasterWriteOptions {
        driver,
        compression: a.compress.clone(),
        tile_size: a.tile_size,
        bigtiff: a.bigtiff,
        creation_options,
        ..Default::default()
    };
    Ok(Some((r, options)))
}

pub fn run(mut a: Args) -> terrio::Result<()> {
    a.input = super::with_driver(&a.input, a.driver.as_deref())?;
    if terrio::pointcloud::is_point_cloud_path(&a.input) {
        let t_srs = a.t_srs.as_deref().map(SpatialRef::parse).transpose()?;
        let format = a.format.as_deref().filter(|f| terrio::pointcloud::PointFormat::from_name(f).is_some());
        return super::points::convert(&a.input, &a.output, format, t_srs, &a.vector, &a.points, &a.point_write);
    }
    if let Some(options) = stream_options(&a)? {
        return terrio::stream::convert(&a.input, &a.output, &options);
    }
    if let Some((mut r, options)) = streamed_raster(&a)? {
        return r.write(&a.output, &options);
    }
    let ds = if a.srcwin.is_some() || a.overview.is_some() {
        let mut r = super::quiet(|| terrio::open_raster_lazy(&a.input))?;
        let opts = WindowOptions { bands: Vec::new(), overview: a.overview };
        let window = match a.srcwin.as_deref() {
            Some(&[x, y, w, h]) => Window::new(x, y, w, h),
            _ => {
                let (w, h) = match a.overview {
                    Some(i) => r.overviews().get(i).copied().ok_or_else(|| {
                        terrio::Error::InvalidArgument(format!("no overview {i}: the raster has {} overview(s)", r.overviews().len()))
                    })?,
                    None => (r.width(), r.height()),
                };
                Window::full(w, h)
            }
        };
        Dataset::Raster(r.read_window(window, &opts)?)
    } else if a.layers.is_empty() && a.open_options.is_empty() && a.vector.spatial_hint(&a.input).is_none() {
        super::quiet(|| terrio::open(&a.input))?
    } else {
        let spatial_filter = a.vector.spatial_hint(&a.input);
        let mut options = VectorOpenOptions { layers: a.layers.clone(), spatial_filter, ..Default::default() };
        for (k, v) in key_values(&a.open_options)? {
            if !options.csv.set(k, v)? && !options.tiles.set(k, v)? {
                return Err(terrio::Error::InvalidArgument(format!("unknown open option '{k}'")));
            }
        }
        if terrio::tiles::is_archive(&a.input) {
            terrio::tiles::open_dataset(&a.input, &options)?
        } else {
            Dataset::Vector(terrio::open_vector(&a.input, &options)?)
        }
    };
    let t_srs = a.t_srs.as_deref().map(SpatialRef::parse).transpose()?;
    match ds {
        Dataset::Raster(mut r) => {
            let append = a.vector.append_only();
            if a.vector.is_set() && !append {
                return Err(terrio::Error::InvalidArgument("the vector options apply to vector inputs only".into()));
            }
            if let Some(dst) = t_srs {
                let o = terrio::ops::warp::WarpOptions { dst_srs: Some(dst), ..Default::default() };
                r = terrio::ops::warp::warp(&[&r], &o)?;
            }
            let mut creation_options: Vec<(String, String)> =
                key_values(&a.raster_options)?.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            creation_options.extend(a.minzoom.map(|z| ("MINZOOM".to_string(), z.to_string())));
            creation_options.extend(a.maxzoom.map(|z| ("MAXZOOM".to_string(), z.to_string())));
            if append {
                // Added to an existing archive of image tiles.
                if !terrio::tiles::is_archive(&a.output) || !std::path::Path::new(&a.output).exists() {
                    return Err(terrio::Error::InvalidArgument(
                        "--append adds a raster to an existing PMTiles or MBTiles archive only".into(),
                    ));
                }
                let mut o = terrio::tiles::TileWriteOptions::default();
                if let Some(c) = &a.compress {
                    o.tile_format = Some(c.to_ascii_uppercase());
                }
                for (k, v) in &creation_options {
                    if !o.set(k, v)? {
                        return Err(terrio::Error::InvalidArgument(format!("unknown creation option '{k}' for tile archives")));
                    }
                }
                return terrio::tiles::update_raster(&a.output, &r, &o);
            }
            // A format terrio does not write itself: GDAL's driver of that name (or of the
            // output's extension), when built in.
            #[cfg(feature = "gdal")]
            {
                let gdal_driver = match a.format.as_deref() {
                    Some(f) if RasterDriver::from_name(f).is_none() && terrio::gdal_backend::has_driver(f) => Some(f.to_string()),
                    None if RasterDriver::for_extension(&a.output).is_none() => terrio::gdal_backend::driver_for_path(&a.output, false),
                    _ => None,
                };
                if let Some(d) = gdal_driver {
                    return terrio::gdal_backend::write_raster(&r, &a.output, &d, &creation_options);
                }
            }
            let driver = a
                .format
                .as_deref()
                .map(|f| RasterDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown raster format '{f}'"))))
                .transpose()?;
            let opts = RasterWriteOptions {
                driver,
                compression: a.compress,
                tile_size: a.tile_size,
                bigtiff: a.bigtiff,
                creation_options,
                ..Default::default()
            };
            terrio::write_raster_dataset(&r, &a.output, &opts)
        }
        Dataset::Vector(v) => {
            let layers = terrio::ops::vector_filter::translate_owned(v.layers, &a.vector.to_options(t_srs)?)?;
            #[cfg(feature = "gdal")]
            {
                let gdal_driver = match a.format.as_deref() {
                    Some(f) if VectorDriver::from_name(f).is_none() && terrio::gdal_backend::has_driver(f) => Some(f.to_string()),
                    None if VectorDriver::for_extension(&a.output).is_none() => terrio::gdal_backend::driver_for_path(&a.output, true),
                    _ => None,
                };
                if let Some(d) = gdal_driver {
                    if a.vector.write_mode() == terrio::ops::vector_filter::WriteMode::Append {
                        return Err(terrio::Error::Unsupported(format!("--append is not supported with GDAL's {d} driver")));
                    }
                    if a.minzoom.is_some() || a.maxzoom.is_some() {
                        return Err(terrio::Error::InvalidArgument(format!(
                            "--minzoom and --maxzoom apply to tile archives, not to GDAL's {d} driver"
                        )));
                    }
                    let pairs = |o: &[String]| -> terrio::Result<Vec<(String, String)>> {
                        Ok(key_values(o)?.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
                    };
                    let (dsco, lco) = (pairs(&a.dataset_options)?, pairs(&a.creation_options)?);
                    return terrio::gdal_backend::write_vector(&layers, &a.output, &d, &dsco, &lco);
                }
            }
            if !a.dataset_options.is_empty() {
                return Err(terrio::Error::InvalidArgument("--dsco applies to GDAL drivers (builds with the GDAL backend)".into()));
            }
            let driver = a
                .format
                .as_deref()
                .map(|f| VectorDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown vector format '{f}'"))))
                .transpose()?;
            let mut options = VectorWriteOptions { driver, ..Default::default() };
            options.tiles.min_zoom = a.minzoom;
            options.tiles.max_zoom = a.maxzoom;
            let tiles_out = matches!(
                driver.or_else(|| VectorDriver::for_extension(&a.output)),
                Some(VectorDriver::MBTiles | VectorDriver::PMTiles | VectorDriver::MVT)
            );
            for (k, v) in key_values(&a.creation_options)? {
                let known = if tiles_out { options.tiles.set(k, v)? } else { options.csv.set(k, v)? || options.geojson.set(k, v)? };
                if !known {
                    return Err(terrio::Error::InvalidArgument(format!("unknown layer creation option '{k}'")));
                }
            }
            terrio::ops::vector_filter::write_vector_with_mode(&layers, &a.output, &options, a.vector.write_mode())
        }
    }
}
