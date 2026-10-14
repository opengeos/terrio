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
    /// chosen from the output's extension.
    #[arg(short = 'f', long = "format")]
    format: Option<String>,
    /// Only these layers of a vector dataset.
    #[arg(short, long = "layer")]
    layers: Vec<String>,
    /// Reproject into this CRS.
    #[arg(long = "t-srs", value_name = "CRS")]
    t_srs: Option<String>,
    /// GeoTIFF/COG compression: NONE, LZW, DEFLATE, ZSTD, PACKBITS, JPEG, WEBP.
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
    /// A vector open option, as GDAL names it: `KEY=VALUE` (CSV: X_POSSIBLE_NAMES,
    /// Y_POSSIBLE_NAMES, GEOM_POSSIBLE_NAMES, AUTODETECT_TYPE, KEEP_GEOM_COLUMNS, SEPARATOR).
    #[arg(long = "oo", value_name = "KEY=VALUE")]
    open_options: Vec<String>,
    /// A vector layer creation option, as GDAL names it: `KEY=VALUE` (CSV: GEOMETRY=AS_WKT|AS_XY;
    /// GeoJSON: RFC7946=YES).
    #[arg(long = "lco", value_name = "KEY=VALUE")]
    creation_options: Vec<String>,
    #[command(flatten)]
    vector: super::vector_opts::VectorArgs,
}

/// Splits `KEY=VALUE` options.
fn key_values(options: &[String]) -> terrio::Result<Vec<(&str, &str)>> {
    options
        .iter()
        .map(|o| o.split_once('=').ok_or_else(|| terrio::Error::InvalidArgument(format!("option '{o}' is not KEY=VALUE"))))
        .collect()
}

pub fn run(a: Args) -> terrio::Result<()> {
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
    } else if a.layers.is_empty() && a.open_options.is_empty() {
        super::quiet(|| terrio::open(&a.input))?
    } else {
        let mut options = VectorOpenOptions { layers: a.layers.clone(), ..Default::default() };
        for (k, v) in key_values(&a.open_options)? {
            if !options.csv.set(k, v)? {
                return Err(terrio::Error::InvalidArgument(format!("unknown open option '{k}'")));
            }
        }
        Dataset::Vector(terrio::open_vector(&a.input, &options)?)
    };
    let t_srs = a.t_srs.as_deref().map(SpatialRef::parse).transpose()?;
    match ds {
        Dataset::Raster(mut r) => {
            if a.vector.is_set() {
                return Err(terrio::Error::InvalidArgument("the vector options apply to vector inputs only".into()));
            }
            let driver = a
                .format
                .as_deref()
                .map(|f| RasterDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown raster format '{f}'"))))
                .transpose()?;
            if let Some(dst) = t_srs {
                let o = terrio::ops::warp::WarpOptions { dst_srs: Some(dst), ..Default::default() };
                r = terrio::ops::warp::warp(&[&r], &o)?;
            }
            let opts = RasterWriteOptions { driver, compression: a.compress, tile_size: a.tile_size, bigtiff: a.bigtiff };
            terrio::write_raster_dataset(&r, &a.output, &opts)
        }
        Dataset::Vector(v) => {
            let driver = a
                .format
                .as_deref()
                .map(|f| VectorDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown vector format '{f}'"))))
                .transpose()?;
            let layers = terrio::ops::vector_filter::translate(&v.layers, &a.vector.to_options(t_srs)?)?;
            let mut options = VectorWriteOptions { driver, ..Default::default() };
            for (k, v) in key_values(&a.creation_options)? {
                if !options.csv.set(k, v)? && !options.geojson.set(k, v)? {
                    return Err(terrio::Error::InvalidArgument(format!("unknown layer creation option '{k}'")));
                }
            }
            terrio::ops::vector_filter::write_vector_with_mode(&layers, &a.output, &options, a.vector.write_mode())
        }
    }
}
