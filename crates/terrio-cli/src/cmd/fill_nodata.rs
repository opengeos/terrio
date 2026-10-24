//! `terrio fill-nodata`.

use super::rasterout::{OutputArgs, open, open_lazy};
use terrio::ops::fill_nodata::{FillNodataOptions, Interpolation, fill_nodata, raster_mask};

#[derive(clap::Args)]
pub struct Args {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// The band to fill, from 1. The output contains only this band.
    #[arg(short = 'b', long, default_value_t = 1)]
    band: usize,
    /// Maximum search distance in pixels; 0 searches the whole image.
    #[arg(long, alias = "md", default_value_t = 100.0)]
    max_distance: f64,
    /// Interpolation: inv_dist or nearest.
    #[arg(long, alias = "interp", default_value = "inv_dist")]
    interpolation: String,
    /// Number of 3x3 smoothing passes on filled pixels.
    #[arg(long, alias = "si", default_value_t = 0)]
    smoothing_iterations: usize,
    /// Raster whose band 1 is zero at holes and nonzero at valid donor pixels.
    #[arg(long)]
    mask: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let mut input = open_lazy(&a.input)?;
    let (w, h) = (input.width(), input.height());
    let mask = match a.mask {
        Some(path) => Some(raster_mask(&open(&path)?, w, h)?),
        None => input.read_mask(terrio::Window::new(0, 0, w, h), None)?,
    };
    let ds = input.read()?;
    let options = FillNodataOptions {
        band: a.band,
        mask: mask.as_deref(),
        max_distance: a.max_distance,
        interpolation: Interpolation::parse(&a.interpolation)?,
        smoothing_iterations: a.smoothing_iterations,
    };
    a.out.write(&fill_nodata(&ds, &options)?, &a.output)
}
