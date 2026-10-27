//! `terrio unscale`, `terrio nodata-to-alpha`, `terrio reclassify` and `terrio as-features`.

use super::rasterout::{OutputArgs, open};
use terrio::formats::VectorDriver;
use terrio::ops::as_features::{AsFeaturesOptions, PixelGeometry};
use terrio::ops::parse_data_type;
use terrio::ops::pixel::{NodataToAlphaOptions, ReclassifyOptions, UnscaleOptions, read_mapping_file};
use terrio::ops::vector_filter::WriteMode;

/// `terrio unscale`.
#[derive(clap::Args)]
pub struct UnscaleArgs {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// The output data type (Byte, Int16, UInt16, Int32, Float32, Float64...). By default
    /// Float32, or Float64 for Float64 inputs.
    #[arg(long = "ot", visible_alias = "datatype")]
    ot: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

/// Runs `terrio unscale`.
pub fn unscale(a: UnscaleArgs) -> terrio::Result<()> {
    let ds = open(&a.input)?;
    let o = UnscaleOptions { output_type: a.ot.as_deref().map(parse_data_type).transpose()? };
    a.out.write(&terrio::ops::pixel::unscale(&ds, &o)?, &a.output)
}

/// `terrio nodata-to-alpha`.
#[derive(clap::Args)]
pub struct NodataToAlphaArgs {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// Nodata value overriding the first band's (a number, nan, inf or -inf): once for its mask, or
    /// once per band (transparent where every band has its value).
    #[arg(long, value_name = "VALUE", allow_hyphen_values = true, value_parser = parse_nodata)]
    nodata: Vec<f64>,
    #[command(flatten)]
    out: OutputArgs,
}

fn parse_nodata(s: &str) -> Result<f64, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "nan" => Ok(f64::NAN),
        "inf" | "+inf" => Ok(f64::INFINITY),
        "-inf" => Ok(f64::NEG_INFINITY),
        t => t.parse().map_err(|_| format!("'{s}' is not a number")),
    }
}

/// Runs `terrio nodata-to-alpha`.
pub fn nodata_to_alpha(a: NodataToAlphaArgs) -> terrio::Result<()> {
    let ds = open(&a.input)?;
    let o = NodataToAlphaOptions { nodata: a.nodata };
    a.out.write(&terrio::ops::pixel::nodata_to_alpha(&ds, &o)?, &a.output)
}

/// `terrio reclassify`.
#[derive(clap::Args)]
pub struct ReclassifyArgs {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// The mapping, as GDAL writes it: `from=to` entries separated by `;`. `from` is a value, an
    /// interval (`[a,b)`, `(a,b]`, `(-inf,b]`...), NO_DATA, NAN or DEFAULT; `to` is a value,
    /// NO_DATA or PASS_THROUGH. `@file` reads the entries from a file, one per line (`#` starts
    /// a comment).
    #[arg(short = 'm', long, allow_hyphen_values = true)]
    mapping: String,
    /// The output data type. By default that of the input.
    #[arg(long = "ot", visible_alias = "datatype")]
    ot: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

/// Runs `terrio reclassify`.
pub fn reclassify(a: ReclassifyArgs) -> terrio::Result<()> {
    let mapping = match a.mapping.strip_prefix('@') {
        Some(path) => read_mapping_file(path)?,
        None => a.mapping,
    };
    let ds = open(&a.input)?;
    let o = ReclassifyOptions { mapping, output_type: a.ot.as_deref().map(parse_data_type).transpose()? };
    a.out.write(&terrio::ops::pixel::reclassify(&ds, &o)?, &a.output)
}

/// `terrio as-features`.
#[derive(clap::Args)]
pub struct AsFeaturesArgs {
    /// The input raster.
    input: String,
    /// The output vector dataset.
    output: String,
    /// A band to report, from 1 (repeatable). All bands by default.
    #[arg(short = 'b', long = "band")]
    bands: Vec<usize>,
    /// The geometry of the features: none (an attribute table), point (pixel centres) or polygon
    /// (pixel squares).
    #[arg(long = "geometry-type", default_value = "none")]
    geometry_type: String,
    /// Leave out the pixels whose first reported band has its nodata value.
    #[arg(long)]
    skip_nodata: bool,
    /// Add CENTER_X and CENTER_Y fields with the pixel centres.
    #[arg(long)]
    include_xy: bool,
    /// Add ROW and COL fields.
    #[arg(long)]
    include_row_col: bool,
    /// The output layer name.
    #[arg(long = "output-layer", visible_alias = "nln", default_value = "pixels")]
    output_layer: String,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format")]
    format: Option<String>,
    /// Add the features to the layer of the same name in an existing output.
    #[arg(long, conflicts_with = "overwrite")]
    append: bool,
    /// Replace the layer of the same name in an existing output (other GeoPackage layers stay).
    #[arg(long)]
    overwrite: bool,
}

/// Runs `terrio as-features`.
pub fn as_features(a: AsFeaturesArgs) -> terrio::Result<()> {
    let ds = open(&a.input)?;
    let o = AsFeaturesOptions {
        bands: a.bands,
        geometry: PixelGeometry::parse(&a.geometry_type)?,
        skip_nodata: a.skip_nodata,
        include_xy: a.include_xy,
        include_row_col: a.include_row_col,
        layer_name: a.output_layer,
    };
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
    let layer = terrio::ops::as_features::as_features(&ds, &o)?;
    terrio::ops::vector_filter::write_vector_with_mode(
        &[layer],
        &a.output,
        &terrio::VectorWriteOptions { driver, ..Default::default() },
        mode,
    )
}
