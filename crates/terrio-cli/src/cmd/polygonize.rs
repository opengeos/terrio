// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_polygonize.rst
// No code is translated from these sources.

//! `terrio polygonize`.

use terrio::formats::VectorDriver;
use terrio::ops::polygonize::PolygonizeOptions;
use terrio::ops::vector_filter::WriteMode;

#[derive(clap::Args)]
pub struct Args {
    /// The raster.
    input: String,
    /// The output vector dataset.
    output: String,
    /// The band to polygonize (1-based).
    #[arg(short, long, default_value_t = 1)]
    band: usize,
    /// The name of the field receiving the pixel values.
    #[arg(long = "field", visible_alias = "fieldname", value_name = "NAME", default_value = "DN")]
    field: String,
    /// The output layer name. By default, the output file's name.
    #[arg(long, value_name = "NAME")]
    nln: Option<String>,
    /// Connect pixels through their corners too (8-connectedness). The default is 4.
    #[arg(long = "8")]
    eight: bool,
    /// Do not leave out nodata pixels.
    #[arg(long, conflicts_with = "mask")]
    nomask: bool,
    /// Leave out the pixels where the first band of this raster is 0.
    #[arg(long, value_name = "RASTER")]
    mask: Option<String>,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format")]
    format: Option<String>,
    /// Add the polygons to the layer of the same name in an existing output.
    #[arg(long, conflicts_with = "overwrite")]
    append: bool,
    /// Replace the layer of the same name in an existing output (other GeoPackage layers stay).
    #[arg(long)]
    overwrite: bool,
}

pub fn run(a: Args) -> terrio::Result<()> {
    if a.band == 0 {
        return Err(terrio::Error::InvalidArgument("bands are numbered from 1".into()));
    }
    let ds = super::quiet(|| terrio::open_raster(&a.input))?;
    let mask = match &a.mask {
        Some(m) => Some(super::quiet(|| terrio::open_raster(m))?),
        None => None,
    };
    let stem = std::path::Path::new(&a.output).file_stem().and_then(|s| s.to_str()).unwrap_or("polygonized").to_string();
    let options = PolygonizeOptions {
        band: a.band - 1,
        field_name: a.field,
        layer_name: a.nln.unwrap_or(stem),
        connectedness8: a.eight,
        ignore_nodata: a.nomask,
        mask,
    };
    let layer = terrio::ops::polygonize::polygonize(&ds, &options)?;
    let driver = a
        .format
        .as_deref()
        .map(|f| VectorDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown vector format '{f}'"))))
        .transpose()?;
    let mode = if a.append {
        WriteMode::Append
    } else if a.overwrite {
        WriteMode::Overwrite
    } else {
        WriteMode::Create
    };
    terrio::ops::vector_filter::write_vector_with_mode(
        &[layer],
        &a.output,
        &terrio::VectorWriteOptions { driver, ..Default::default() },
        mode,
    )
}
