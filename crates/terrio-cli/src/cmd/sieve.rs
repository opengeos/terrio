// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_sieve.rst
// No code is translated from these sources.

//! `terrio sieve`.

use super::rasterout::{OutputArgs, open};
use terrio::ops::sieve::{SieveMask, SieveOptions, sieve};

#[derive(clap::Args)]
pub struct Args {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// The band to sieve, from 1.
    #[arg(short = 'b', long = "band", default_value_t = 1)]
    band: usize,
    /// Merge areas smaller than this many pixels.
    #[arg(long = "st", alias = "threshold", default_value_t = 2)]
    threshold: usize,
    /// Connect pixels by their sides only (the default).
    #[arg(short = '4', conflicts_with = "eight")]
    four: bool,
    /// Connect pixels by their corners too.
    #[arg(short = '8')]
    eight: bool,
    /// Sieve every pixel, ignoring the band's nodata value.
    #[arg(long, conflicts_with = "mask")]
    nomask: bool,
    /// Sieve only the pixels where band 1 of this raster is not 0.
    #[arg(long)]
    mask: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let ds = open(&a.input)?;
    let mask_ds = a.mask.as_deref().map(open).transpose()?;
    let mask = match (&mask_ds, a.nomask) {
        (Some(m), _) => SieveMask::Raster(m),
        (None, true) => SieveMask::None,
        (None, false) => SieveMask::Default,
    };
    let _ = a.four;
    let o = SieveOptions { band: a.band, threshold: a.threshold, eight_connected: a.eight, mask };
    a.out.write(&sieve(&ds, &o)?, &a.output)
}
