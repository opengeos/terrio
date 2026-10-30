// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_pansharpen.rst
// No code is translated from these sources.

//! `terrio pansharpen`.
use super::rasterout::{OutputArgs, open_lazy};
use terrio::Error;
use terrio::ops::pansharpen::{PansharpenExtent, PansharpenNodata, PansharpenOptions};
use terrio::ops::{Resampling, parse_data_type};

#[derive(clap::Args)]
pub struct Args {
    /// Panchromatic input, spectral inputs (optionally PATH,band=N), then output.
    #[arg(required = true, num_args = 3..)]
    paths: Vec<String>,
    /// Weight per spectral input band; repeat for all bands. Defaults to equal weights.
    #[arg(short = 'w', long = "weight", allow_negative_numbers = true)]
    weights: Vec<f64>,
    /// Spectral inputs to output, from 1; repeat to select/reorder. All contribute intensity.
    #[arg(short = 'b', long = "band")]
    bands: Vec<usize>,
    /// Panchromatic band, from 1.
    #[arg(long, default_value_t = 1)]
    pan_band: usize,
    /// Spectral upsampling: nearest, bilinear, cubic, cubicspline, lanczos or average.
    #[arg(short = 'r', long, default_value = "cubic")]
    resampling: String,
    /// Clamp unsigned samples to this bit depth (1..31, fitting the input type).
    #[arg(long)]
    bitdepth: Option<u8>,
    /// Common nodata, or none to disable. Default: infer from the selected pan band.
    #[arg(long, allow_hyphen_values = true)]
    nodata: Option<String>,
    /// How unequal footprints are handled: union, intersection or none.
    #[arg(long, default_value = "union")]
    spat_adjust: String,
    /// Output data type; defaults to the first selected spectral band's type.
    #[arg(long)]
    ot: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let input = &a.paths[0];
    let output = a.paths.last().unwrap();
    let mut spectral = Vec::new();
    for path in &a.paths[1..a.paths.len() - 1] {
        let (path, band) = match path.rsplit_once(",band=") {
            Some((p, b)) => (p, Some(b.parse::<usize>().map_err(|_| Error::InvalidArgument("spectral band must be an integer".into()))?)),
            None => (path.as_str(), None),
        };
        let ds = open_lazy(path)?;
        for band in band.map(|b| vec![b]).unwrap_or_else(|| (1..=ds.band_count()).collect()) {
            spectral.push((path.to_string(), band));
        }
    }
    let nodata = match a.nodata.as_deref() {
        None => PansharpenNodata::Auto,
        Some(s) if s.eq_ignore_ascii_case("none") => PansharpenNodata::None,
        Some(s) => PansharpenNodata::Value(s.parse().map_err(|_| Error::InvalidArgument("nodata must be a number or none".into()))?),
    };
    let o = PansharpenOptions {
        pan_band: a.pan_band,
        weights: a.weights,
        output_bands: a.bands,
        resampling: Resampling::parse(&a.resampling)?,
        bit_depth: a.bitdepth,
        nodata,
        extent: PansharpenExtent::parse(&a.spat_adjust)?,
        output_type: a.ot.as_deref().map(parse_data_type).transpose()?,
    };
    if a.out.format.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("VRT"))
        || (a.out.format.is_none() && std::path::Path::new(output).extension().is_some_and(|v| v.eq_ignore_ascii_case("vrt")))
    {
        return terrio::ops::pansharpen::vrt::PansharpenVrt::from_paths(input, &spectral, &o)?.write(output);
    }
    terrio::ops::pansharpen::vrt::pansharpen_to_path(input, &spectral, &o, output, &a.out.options()?)
}
