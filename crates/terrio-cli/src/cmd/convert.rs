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
    /// GeoTIFF/COG compression: NONE, LZW, DEFLATE, PACKBITS, JPEG, WEBP.
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
    } else if a.layers.is_empty() {
        super::quiet(|| terrio::open(&a.input))?
    } else {
        Dataset::Vector(terrio::open_vector(&a.input, &VectorOpenOptions { layers: a.layers.clone() })?)
    };
    let t_srs = a.t_srs.as_deref().map(SpatialRef::parse).transpose()?;
    match ds {
        Dataset::Raster(r) => {
            let driver = a
                .format
                .as_deref()
                .map(|f| RasterDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown raster format '{f}'"))))
                .transpose()?;
            let mut raster = r.raster;
            if let Some(dst) = t_srs {
                let code = dst.epsg.ok_or_else(|| terrio::Error::Crs("raster reprojection needs an EPSG code".into()))?;
                raster = raster.reproject_to_epsg(code, terrio::raster::ResampleMethod::Nearest)?;
            }
            let opts = RasterWriteOptions { driver, compression: a.compress, tile_size: a.tile_size, bigtiff: a.bigtiff };
            terrio::write_raster(&raster, &a.output, &opts)
        }
        Dataset::Vector(v) => {
            let driver = a
                .format
                .as_deref()
                .map(|f| VectorDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown vector format '{f}'"))))
                .transpose()?;
            let layers = match t_srs {
                Some(dst) => v.layers.iter().map(|l| terrio::vector::reproject_layer(l, &dst)).collect::<terrio::Result<Vec<_>>>()?,
                None => v.layers,
            };
            terrio::write_vector(&layers, &a.output, &VectorWriteOptions { driver })
        }
    }
}
