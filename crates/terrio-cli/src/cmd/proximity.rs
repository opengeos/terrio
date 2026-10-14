//! `terrio proximity`.

use super::rasterout::{OutputArgs, open};
use terrio::Error;
use terrio::ops::parse_data_type;
use terrio::ops::proximity::{ProximityOptions, parse_values, proximity};

#[derive(clap::Args)]
pub struct Args {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// The band of the input, from 1.
    #[arg(long = "srcband", alias = "band", short = 'b', default_value_t = 1)]
    band: usize,
    /// The target pixel values, comma-separated. By default, every non-zero pixel.
    #[arg(long, allow_hyphen_values = true)]
    values: Option<String>,
    /// The distance units: PIXEL (the default) or GEO (georeferenced units).
    #[arg(long, default_value = "PIXEL")]
    distunits: String,
    /// The largest distance computed; farther pixels get the --nodata value.
    #[arg(long)]
    maxdist: Option<f64>,
    /// The value of pixels farther than --maxdist or without any target (65535 by default).
    #[arg(long, allow_hyphen_values = true)]
    nodata: Option<f64>,
    /// Keep the input's nodata pixels as --nodata in the output.
    #[arg(long = "use-input-nodata", alias = "use_input_nodata")]
    use_input_nodata: bool,
    /// Give pixels within --maxdist this value instead of their distance.
    #[arg(long = "fixed-buf-val", allow_hyphen_values = true)]
    fixed_buf_val: Option<f64>,
    /// The output type (Float32 by default).
    #[arg(long, default_value = "Float32")]
    ot: String,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let geo_units = match a.distunits.to_ascii_uppercase().as_str() {
        "PIXEL" => false,
        "GEO" => true,
        other => return Err(Error::InvalidArgument(format!("unrecognized distance units '{other}' (PIXEL, GEO)"))),
    };
    let o = ProximityOptions {
        band: a.band,
        values: a.values.as_deref().map(parse_values).unwrap_or_default(),
        geo_units,
        max_distance: a.maxdist,
        nodata: a.nodata,
        use_input_nodata: a.use_input_nodata,
        fixed_value: a.fixed_buf_val,
        output_type: parse_data_type(&a.ot)?,
    };
    let ds = open(&a.input)?;
    a.out.write(&proximity(&ds, &o)?, &a.output)
}
