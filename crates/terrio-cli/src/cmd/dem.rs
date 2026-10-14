//! `terrio dem`.

use super::rasterout::{OutputArgs, open};
use terrio::Error;
use terrio::ops::dem::{ColorMap, ColorSelection, DemOptions, DemProduct, GradientAlg, Shading, TriAlg, color_relief, dem, pam_statistics};

#[derive(clap::Args)]
pub struct Args {
    /// The processing: hillshade, slope, aspect, color-relief, TRI, TPI or roughness.
    mode: String,
    /// The input DEM, then (for color-relief) the color file, then the output path.
    #[arg(required = true, num_args = 2..=3, value_names = ["INPUT", "OUTPUT"])]
    paths: Vec<String>,
    /// The band to process, from 1.
    #[arg(short = 'b', long = "band", default_value_t = 1)]
    band: usize,
    /// Compute values at the edges of the raster and next to nodata too.
    #[arg(long = "compute-edges", alias = "compute_edges")]
    compute_edges: bool,
    /// The algorithm: Horn or ZevenbergenThorne (hillshade, slope, aspect), Riley or Wilson
    /// (TRI).
    #[arg(long)]
    alg: Option<String>,
    /// Vertical exaggeration (hillshade).
    #[arg(short = 'z', long = "z-factor", default_value_t = 1.0)]
    z: f64,
    /// Ratio of vertical to horizontal units (hillshade, slope); 111120 for a geographic DEM in
    /// metres. By default derived from the CRS.
    #[arg(short = 's', long = "scale")]
    scale: Option<f64>,
    /// Ratio of vertical to horizontal units along x (with --yscale).
    #[arg(long)]
    xscale: Option<f64>,
    /// Ratio of vertical to horizontal units along y (with --xscale).
    #[arg(long)]
    yscale: Option<f64>,
    /// Azimuth of the light, in degrees (hillshade).
    #[arg(long = "az", alias = "azimuth", allow_negative_numbers = true)]
    azimuth: Option<f64>,
    /// Altitude of the light, in degrees (hillshade).
    #[arg(long = "alt", alias = "altitude")]
    altitude: Option<f64>,
    /// Combined shading: hillshade and slope (hillshade).
    #[arg(long, conflicts_with_all = ["multidirectional", "igor"])]
    combined: bool,
    /// Multidirectional shading (hillshade).
    #[arg(long, conflicts_with_all = ["igor", "azimuth"])]
    multidirectional: bool,
    /// Igor's shading, which only darkens slopes facing away from the light (hillshade).
    #[arg(long, conflicts_with = "altitude")]
    igor: bool,
    /// Slope as a percentage rather than in degrees (slope).
    #[arg(short = 'p', long = "percent")]
    percent: bool,
    /// Aspect as a trigonometric angle rather than an azimuth (aspect).
    #[arg(long)]
    trigonometric: bool,
    /// Aspect of 0 rather than nodata for flat areas (aspect).
    #[arg(long = "zero-for-flat", alias = "zero_for_flat")]
    zero_for_flat: bool,
    /// Add an alpha band (color-relief).
    #[arg(long)]
    alpha: bool,
    /// Only color values that have an entry in the color file (color-relief).
    #[arg(long = "exact-color-entry", alias = "exact_color_entry", conflicts_with = "nearest_color_entry")]
    exact_color_entry: bool,
    /// Use the color of the nearest entry rather than interpolating (color-relief).
    #[arg(long = "nearest-color-entry", alias = "nearest_color_entry")]
    nearest_color_entry: bool,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let mode = a.mode.to_ascii_lowercase();
    let color = mode == "color-relief";
    let (input, colors, output) = match (color, a.paths.as_slice()) {
        (true, [i, c, o]) => (i, Some(c), o),
        (false, [i, o]) => (i, None, o),
        (true, _) => return Err(Error::InvalidArgument("color-relief takes an input DEM, a color file and an output".into())),
        (false, _) => return Err(Error::InvalidArgument(format!("{} takes an input DEM and an output", a.mode))),
    };
    let product = if color { None } else { Some(DemProduct::parse(&mode)?) };
    let hillshade = product == Some(DemProduct::Hillshade);
    if (a.combined || a.multidirectional || a.igor || a.azimuth.is_some() || a.altitude.is_some()) && !hillshade {
        return Err(Error::InvalidArgument("--combined, --multidirectional, --igor, --az and --alt apply to hillshade".into()));
    }
    let mut o = DemOptions {
        band: a.band,
        z: a.z,
        scale: a.scale,
        xscale: a.xscale,
        yscale: a.yscale,
        azimuth: a.azimuth.unwrap_or(315.0),
        altitude: a.altitude.unwrap_or(45.0),
        shading: if a.combined {
            Shading::Combined
        } else if a.multidirectional {
            Shading::MultiDirectional
        } else if a.igor {
            Shading::Igor
        } else {
            Shading::Standard
        },
        slope_percent: a.percent,
        trigonometric: a.trigonometric,
        zero_for_flat: a.zero_for_flat,
        compute_edges: a.compute_edges,
        alpha: a.alpha,
        color_selection: if a.exact_color_entry {
            ColorSelection::Exact
        } else if a.nearest_color_entry {
            ColorSelection::Nearest
        } else {
            ColorSelection::Interpolate
        },
        ..Default::default()
    };
    if let Some(alg) = &a.alg {
        match product {
            Some(DemProduct::Hillshade | DemProduct::Slope | DemProduct::Aspect) => o.gradient = GradientAlg::parse(alg)?,
            Some(DemProduct::Tri) => o.tri = TriAlg::parse(alg)?,
            _ => return Err(Error::InvalidArgument("--alg applies to hillshade, slope, aspect and TRI".into())),
        }
    }
    let ds = open(input)?;
    let out = match colors {
        Some(path) => {
            let text =
                std::fs::read_to_string(path).map_err(|e| Error::InvalidArgument(format!("cannot read the color file '{path}': {e}")))?;
            color_relief(&ds, &ColorMap::parse(&text, &ds, o.band, pam_statistics(input, o.band))?, &o)?
        }
        None => dem(&ds, product.expect("not color-relief"), &o)?,
    };
    a.out.write(&out, output)
}
