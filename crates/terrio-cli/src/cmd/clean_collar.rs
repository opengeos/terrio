//! `terrio clean-collar` and `terrio nearblack`.

use super::rasterout::{OutputArgs, open_lazy, same_file};
use terrio::ops::nearblack::{CollarAlgorithm, NearblackOptions, color_bands, keep_alpha_or_mask, nearblack, nearblack_file, parse_color};

#[derive(clap::Args)]
pub struct Args {
    /// The input raster (normally of Byte bands).
    input: String,
    /// The output path (a new Byte raster). Without it, --update updates the input in place.
    output: Option<String>,
    /// Update the input in place (a local GeoTIFF), rather than writing a new raster.
    #[arg(long, conflicts_with = "output")]
    update: bool,
    /// A collar color: black, white, or a value per non-alpha band (r,g,b). Repeatable.
    #[arg(long = "color", allow_hyphen_values = true)]
    colors: Vec<String>,
    /// Search for nearly white pixels, set to 255 (nearblack's -white).
    #[arg(long, conflicts_with = "colors")]
    white: bool,
    /// How far from the colors values can be and still be part of the collar.
    #[arg(long = "color-threshold", alias = "near", default_value_t = 15)]
    color_threshold: i32,
    /// How many consecutive non-collar pixels the search inwards goes past.
    #[arg(long = "pixel-distance", alias = "nb", alias = "nb-non-black", default_value_t = 2)]
    pixel_distance: i32,
    /// Add an alpha band, transparent in the collar (or update the input's last alpha band).
    #[arg(long = "add-alpha", alias = "setalpha", conflicts_with = "add_mask")]
    add_alpha: bool,
    /// Add a mask, invalid in the collar.
    #[arg(long = "add-mask", alias = "setmask")]
    add_mask: bool,
    /// The algorithm: floodfill (the clean-collar default) or twopasses (the nearblack default).
    #[arg(long, alias = "alg")]
    algorithm: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

/// Runs `clean-collar` (GDAL's `gdal raster clean-collar` defaults: flood fill, and an alpha band
/// or mask when the input has one) or `nearblack` (`nearblack`'s: two passes).
pub fn run(a: Args, clean_collar: bool) -> terrio::Result<()> {
    if a.color_threshold < 0 || a.pixel_distance < 0 {
        return Err(terrio::Error::InvalidArgument("--color-threshold and --pixel-distance must not be negative".into()));
    }
    let algorithm = match &a.algorithm {
        Some(s) => CollarAlgorithm::parse(s)?,
        None if clean_collar => CollarAlgorithm::FloodFill,
        None => CollarAlgorithm::TwoPasses,
    };
    let path = match (&a.output, a.update) {
        (Some(o), _) => o.clone(),
        (None, true) => a.input.clone(),
        (None, false) => return Err(terrio::Error::InvalidArgument("give an output, or --update to update the input in place".into())),
    };
    let in_place = a.update || same_file(&a.input, &path);
    let mut lazy = open_lazy(&a.input)?;
    let header = lazy.header();
    let bands = color_bands(&header, a.add_alpha || a.add_mask || clean_collar);
    let colors = a.colors.iter().map(|c| parse_color(c, bands)).collect::<terrio::Result<Vec<_>>>()?;
    let mut o = NearblackOptions {
        colors,
        white: a.white,
        near: a.color_threshold,
        max_non_black: a.pixel_distance,
        set_alpha: a.add_alpha,
        set_mask: a.add_mask,
        algorithm,
    };
    // gdal raster clean-collar keeps the input's alpha band or mask.
    if clean_collar {
        keep_alpha_or_mask(&mut o, &header, lazy.has_mask());
    }
    if in_place {
        drop(lazy);
        return super::quiet(|| nearblack_file(&a.input, &o, &a.out.options()?));
    }
    let ds = lazy.read()?;
    let collar = nearblack(&ds, &o)?;
    let mut options = a.out.options()?;
    options.mask = collar.mask;
    super::quiet(|| terrio::write_raster_dataset(&collar.dataset, &path, &options))
}
