//! `terrio warp`.

use super::rasterout::{OutputArgs, srs};
use terrio::formats::RasterDriver;
use terrio::ops::gcp::GcpMethod;
use terrio::ops::rpc::{RpcOptions, read_rpc};
use terrio::ops::warp::{Cutline, Nodata, WarpOptions, warp_to_path};
use terrio::ops::{Resampling, parse_data_type};

#[derive(clap::Args)]
pub struct Args {
    /// The input rasters, then the output path. `@FILE` reads input paths from FILE, one per line.
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
    /// The output geotransform, in GDAL's order, for a rotated output grid (or any affine one):
    /// needs `--ts`, and excludes `--te`, `--tr` and `--tap`.
    #[arg(long = "dst-transform", num_args = 6, value_names = ["X0", "A", "B", "Y0", "D", "E"], allow_negative_numbers = true)]
    dst_transform: Option<Vec<f64>>,
    /// Align the output extent on multiples of `--tr`.
    #[arg(long)]
    tap: bool,
    /// Resampling: near, bilinear, cubic, cubicspline, lanczos, average, rms, mode, min, max,
    /// med, q1, q3, sum.
    #[arg(short = 'r', long = "resampling", default_value = "near")]
    resampling: String,
    /// GDAL resampling behavior to reproduce: 3.12 (default) or 3.13.
    #[arg(long, default_value = "3.12")]
    kernel_version: String,
    /// Source overview: AUTO, AUTO-n, NONE, or a level numbered from 0.
    #[arg(long, alias = "ovr", default_value = "AUTO")]
    overview: String,

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
    /// Maximum error of the approximate transformer, in pixels (0: exact). By default 0.125, or 0
    /// with an RPC DEM (`--to RPC_DEM=...`), as gdalwarp.
    #[arg(long)]
    et: Option<f64>,
    /// The order of the polynomial fitted to the first source's GCPs (1, 2 or 3), as
    /// `gdalwarp -order`. By default, GDAL's choice.
    #[arg(long)]
    order: Option<u32>,
    /// Fit thin plate splines to the first source's GCPs, as `gdalwarp -tps`.
    #[arg(long)]
    tps: bool,
    /// Georeference the first source by its RPCs (the GeoTIFF RPC tag, or a `_RPC.TXT` or `.RPB`
    /// sidecar), as `gdalwarp -rpc`.
    #[arg(long, conflicts_with = "tps")]
    rpc: bool,
    /// Require longitude/latitude geolocation arrays, as `gdalwarp -geoloc`.
    #[arg(long, conflicts_with_all = ["rpc", "tps", "order"])]
    geoloc: bool,
    /// A transformer option, as `gdalwarp -to` takes it: `RPC_HEIGHT=`, `RPC_HEIGHT_SCALE=`,
    /// `RPC_DEM=`, `RPC_DEMINTERPOLATION=near|bilinear|cubic`, `RPC_DEM_MISSING_VALUE=`,
    /// `RPC_DEM_SRS=`, `RPC_DEM_APPLY_VDATUM_SHIFT=`, `RPC_PIXEL_ERROR_THRESHOLD=`,
    /// `RPC_MAX_ITERATIONS=`, or `METHOD=RPC` (as `--rpc`).
    #[arg(long = "to", value_name = "KEY=VALUE")]
    transformer_options: Vec<String>,
    /// A warp option, as `gdalwarp -wo` takes it: `SOURCE_EXTRA=n`, `SAMPLE_STEPS=ALL`,
    /// `UNIFIED_SRC_NODATA=YES|NO|PARTIAL`,
    /// `SKIP_NOSOURCE=NO` (to warp beyond the footprint of a source with RPCs; `YES` is the
    /// default and changes nothing).
    #[arg(long = "wo", value_name = "KEY=VALUE")]
    warp_options: Vec<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let (output, inputs) = a.paths.split_last().expect("clap requires two paths");
    let inputs = &super::expand_file_lists(inputs)?[..];
    let pair = |v: &Option<Vec<f64>>| v.as_ref().map(|v| (v[0], v[1]));
    let to: Vec<(String, String)> =
        super::convert::key_values(&a.transformer_options)?.into_iter().map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())).collect();
    let wo: Vec<(String, String)> =
        super::convert::key_values(&a.warp_options)?.into_iter().map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())).collect();
    let option = |list: &[(String, String)], k: &str| list.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    let method = option(&to, "SRC_METHOD").or_else(|| option(&to, "METHOD")).map(|m| m.to_ascii_uppercase());
    let force_geoloc = a.geoloc || method.as_deref() == Some("GEOLOC_ARRAY");
    let force_rpc = a.rpc || method.as_deref() == Some("RPC");
    if force_geoloc && (force_rpc || a.tps || a.order.is_some()) {
        return Err(terrio::Error::InvalidArgument("geolocation arrays exclude RPC and GCP methods".into()));
    }
    if force_rpc && a.tps {
        return Err(terrio::Error::InvalidArgument("--tps and the RPC method exclude each other".into()));
    }
    let rpc_options = RpcOptions::from_options(&to)?;
    let has_dem = rpc_options.dem.is_some();
    let mut o = WarpOptions {
        kernel_version: terrio::ops::warp::KernelVersion::parse(&a.kernel_version)?,
        transform_options: terrio::crs::TransformOptions::from_options(&to)?,
        geoloc: force_geoloc,
        src_srs: srs(&a.s_srs)?,
        dst_srs: srs(&a.t_srs)?,
        resolution: pair(&a.tr),
        extent: a.te.as_ref().map(|v| (v[0], v[1], v[2], v[3])),
        extent_srs: srs(&a.te_srs)?,
        size: a.ts.as_ref().map(|v| (v[0], v[1])),
        dst_transform: a.dst_transform.as_ref().map(|v| [v[0], v[1], v[2], v[3], v[4], v[5]]),
        target_aligned_pixels: a.tap,
        resampling: Resampling::parse(&a.resampling)?,
        overview: terrio::ops::overview::OverviewSelection::parse(&a.overview)?,
        src_nodata: a.srcnodata.as_deref().map(Nodata::parse).transpose()?.unwrap_or_default(),
        dst_nodata: a.dstnodata.as_deref().map(Nodata::parse).transpose()?.unwrap_or_default(),
        output_type: a.ot.as_deref().map(parse_data_type).transpose()?,
        cutline: a.cutline.as_deref().map(|p| super::quiet(|| Cutline::open(p))).transpose()?,
        crop_to_cutline: a.crop_to_cutline,
        // As gdalwarp: exact transformations with an RPC DEM.
        error_threshold: a.et.unwrap_or(if has_dem { 0.0 } else { 0.125 }),
        // The first input's GCPs are read by `warp_to_path`. As gdalwarp: `-order` sets the
        // polynomial's order, but 4 or 5 GCPs still make a homography.
        gcp_method: if a.tps { GcpMethod::Tps } else { GcpMethod::Auto },
        order: a.order.unwrap_or(0),
        rpc_options,
        ..Default::default()
    };
    if o.target_aligned_pixels && o.resolution.is_none() {
        return Err(terrio::Error::InvalidArgument("--tap needs --tr".into()));
    }
    // A first source with RPCs, which the transformer may use (gdalwarp's FetchSrcMethod).
    let local = !inputs[0].contains("://") && !inputs[0].starts_with("/vsi");
    let rpc_allowed = !force_geoloc && !a.tps && method.as_deref().is_none_or(|m| m == "RPC");
    // Remote files are fetched for their RPCs only when asked to use them.
    // A file whose RPCs cannot be read has none, unless asked to use them.
    let file_rpc = if force_rpc {
        super::quiet(|| read_rpc(&inputs[0]))?
    } else if rpc_allowed && local {
        super::quiet(|| read_rpc(&inputs[0])).ok().flatten()
    } else {
        None
    };
    if force_rpc {
        o.rpc = Some(file_rpc.clone().ok_or_else(|| terrio::Error::InvalidArgument(format!("{} has no RPCs", inputs[0])))?);
    }
    if file_rpc.is_some() {
        // As gdalwarp for a source with RPC metadata (whatever georeferences it, unless a GCP
        // method is forced): a few more source pixels around each chunk, every
        // edge point sampled with a DEM, and only the output window the source covers warped
        // (into a new GeoTIFF).
        o.source_extra = Some(5);
        o.sample_steps_all = has_dem;
        let driver = a.out.options()?.driver.or_else(|| RasterDriver::for_extension(output));
        o.rpc_output_window = matches!(driver, Some(RasterDriver::GTiff) | Some(RasterDriver::GPKG));
    }
    for (k, v) in &wo {
        match k.as_str() {
            "SOURCE_EXTRA" => {
                o.source_extra = Some(
                    v.trim().parse().map_err(|_| terrio::Error::InvalidArgument(format!("SOURCE_EXTRA must be a number, not '{v}'")))?,
                )
            }
            "SAMPLE_STEPS" => o.sample_steps_all = v.eq_ignore_ascii_case("ALL"),
            "UNIFIED_SRC_NODATA" => {
                o.unified_src_nodata = match v.trim().to_ascii_uppercase().as_str() {
                    "YES" => Some(true),
                    "NO" => Some(false),
                    "PARTIAL" => {
                        if let Nodata::Values(values) = &o.src_nodata {
                            o.src_nodata = Nodata::PerBand(values.iter().copied().map(Some).collect());
                        }
                        None
                    }
                    _ => return Err(terrio::Error::InvalidArgument("UNIFIED_SRC_NODATA must be YES, NO or PARTIAL".into())),
                };
            }
            "SKIP_NOSOURCE" => o.rpc_output_window &= !matches!(v.to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"),
            // Options that tune GDAL's warper without changing the result (NUM_THREADS,
            // INIT_DEST...) are ignored.
            _ => eprintln!("terrio: warning: the warp option {k} is ignored"),
        }
    }
    super::quiet(|| warp_to_path(inputs, &o, output, &a.out.options()?))
}
