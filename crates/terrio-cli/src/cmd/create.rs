// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_create.rst
//     doc/source/programs/gdal_raster_create.rst
// No code is translated from these sources.

//! `terrio create-raster`: create a raster from scratch or like a template (GDAL's `gdal raster create`
//! and `gdal_create`).

use super::edit::parse_nodata;
use super::rasterout::{OutputArgs, parse_bbox};
use terrio::ops::create::{CreateOptions, create_raster_file};

/// Create a raster: its size, bands, data type, CRS, extent, nodata and metadata, every band
/// filled with a burn value (else the nodata value, else 0), from scratch or like a template
/// (like gdal raster create and gdal_create).
#[derive(clap::Args)]
pub struct Args {
    /// The output raster.
    output: String,
    /// The template raster: its size, bands, data type, CRS, extent and nodata, unless given.
    #[arg(long, alias = "input")]
    like: Option<String>,
    /// The size in pixels: `WIDTH,HEIGHT`.
    #[arg(long)]
    size: Option<String>,
    /// The number of bands (default 1).
    #[arg(long)]
    band_count: Option<usize>,
    /// The data type: Byte, Int8, UInt16, Int16, UInt32, Int32, UInt64, Int64, Float32, Float64
    /// (default Byte).
    #[arg(long, alias = "datatype")]
    ot: Option<String>,
    /// The CRS.
    #[arg(long, alias = "a-srs")]
    crs: Option<String>,
    /// The extent: `xmin,ymin,xmax,ymax`.
    #[arg(long, allow_hyphen_values = true)]
    bbox: Option<String>,
    /// The nodata value of every band (a number, nan, inf, -inf, or none).
    #[arg(long, allow_hyphen_values = true)]
    nodata: Option<String>,
    /// The value of every pixel: once for every band, or once per band (repeatable).
    #[arg(long, allow_hyphen_values = true)]
    burn: Vec<f64>,
    /// A dataset metadata item `KEY=VALUE` (repeatable).
    #[arg(long, alias = "mo", value_name = "KEY=VALUE")]
    metadata: Vec<String>,
    /// Copy the template's dataset and band metadata.
    #[arg(long, requires = "like")]
    copy_metadata: bool,
    /// Build the template's overview levels (GeoTIFF outputs).
    #[arg(long, requires = "like")]
    copy_overviews: bool,
    /// Replace an existing output.
    #[arg(long)]
    overwrite: bool,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let size = a
        .size
        .as_deref()
        .map(|s| {
            let v: Vec<usize> = s
                .split(',')
                .map(|x| x.trim().parse())
                .collect::<Result<_, _>>()
                .map_err(|_| terrio::Error::InvalidArgument(format!("size '{s}' is not WIDTH,HEIGHT")))?;
            match v.as_slice() {
                [w, h] => Ok((*w, *h)),
                _ => Err(terrio::Error::InvalidArgument(format!("size '{s}' is not WIDTH,HEIGHT"))),
            }
        })
        .transpose()?;
    let nodata = a.nodata.as_deref().map(parse_nodata).transpose()?;
    let o = CreateOptions {
        size,
        band_count: a.band_count,
        data_type: a.ot.as_deref().map(terrio::ops::parse_data_type).transpose()?,
        crs: super::rasterout::srs(&a.crs)?,
        bbox: a.bbox.as_deref().map(parse_bbox).transpose()?,
        nodata: nodata.flatten(),
        unset_nodata: nodata.is_some_and(|n| n.is_none()),
        burn: a.burn.clone(),
        metadata: super::convert::key_values(&a.metadata)?.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        copy_metadata: a.copy_metadata,
        copy_overviews: a.copy_overviews,
        overwrite: a.overwrite,
    };
    super::quiet(|| create_raster_file(&a.output, a.like.as_deref(), &o, &a.out.options()?))
}
