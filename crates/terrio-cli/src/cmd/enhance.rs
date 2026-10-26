//! `terrio enhance`.

use super::rasterout::{OutputArgs, open_lazy, same_file};
use terrio::ops::enhance::{BandLut, EnhanceOptions, enhance_to_path, equalization_luts_from_path, format_luts, linear_luts, parse_luts};

#[derive(clap::Args)]
pub struct Args {
    /// The input raster.
    input: String,
    /// The output path. Without it, the tables are written to --config (or printed).
    output: Option<String>,
    /// Compute histogram-equalization tables.
    #[arg(long)]
    equalize: bool,
    /// A LUT file, as gdalenhance writes it: read when not equalizing; written by --equalize
    /// without an output.
    #[arg(long)]
    config: Option<String>,
    /// Stretch this source range linearly over the bins, without tables (gdalenhance parses
    /// -src_scale but ignores it).
    #[arg(long = "src-scale", alias = "src_scale", num_args = 2, value_names = ["MIN", "MAX"], allow_hyphen_values = true, conflicts_with_all = ["equalize", "config"])]
    src_scale: Option<Vec<f64>>,
    /// The number of bins of the tables (256, as in gdalenhance).
    #[arg(long = "lut-bins", alias = "lutbins", default_value_t = 256)]
    lut_bins: usize,
    /// The output data type (Byte by default; gdalenhance writes only Byte).
    #[arg(long = "ot", default_value = "Byte")]
    ot: String,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    if a.output.is_none() && !a.equalize {
        return Err(terrio::Error::InvalidArgument("give an output, or --equalize to write lookup tables".into()));
    }
    if a.output.is_none() && a.config.as_ref().is_some_and(|config| same_file(&a.input, config)) {
        return Err(terrio::Error::InvalidArgument("the config output must not be the input".into()));
    }
    if a.output.is_some() && a.equalize && a.config.is_some() {
        return Err(terrio::Error::InvalidArgument("--config with --equalize writes tables and requires no raster output".into()));
    }
    let o = EnhanceOptions { bins: a.lut_bins, output_type: terrio::ops::parse_data_type(&a.ot)? };
    let mut src = open_lazy(&a.input)?;
    let luts: Vec<BandLut> = if a.equalize {
        super::quiet(|| equalization_luts_from_path(&a.input, o.bins))?
    } else if let Some(config) = &a.config {
        parse_luts(&std::fs::read_to_string(config)?, src.band_count(), o.bins)?
    } else if let Some(s) = &a.src_scale {
        linear_luts(src.band_count(), s[0], s[1])?
    } else {
        return Err(terrio::Error::InvalidArgument("give --equalize, --config FILE or --src-scale MIN MAX".into()));
    };
    let Some(output) = a.output else {
        let text = format_luts(&luts);
        return match &a.config {
            Some(config) => Ok(std::fs::write(config, text)?),
            None => {
                print!("{text}");
                Ok(())
            }
        };
    };
    if same_file(&a.input, &output) {
        return Err(terrio::Error::InvalidArgument("the output must not be the input".into()));
    }
    super::quiet(|| enhance_to_path(&mut src, &luts, &o, &output, &a.out.options()?))
}
