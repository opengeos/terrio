//! `terrio mosaic`.

use super::rasterout::{OutputArgs, open_lazy, srs};
use std::path::Path;
use terrio::ops::Resampling;
use terrio::ops::mosaic::{MosaicOptions, MosaicSource, ResolutionStrategy, build_vrt, mosaic_to_path};
use terrio::ops::warp::Nodata;

#[derive(clap::Args)]
pub struct Args {
    /// The input rasters, then the output path. A `.vrt` output is a virtual mosaic referencing
    /// the inputs, as `gdalbuildvrt` writes. `@FILE` reads input paths from FILE, one per line.
    #[arg(required = true, num_args = 2..)]
    paths: Vec<String>,
    /// The resolution: highest, lowest, average, or user (with `--tr`).
    #[arg(long, default_value = "average")]
    resolution: String,
    /// The output resolution (implies `--resolution user`).
    #[arg(long, num_args = 2, value_names = ["XRES", "YRES"])]
    tr: Option<Vec<f64>>,
    /// The output extent.
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true)]
    te: Option<Vec<f64>>,
    /// Align the extent on multiples of the resolution.
    #[arg(long)]
    tap: bool,
    /// Put each input in its own band instead of mosaicking them.
    #[arg(long)]
    separate: bool,
    /// A band to use, from 1; repeat for several.
    #[arg(short = 'b', long = "band")]
    bands: Vec<usize>,
    /// Ignore input pixels with this value and use it as the output nodata.
    #[arg(long, allow_hyphen_values = true)]
    nodata: Option<String>,
    /// Ignore input pixels with these values (or `none` to ignore the inputs' nodata).
    #[arg(long, allow_hyphen_values = true)]
    srcnodata: Option<String>,
    /// The output nodata (or `none`). By default, the inputs'.
    #[arg(long, allow_hyphen_values = true)]
    dstnodata: Option<String>,
    /// Resampling when inputs are resampled or reprojected.
    #[arg(short = 'r', long = "resampling", default_value = "near")]
    resampling: String,
    /// The output CRS; inputs in other CRSs are reprojected. By default, the first input's.
    #[arg(long = "t-srs", value_name = "CRS")]
    t_srs: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let (output, inputs) = a.paths.split_last().expect("clap requires two paths");
    let inputs = &super::expand_file_lists(inputs)?[..];
    let resolution = match (&a.tr, a.resolution.to_ascii_lowercase().as_str()) {
        (Some(v), _) => ResolutionStrategy::User(v[0], v[1]),
        (None, "user") => return Err(terrio::Error::InvalidArgument("--resolution user needs --tr".into())),
        (None, s) => ResolutionStrategy::parse(s)?,
    };
    let parse = |s: &Option<String>| s.as_deref().map(Nodata::parse).transpose();
    let nodata = parse(&a.nodata)?;
    let o = MosaicOptions {
        resolution,
        extent: a.te.as_ref().map(|v| (v[0], v[1], v[2], v[3])),
        target_aligned_pixels: a.tap,
        separate: a.separate,
        bands: a.bands,
        src_nodata: parse(&a.srcnodata)?.or(nodata.clone()).unwrap_or_default(),
        dst_nodata: parse(&a.dstnodata)?.or(nodata).unwrap_or_default(),
        resampling: Resampling::parse(&a.resampling)?,
        dst_srs: srs(&a.t_srs)?,
    };
    if !output.to_ascii_lowercase().ends_with(".vrt") {
        // The inputs read by windows where they can be.
        return super::quiet(|| mosaic_to_path(inputs, &o, output, &a.out.options()?));
    }
    // A VRT only references its inputs: read their headers, not their pixels.
    let datasets = inputs.iter().map(|p| open_lazy(p).map(|l| l.header())).collect::<terrio::Result<Vec<_>>>()?;
    let sources: Vec<MosaicSource> = inputs.iter().zip(&datasets).map(|(p, ds)| MosaicSource { path: p.clone(), ds }).collect();
    let dir = Path::new(output).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    build_vrt(&sources, &o, Some(dir))?.write(output)
}
