// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdaldem.rst
// No code is translated from these sources.

//! `terrio dem`.

use super::rasterout::{OutputArgs, open, open_lazy, same_file};
use terrio::Error;
use terrio::formats::RasterDriver;
use terrio::ops::dem::{
    ColorMap, ColorSelection, DemOptions, DemProduct, GradientAlg, Shading, TriAlg, color_relief, color_relief_to_path, color_relief_vrt,
    dem, dem_to_path, pam_statistics,
};

#[derive(clap::Args)]
pub struct Args {
    /// The processing: hillshade, slope, aspect, color-relief, TRI, TPI or roughness.
    mode: String,
    /// GDAL arithmetic version: 3.12 (default) or 3.13 (slope/standard hillshade).
    /// GDAL 3.13 hillshade rounding may differ across CPUs and architectures.
    #[arg(long, default_value = "3.12")]
    kernel_version: String,
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
    /// About how much memory (MiB) the strips of rows in flight may take, which sets their
    /// height (by default, strips of about 64 MiB of input values). The output does not depend on
    /// it. Below one row of the input's blocks, each strip decodes the blocks it touches again:
    /// slower, same values. Refused for a VRT output and for an output that is also the input,
    /// which are not written in strips.
    #[arg(long = "max-memory", value_name = "MIB")]
    max_memory: Option<f64>,
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
        kernel_version: terrio::ops::warp::KernelVersion::parse(&a.kernel_version)?,
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
        max_memory_mib: a.max_memory,
        ..Default::default()
    };
    if let Some(alg) = &a.alg {
        match product {
            Some(DemProduct::Hillshade | DemProduct::Slope | DemProduct::Aspect) => o.gradient = GradientAlg::parse(alg)?,
            Some(DemProduct::Tri) => o.tri = TriAlg::parse(alg)?,
            _ => return Err(Error::InvalidArgument("--alg applies to hillshade, slope, aspect and TRI".into())),
        }
    }
    // A VRT output (`-f VRT` or a .vrt file): the color relief as lookup tables over the DEM.
    let vrt = match a.out.format.as_deref() {
        Some(f) => RasterDriver::from_name(f) == Some(RasterDriver::VRT),
        None => RasterDriver::for_extension(output) == Some(RasterDriver::VRT),
    };
    if vrt && colors.is_none() {
        return Err(Error::InvalidArgument("VRT output is only supported for color-relief".into()));
    }
    // Only outputs written a strip at a time take a memory budget, as in Python.
    if a.max_memory.is_some() && (vrt || same_file(input, output)) {
        return Err(Error::InvalidArgument(
            "--max-memory does not apply to a VRT output or to an output that is also the input, which are not written in strips".into(),
        ));
    }
    // Terrain products a strip at a time, without holding the DEM or the output whole.
    let colors = match (colors, product) {
        // The DEM read whole first when it is also the output.
        (None, Some(product)) if same_file(input, output) => return a.out.write(&dem(&open(input)?, product, &o)?, output),
        (None, Some(product)) => {
            let mut src = open_lazy(input)?;
            return super::quiet(|| dem_to_path(&mut src, product, &o, output, &a.out.options()?)).map_err(cli_flag_names);
        }
        (Some(colors), None) => colors,
        _ => unreachable!("color-relief alone takes a color file"),
    };
    let text =
        std::fs::read_to_string(colors).map_err(|e| Error::InvalidArgument(format!("cannot read the color file '{colors}': {e}")))?;
    // A color relief a strip at a time, without holding the DEM or the output whole.
    if !vrt && !same_file(input, output) {
        let mut src = open_lazy(input)?;
        let map = ColorMap::parse_lazy(&text, &mut src, o.band, pam_statistics(input, o.band), o.max_memory_mib).map_err(cli_flag_names)?;
        return super::quiet(|| color_relief_to_path(&mut src, &map, &o, output, &a.out.options()?)).map_err(cli_flag_names);
    }
    let ds = open(input)?;
    let map = ColorMap::parse(&text, &ds, o.band, pam_statistics(input, o.band))?;
    if vrt {
        return color_relief_vrt(&ds, input, &map, &o, Some(output))?.write(output);
    }
    a.out.write(&color_relief(&ds, &map, &o)?, output)
}

/// The library's refusals of a memory budget under the CLI's flag name: `same_file` above compares
/// paths, so an output that is a hard link to the input is refused by the library instead.
fn cli_flag_names(e: Error) -> Error {
    match e {
        Error::InvalidArgument(m) => Error::InvalidArgument(m.replace("max_memory_mib", "--max-memory")),
        e => e,
    }
}
