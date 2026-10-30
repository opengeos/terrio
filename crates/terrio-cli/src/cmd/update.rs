// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_raster_update.rst
// No code is translated from these sources.

//! `terrio update-raster`: write a raster into an existing GeoTIFF, reprojected to its grid (GDAL's
//! `gdal raster update`).

use terrio::ops::Resampling;
use terrio::ops::update::{UpdateOptions, parse_geometry, update_raster};

/// Write a raster into an existing GeoTIFF, reprojected to its grid: the source's valid pixels
/// replace the destination's where it overlaps (optionally within a geometry), and the
/// destination's overviews are refreshed there (like gdal raster update).
///
/// Only the changed blocks are rewritten, through a copy renamed over the file: an error before replacement leaves
/// it as it was.
#[derive(clap::Args)]
pub struct Args {
    /// The source raster.
    input: String,
    /// The destination GeoTIFF, updated in place.
    output: String,
    /// The resampling: nearest, bilinear, cubic, cubicspline, lanczos, average, rms, mode, min,
    /// max, med, q1, q3, sum.
    #[arg(short = 'r', long, default_value = "nearest")]
    resampling: String,
    /// Write only within this geometry: WKT or GeoJSON.
    #[arg(long)]
    geometry: Option<String>,
    /// The CRS of --geometry (by default the destination's).
    #[arg(long, requires = "geometry")]
    geometry_crs: Option<String>,
    /// Leave the destination's overviews as they are.
    #[arg(long)]
    no_update_overviews: bool,
    /// The approximate transformer's error threshold in pixels (0 for exact transforms).
    #[arg(long = "error-threshold", alias = "et")]
    error_threshold: Option<f64>,
    /// Update a Cloud Optimized GeoTIFF even though that breaks its layout.
    #[arg(long)]
    ignore_cog_layout_break: bool,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let geometry = a.geometry.as_deref().map(|g| parse_geometry(g, super::rasterout::srs(&a.geometry_crs)?)).transpose()?;
    let o = UpdateOptions {
        resampling: Resampling::parse(&a.resampling)?,
        geometry,
        no_update_overviews: a.no_update_overviews,
        error_threshold: a.error_threshold,
        ignore_cog_layout_break: a.ignore_cog_layout_break,
    };
    let mut src = super::rasterout::open_lazy(&a.input)?;
    match super::quiet(|| update_raster(&mut src, &a.output, &o))? {
        Some(w) => println!("updated {}x{} pixels at ({}, {})", w.width, w.height, w.x, w.y),
        None => println!("nothing changed: the source does not overlap the destination or has no valid pixels in it"),
    }
    Ok(())
}
