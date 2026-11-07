// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_translate.rst
// No code is translated from these sources.

//! `terrio translate`.

use super::rasterout::{OutputArgs, open, srs};
use std::ffi::OsString;
use terrio::ops::overview::OverviewSelection;
use terrio::ops::translate::{Scale, Size, TranslateOptions, translate, translate_lazy_to_file};
use terrio::ops::{Resampling, parse_data_type};

/// Rewrites `terrio translate ... --scale [s0 s1] <input> <output>` so that positional
/// `<input>` and `<output>` arguments following a 0- or 2-argument `--scale` are not
/// consumed by clap as scale numbers.
pub fn rewrite_args(args: Vec<OsString>) -> Vec<OsString> {
    if args.iter().skip(1).find_map(|a| a.to_str().filter(|s| !s.starts_with('-'))) != Some("translate") {
        return args;
    }
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let s = args[i].to_str();
        if s == Some("--") {
            out.extend_from_slice(&args[i..]);
            break;
        }
        if s == Some("--scale") {
            let mut k = 0usize;
            while k < 4 && i + 1 + k < args.len() && args[i + 1 + k].to_str().is_some_and(|t| t.parse::<f64>().is_ok()) {
                k += 1;
            }
            if k < 4 {
                let mut m = k;
                while i + 1 + m < args.len() && args[i + 1 + m].to_str().is_some_and(|t| !t.starts_with('-')) {
                    m += 1;
                }
                if (k == 0 || k == 2) && (1..=2).contains(&(m - k)) {
                    out.extend_from_slice(&args[i + 1 + k..i + 1 + m]);
                    out.extend_from_slice(&args[i..i + 1 + k]);
                    i += 1 + m;
                    continue;
                }
            }
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

#[derive(clap::Args)]
pub struct Args {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// A band to keep, from 1; repeat for several.
    #[arg(short = 'b', long = "band")]
    bands: Vec<usize>,
    /// The output data type (Byte, Int16, UInt16, Int32, UInt32, Float32, Float64...).
    #[arg(long)]
    ot: Option<String>,
    /// A window in pixels.
    #[arg(long, num_args = 4, value_names = ["XOFF", "YOFF", "XSIZE", "YSIZE"], allow_negative_numbers = true)]
    srcwin: Option<Vec<f64>>,
    /// A window in georeferenced coordinates (or `--projwin-srs`).
    #[arg(long, num_args = 4, value_names = ["ULX", "ULY", "LRX", "LRY"], allow_negative_numbers = true)]
    projwin: Option<Vec<f64>>,
    /// The CRS of `--projwin`.
    #[arg(long = "projwin-srs", value_name = "CRS")]
    projwin_srs: Option<String>,
    /// The output size, in pixels or as a percentage (`50%`); 0 keeps the aspect ratio.
    #[arg(long, num_args = 2, value_names = ["WIDTH", "HEIGHT"])]
    outsize: Option<Vec<String>>,
    /// The output resolution.
    #[arg(long, num_args = 2, value_names = ["XRES", "YRES"])]
    tr: Option<Vec<f64>>,
    /// Resampling when resizing: near, bilinear, cubic, cubicspline, lanczos, average, rms, mode.
    #[arg(short = 'r', long = "resampling")]
    resampling: Option<String>,
    /// Source overview, as gdal_translate -ovr: AUTO (the one GDAL's RasterIO chooses for the
    /// output size), AUTO-n, NONE, or a level numbered from 0, read alone.
    #[arg(long, alias = "ovr", default_value = "AUTO")]
    overview: String,
    /// Rescale values: `--scale` (band min/max to the type's range), `--scale SRC_MIN SRC_MAX`, or
    /// `--scale SRC_MIN SRC_MAX DST_MIN DST_MAX`.
    #[arg(long, num_args = 0..=4, allow_negative_numbers = true)]
    scale: Option<Vec<f64>>,
    /// Apply a power function when rescaling.
    #[arg(long)]
    exponent: Option<f64>,
    /// Assign a nodata value, or `none` to remove it.
    #[arg(long = "a-nodata", allow_hyphen_values = true)]
    a_nodata: Option<String>,
    /// Assign a CRS.
    #[arg(long = "a-srs", value_name = "CRS")]
    a_srs: Option<String>,
    /// Assign the bounds.
    #[arg(long = "a-ullr", num_args = 4, value_names = ["ULX", "ULY", "LRX", "LRY"], allow_negative_numbers = true)]
    a_ullr: Option<Vec<f64>>,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let overview = OverviewSelection::parse(&a.overview)?;
    let four = |v: &Option<Vec<f64>>| v.as_ref().map(|v| (v[0], v[1], v[2], v[3]));
    let scale = match a.scale.as_deref() {
        None => None,
        Some([]) => Some(Scale::default()),
        Some([s0, s1]) => Some(Scale { src: Some((*s0, *s1)), dst: None }),
        Some([s0, s1, d0, d1]) => Some(Scale { src: Some((*s0, *s1)), dst: Some((*d0, *d1)) }),
        Some(_) => return Err(terrio::Error::InvalidArgument("--scale takes 0, 2 or 4 values".into())),
    };
    let assign_nodata = match a.a_nodata.as_deref() {
        None => None,
        Some(s) if s.eq_ignore_ascii_case("none") => Some(None),
        Some(s) => Some(Some(s.parse().map_err(|_| terrio::Error::InvalidArgument(format!("invalid nodata value '{s}'")))?)),
    };
    let out_size = match &a.outsize {
        Some(v) => Some((Size::parse(&v[0])?, Size::parse(&v[1])?)),
        None => None,
    };
    let o = TranslateOptions {
        bands: a.bands,
        output_type: a.ot.as_deref().map(parse_data_type).transpose()?,
        src_win: four(&a.srcwin),
        proj_win: four(&a.projwin),
        proj_win_srs: srs(&a.projwin_srs)?,
        out_size,
        resolution: a.tr.as_ref().map(|v| (v[0], v[1])),
        resampling: a.resampling.as_deref().map(Resampling::parse).transpose()?,
        scale,
        exponent: a.exponent,
        assign_nodata,
        assign_srs: srs(&a.a_srs)?,
        assign_ullr: four(&a.a_ullr),
    };
    // GeoTIFFs, Zarr stores and other rasters read by window are read for the output alone: the
    // tiles or chunks its window touches, of its bands, from the overview it is read from. Rasters
    // named `gdal:`, and those the lazy reader cannot read (GDAL may), are read whole, as `open`
    // reads them, at full resolution. Errors GDAL's fallback does not retry either (invalid
    // arguments, network failures, missing local files, resource refusals) are kept.
    let gdal_named = a.input.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("gdal:"));
    let lazy = if gdal_named {
        None
    } else {
        match super::quiet(|| terrio::open_raster_lazy(&a.input)) {
            Ok(src) => Some(src),
            Err(e @ (terrio::Error::InvalidArgument(_) | terrio::Error::Http(_))) => return Err(e),
            Err(e) if e.is_resource_refusal() => return Err(e),
            Err(terrio::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound && !a.input.contains(':') => {
                return Err(terrio::Error::Io(e));
            }
            Err(_) => None,
        }
    };
    // A raster read by window is written as it is translated: a GeoTIFF or COG a strip at a time.
    if let Some(mut src) = lazy {
        return translate_lazy_to_file(&mut src, &o, overview, &a.output, &a.out.options()?);
    }
    if matches!(overview, OverviewSelection::Level(_) | OverviewSelection::AutoOffset(_)) {
        return Err(terrio::Error::Unsupported(format!(
            "--overview {} needs a raster read by window; {} is read whole",
            a.overview, a.input
        )));
    }
    a.out.write(&translate(&open(&a.input)?, &o)?, &a.output)
}

#[cfg(test)]
mod tests {
    use super::rewrite_args;

    #[test]
    fn rewrites_scale_before_positional_paths() {
        for (input, expected) in [
            (
                vec!["terrio", "translate", "--ot", "Byte", "--scale", "in.tif", "out.tif"],
                vec!["terrio", "translate", "--ot", "Byte", "in.tif", "out.tif", "--scale"],
            ),
            (
                vec!["terrio", "translate", "--scale", "0", "1000", "in.tif", "out.tif"],
                vec!["terrio", "translate", "in.tif", "out.tif", "--scale", "0", "1000"],
            ),
            (
                vec!["terrio", "translate", "--scale", "0", "1000", "0", "255", "in.tif", "out.tif"],
                vec!["terrio", "translate", "--scale", "0", "1000", "0", "255", "in.tif", "out.tif"],
            ),
        ] {
            let out: Vec<String> =
                rewrite_args(input.into_iter().map(Into::into).collect()).into_iter().map(|s| s.into_string().unwrap()).collect();
            assert_eq!(out, expected);
        }
    }
}
