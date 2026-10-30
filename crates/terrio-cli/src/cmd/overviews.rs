// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdaladdo.rst
//     doc/source/programs/gdal_raster_overview_add.rst
// No code is translated from these sources.

//! `terrio overviews`: build, delete or refresh the overviews of a GeoTIFF (GDAL's `gdaladdo` and
//! `gdal raster overview add/delete/refresh`).

use super::rasterout::parse_bbox;
use terrio::ops::Resampling;
use terrio::ops::overview::{
    DeleteOptions, OverviewOptions, RefreshOptions, add_overviews, delete_overviews, like_bounds, refresh_overviews,
};

/// Build overviews of a GeoTIFF in place, or in its `.ovr` sidecar (like gdaladdo and `gdal raster
/// overview add`); with `--clean`, delete them (`gdal raster overview delete`); with `--refresh`,
/// recompute them from the full-resolution image (`gdal raster overview refresh`).
///
/// The file is updated through a copy renamed over it: an error leaves it as it was. Only local,
/// writable little-endian GeoTIFFs are updated.
#[derive(clap::Args)]
pub struct Args {
    /// The GeoTIFF.
    input: String,
    /// Decimation factors, as gdaladdo takes them (`2 4 8 16`). Without them (or `--levels`),
    /// factors of 2, 4, 8... until the overview fits in `--min-size` pixels.
    factors: Vec<usize>,
    /// Decimation factors, comma-separated (`2,4,8`), as `gdal raster overview` takes them.
    #[arg(long, value_delimiter = ',')]
    levels: Vec<usize>,
    /// The resampling: nearest, average, min, max or mode. Refreshing defaults to each overview's
    /// own method (its RESAMPLING metadata item).
    #[arg(short = 'r', long)]
    resampling: Option<String>,
    /// Automatic levels stop at the first overview that fits in this many pixels.
    #[arg(long, default_value_t = 256)]
    min_size: usize,
    /// Use the external `.ovr` sidecar (gdaladdo's `-ro`) instead of internal overviews.
    #[arg(long, alias = "ro")]
    external: bool,
    /// The overviews' compression (COMPRESS_OVERVIEW): by default the image's for internal
    /// overviews, NONE for external ones.
    #[arg(long)]
    compress: Option<String>,
    /// Delete the overviews (gdaladdo's `-clean`).
    #[arg(long, alias = "delete", conflicts_with_all = ["refresh", "factors", "levels", "resampling", "compress"])]
    clean: bool,
    /// Recompute existing overviews instead of building new ones.
    #[arg(long, conflicts_with = "compress")]
    refresh: bool,
    /// With `--refresh`, recompute only the overviews over this extent: `xmin,ymin,xmax,ymax` in
    /// the raster's CRS (repeatable).
    #[arg(long, allow_hyphen_values = true, requires = "refresh", conflicts_with = "like")]
    bbox: Vec<String>,
    /// With `--refresh`, recompute only the overviews over the extent of these rasters (sources
    /// that changed), repeatable.
    #[arg(long, requires = "refresh")]
    like: Vec<String>,
    /// Update a Cloud Optimized GeoTIFF even though that breaks its layout (GDAL's
    /// IGNORE_COG_LAYOUT_BREAK=YES open option): it stays a valid GeoTIFF.
    #[arg(long)]
    ignore_cog_layout_break: bool,
}

pub fn run(a: Args) -> terrio::Result<()> {
    if a.clean {
        return delete_overviews(&a.input, &DeleteOptions { external: a.external, ignore_cog_layout_break: a.ignore_cog_layout_break });
    }
    let mut levels = a.factors.clone();
    levels.extend(&a.levels);
    let resampling = a.resampling.as_deref().map(Resampling::parse).transpose()?;
    if a.refresh {
        let mut bbox: Vec<[f64; 4]> = a.bbox.iter().map(|b| parse_bbox(b)).collect::<terrio::Result<_>>()?;
        bbox.extend(super::quiet(|| like_bounds(&a.input, &a.like))?);
        let o = RefreshOptions { resampling, levels, bbox, external: a.external, ignore_cog_layout_break: a.ignore_cog_layout_break };
        let sizes = super::quiet(|| refresh_overviews(&a.input, &o))?;
        println!("refreshed {}", sizes_text(&sizes));
        return Ok(());
    }
    let o = OverviewOptions {
        levels,
        min_size: a.min_size,
        resampling: resampling.unwrap_or_default(),
        external: a.external,
        compression: a.compress.clone(),
        ignore_cog_layout_break: a.ignore_cog_layout_break,
    };
    let sizes = super::quiet(|| add_overviews(&a.input, &o))?;
    println!("overviews {}", sizes_text(&sizes));
    Ok(())
}

fn sizes_text(sizes: &[(usize, usize)]) -> String {
    if sizes.is_empty() {
        return "none".into();
    }
    sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect::<Vec<_>>().join(", ")
}
