//! `terrio translate`.

use super::rasterout::{OutputArgs, open, srs};
use terrio::ops::translate::{Scale, Size, TranslateOptions, translate};
use terrio::ops::{Resampling, parse_data_type};

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
    // A window inside a GeoTIFF is read alone: only the tiles it touches are decoded.
    let mut src_win = a.srcwin.as_ref().map(|v| (v[0], v[1], v[2], v[3]));
    let ds = match src_win {
        Some((x, y, w, h)) if x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0 && [x, y, w, h].iter().all(|v| v.fract() == 0.0) => {
            let mut lazy = super::quiet(|| terrio::open_raster_lazy(&a.input))?;
            let window = terrio::Window::new(x as usize, y as usize, w as usize, h as usize);
            if lazy.is_lazy() && window.fits(lazy.width(), lazy.height()) {
                src_win = None;
                lazy.read_window(window, &Default::default())?
            } else {
                open(&a.input)?
            }
        }
        _ => open(&a.input)?,
    };
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
        src_win,
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
    let out = translate(&ds, &o)?;
    a.out.write(&out, &a.output)
}
