// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_edit.rst
//     doc/source/programs/gdalmove.rst
//     doc/source/programs/gdal_raster_edit.rst
// No code is translated from these sources.

//! `terrio edit-raster`: edit a GeoTIFF in place (GDAL's `gdal raster edit` and `gdal_edit.py`), or move
//! its georeferencing to another CRS (`gdalmove.py`).

use super::rasterout::parse_bbox;
use terrio::crs::SpatialRef;
use terrio::ops::edit::{EditOptions, MoveOptions, edit_raster, move_raster};

/// Edit a GeoTIFF in place: its CRS, geotransform or GCPs, nodata value, metadata, statistics and
/// histograms (like gdal raster edit and gdal_edit.py). With --t-srs, move its georeferencing to
/// another CRS without resampling (like gdalmove.py).
///
/// The file is updated through a copy renamed over it: an error leaves it as it was. Pixels are
/// not rewritten. Histograms go to the .aux.xml sidecar, as GDAL stores them.
#[derive(clap::Args)]
pub struct Args {
    /// The GeoTIFF.
    input: String,
    /// Assign a CRS without reprojecting (`-a_srs`); `none` removes it.
    #[arg(long, alias = "a-srs")]
    crs: Option<String>,
    /// Set a north-up geotransform from the extent `xmin,ymin,xmax,ymax`.
    #[arg(long, allow_hyphen_values = true, conflicts_with = "geotransform")]
    bbox: Option<String>,
    /// Set the geotransform: six comma-separated values (`-a_gt`).
    #[arg(long, allow_hyphen_values = true)]
    geotransform: Option<String>,
    /// Georeference by a ground control point `pixel,line,x,y[,z]` (repeatable).
    #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["bbox", "geotransform"])]
    gcp: Vec<String>,
    /// Set the nodata value of every band (a number, nan, inf, -inf); `none` removes it.
    #[arg(long, alias = "a-nodata", allow_hyphen_values = true)]
    nodata: Option<String>,
    /// Remove the nodata value.
    #[arg(long, conflicts_with = "nodata")]
    unset_nodata: bool,
    /// Add or replace a dataset metadata item `KEY=VALUE` (repeatable; `-mo`).
    #[arg(long, alias = "mo", value_name = "KEY=VALUE")]
    metadata: Vec<String>,
    /// Remove a dataset metadata item (repeatable).
    #[arg(long, value_name = "KEY")]
    unset_metadata: Vec<String>,
    /// Remove every dataset metadata item first (`-unsetmd`).
    #[arg(long)]
    unset_all_metadata: bool,
    /// Compute and store band statistics from every pixel.
    #[arg(long, conflicts_with = "approx_stats")]
    stats: bool,
    /// Compute and store approximate band statistics (an overview, or sampled blocks).
    #[arg(long)]
    approx_stats: bool,
    /// Remove the stored band statistics.
    #[arg(long, conflicts_with_all = ["stats", "approx_stats"])]
    unset_stats: bool,
    /// Compute and store each band's default histogram in the .aux.xml sidecar.
    #[arg(long)]
    hist: bool,
    /// Move the georeferencing to this CRS without resampling (gdalmove.py's `-t_srs`).
    #[arg(long, conflicts_with_all = ["crs", "bbox", "geotransform", "gcp", "nodata", "unset_nodata", "metadata", "unset_metadata", "unset_all_metadata", "stats", "approx_stats", "unset_stats", "hist"])]
    t_srs: Option<String>,
    /// With --t-srs, the raster's CRS, overriding its own.
    #[arg(long, requires = "t_srs")]
    s_srs: Option<String>,
    /// With --t-srs, update the file only when the moved georeferencing is within this many
    /// pixels of the transformed corners and centre; without it, the move is only reported.
    #[arg(long = "error-threshold", alias = "et", requires = "t_srs")]
    error_threshold: Option<f64>,
    /// Update a Cloud Optimized GeoTIFF even though that breaks its layout (GDAL's
    /// IGNORE_COG_LAYOUT_BREAK=YES): it stays a valid GeoTIFF.
    #[arg(long)]
    ignore_cog_layout_break: bool,
}

fn numbers(s: &str, n: &[usize], what: &str) -> terrio::Result<Vec<f64>> {
    let v: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| terrio::Error::InvalidArgument(format!("{what} '{s}' is not comma-separated numbers")))?;
    if !n.contains(&v.len()) {
        return Err(terrio::Error::InvalidArgument(format!("{what} '{s}' has {} values", v.len())));
    }
    Ok(v)
}

/// Parses a nodata value: a number, nan, inf or -inf; `None` for `none`.
pub fn parse_nodata(s: &str) -> terrio::Result<Option<f64>> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(None),
        "nan" => Ok(Some(f64::NAN)),
        "inf" | "+inf" => Ok(Some(f64::INFINITY)),
        "-inf" => Ok(Some(f64::NEG_INFINITY)),
        v => v.parse().map(Some).map_err(|_| terrio::Error::InvalidArgument(format!("invalid nodata value '{s}'"))),
    }
}

pub fn run(a: Args) -> terrio::Result<()> {
    if let Some(t) = &a.t_srs {
        let o = MoveOptions {
            t_srs: SpatialRef::parse(t)?,
            s_srs: a.s_srs.as_deref().map(SpatialRef::parse).transpose()?,
            error_threshold: a.error_threshold,
            ignore_cog_layout_break: a.ignore_cog_layout_break,
        };
        let r = super::quiet(|| move_raster(&a.input, &o))?;
        let names = ["Upper Left", "Lower Left", "Upper Right", "Lower Right", "Center"];
        for ((name, (x, y)), e) in names.iter().zip(&r.corners).zip(&r.errors) {
            println!("{name:<12} ({x:.3}, {y:.3})  error {e:.5} px");
        }
        let gt = r.geo_transform;
        println!("geotransform {}, {}, {}, {}, {}, {}", gt[0], gt[1], gt[2], gt[3], gt[4], gt[5]);
        match (a.error_threshold, r.updated) {
            (None, _) => println!("No error threshold given (--error-threshold): the file is not updated."),
            (Some(_), true) => println!("Updated (largest error {:.5} pixels).", r.max_error),
            (Some(e), false) => println!("Largest error {:.5} pixels exceeds the threshold {e}: the file is not updated.", r.max_error),
        }
        return Ok(());
    }
    let (crs, unset_crs) = match a.crs.as_deref() {
        Some(c) if c.eq_ignore_ascii_case("none") || c.is_empty() => (None, true),
        Some(c) => (Some(SpatialRef::parse(c)?), false),
        None => (None, false),
    };
    let (nodata, unset_nodata) = match a.nodata.as_deref().map(parse_nodata).transpose()? {
        Some(Some(v)) => (Some(v), false),
        Some(None) => (None, true),
        None => (None, a.unset_nodata),
    };
    let gcps = a
        .gcp
        .iter()
        .map(|g| {
            let v = numbers(g, &[4, 5], "GCP")?;
            Ok(terrio::Gcp { pixel: v[0], line: v[1], x: v[2], y: v[3], z: v.get(4).copied().unwrap_or(0.0), ..Default::default() })
        })
        .collect::<terrio::Result<Vec<_>>>()?;
    let metadata = super::convert::key_values(&a.metadata)?.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let o = EditOptions {
        crs,
        unset_crs,
        geo_transform: a
            .geotransform
            .as_deref()
            .map(|g| numbers(g, &[6], "geotransform").map(|v| [v[0], v[1], v[2], v[3], v[4], v[5]]))
            .transpose()?,
        bbox: a.bbox.as_deref().map(parse_bbox).transpose()?,
        gcps,
        nodata,
        unset_nodata,
        metadata,
        unset_metadata: a.unset_metadata.clone(),
        unset_all_metadata: a.unset_all_metadata,
        stats: a.stats,
        approx_stats: a.approx_stats,
        unset_stats: a.unset_stats,
        histogram: a.hist,
        ignore_cog_layout_break: a.ignore_cog_layout_break,
    };
    super::quiet(|| edit_raster(&a.input, &o))
}
