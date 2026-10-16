//! `terrio warp`.

use super::rasterout::{OutputArgs, open_lazy, srs};
use terrio::ops::gcp::GcpMethod;
use terrio::ops::warp::{Cutline, Nodata, WarpOptions, warp_to_path};
use terrio::ops::{Resampling, parse_data_type};

#[derive(clap::Args)]
pub struct Args {
    /// The input rasters, then the output path.
    #[arg(required = true, num_args = 2..)]
    paths: Vec<String>,
    /// The sources' CRS, overriding what they declare.
    #[arg(long = "s-srs", value_name = "CRS")]
    s_srs: Option<String>,
    /// The output CRS: `EPSG:4326`, WKT, a PROJ string, or a file holding one. By default, the
    /// first input's.
    #[arg(long = "t-srs", value_name = "CRS")]
    t_srs: Option<String>,
    /// The output resolution, in output CRS units.
    #[arg(long, num_args = 2, value_names = ["XRES", "YRES"])]
    tr: Option<Vec<f64>>,
    /// The output extent, in output CRS units (or `--te-srs`).
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true)]
    te: Option<Vec<f64>>,
    /// The CRS of `--te`.
    #[arg(long = "te-srs", value_name = "CRS")]
    te_srs: Option<String>,
    /// The output size in pixels; 0 computes one dimension from the other.
    #[arg(long, num_args = 2, value_names = ["WIDTH", "HEIGHT"])]
    ts: Option<Vec<usize>>,
    /// Align the output extent on multiples of `--tr`.
    #[arg(long)]
    tap: bool,
    /// Resampling: near, bilinear, cubic, cubicspline, lanczos, average, rms, mode, min, max,
    /// med, q1, q3, sum.
    #[arg(short = 'r', long = "resampling", default_value = "near")]
    resampling: String,
    /// Source nodata values (one, or one per band), or `none` to ignore the sources'.
    #[arg(long, allow_hyphen_values = true)]
    srcnodata: Option<String>,
    /// Output nodata values, or `none`. By default, the source nodata.
    #[arg(long, allow_hyphen_values = true)]
    dstnodata: Option<String>,
    /// The output data type (Byte, Int16, UInt16, Int32, UInt32, Float32, Float64...).
    #[arg(long)]
    ot: Option<String>,
    /// A vector file whose polygons mask the sources.
    #[arg(long)]
    cutline: Option<String>,
    /// Set the output extent to the cutline's.
    #[arg(long)]
    crop_to_cutline: bool,
    /// Maximum error of the approximate transformer, in pixels (0: exact).
    #[arg(long, default_value_t = 0.125)]
    et: f64,
    /// The order of the polynomial fitted to the first source's GCPs (1, 2 or 3), as
    /// `gdalwarp -order`. By default, GDAL's choice.
    #[arg(long)]
    order: Option<u32>,
    /// Fit thin plate splines to the first source's GCPs, as `gdalwarp -tps`.
    #[arg(long)]
    tps: bool,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let (output, inputs) = a.paths.split_last().expect("clap requires two paths");
    // The first input's metadata, for its GCPs.
    let first = open_lazy(&inputs[0])?.header();
    let pair = |v: &Option<Vec<f64>>| v.as_ref().map(|v| (v[0], v[1]));
    let o = WarpOptions {
        src_srs: srs(&a.s_srs)?,
        dst_srs: srs(&a.t_srs)?,
        resolution: pair(&a.tr),
        extent: a.te.as_ref().map(|v| (v[0], v[1], v[2], v[3])),
        extent_srs: srs(&a.te_srs)?,
        size: a.ts.as_ref().map(|v| (v[0], v[1])),
        target_aligned_pixels: a.tap,
        resampling: Resampling::parse(&a.resampling)?,
        src_nodata: a.srcnodata.as_deref().map(Nodata::parse).transpose()?.unwrap_or_default(),
        dst_nodata: a.dstnodata.as_deref().map(Nodata::parse).transpose()?.unwrap_or_default(),
        output_type: a.ot.as_deref().map(parse_data_type).transpose()?,
        cutline: a.cutline.as_deref().map(|p| super::quiet(|| Cutline::open(p))).transpose()?,
        crop_to_cutline: a.crop_to_cutline,
        error_threshold: a.et,
        gcps: if first.affine.is_none() && first.geo_transform() == [0.0, 1.0, 0.0, first.raster.rows as f64, 0.0, -1.0] {
            terrio::read_gcps(&inputs[0])?
        } else {
            Vec::new()
        },
        // As gdalwarp: `-order` sets the polynomial's order, but 4 or 5 GCPs still make a
        // homography.
        gcp_method: if a.tps { GcpMethod::Tps } else { GcpMethod::Auto },
        order: a.order.unwrap_or(0),
        ..Default::default()
    };
    if o.target_aligned_pixels && o.resolution.is_none() {
        return Err(terrio::Error::InvalidArgument("--tap needs --tr".into()));
    }
    super::quiet(|| warp_to_path(inputs, &o, output, &a.out.options()?))
}
