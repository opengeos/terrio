//! `terrio xyz` (also exposed automatically as an MCP tool).
use std::io::Write;
use terrio::ops::xyz::{XyzOptions, write_xyz};
use terrio::{Error, Result};

#[derive(clap::Args)]
pub struct Args {
    /// The raster.
    input: String,
    /// The output text file; standard output when omitted or `-`.
    output: Option<String>,
    /// Bands to write, from 1 (repeat, or comma-separated). By default, band 1.
    #[arg(short = 'b', long = "band", value_delimiter = ',', conflicts_with = "allbands")]
    bands: Vec<usize>,
    /// Write all bands.
    #[arg(long)]
    allbands: bool,
    /// Write every Nth column and row: `N`, or `X,Y` for columns and rows.
    #[arg(long, value_name = "N|X,Y", default_value = "1")]
    skip: String,
    /// Write only this window, in pixels.
    #[arg(long, num_args = 4, value_names = ["XOFF", "YOFF", "XSIZE", "YSIZE"])]
    srcwin: Option<Vec<usize>>,
    /// Leave out pixels whose values all equal the source nodata.
    #[arg(long, visible_alias = "skip-nodata")]
    skipnodata: bool,
    /// The source nodata: one value, or one per band (comma-separated). By default, the bands'.
    #[arg(long, value_delimiter = ',', allow_negative_numbers = true, value_name = "VALUE")]
    srcnodata: Vec<f64>,
    /// Write these values for nodata pixels: one, or one per band (comma-separated).
    #[arg(long, value_delimiter = ',', allow_negative_numbers = true, value_name = "VALUE")]
    dstnodata: Vec<f64>,
    /// Separate values with commas rather than spaces.
    #[arg(long)]
    csv: bool,
    /// Separate values with this text.
    #[arg(long, conflicts_with = "csv")]
    delimiter: Option<String>,
}

fn parse_skip(s: &str) -> Result<(usize, usize)> {
    let n = |v: &str| v.trim().parse::<usize>().map_err(|_| Error::InvalidArgument(format!("invalid --skip '{s}'")));
    match s.split_once(',') {
        Some((x, y)) => Ok((n(x)?, n(y)?)),
        None => {
            let v = n(s)?;
            Ok((v, v))
        }
    }
}

pub fn run(a: Args) -> Result<()> {
    let options = XyzOptions {
        bands: if a.allbands {
            Vec::new()
        } else if a.bands.is_empty() {
            vec![1]
        } else {
            a.bands.clone()
        },
        skip: parse_skip(&a.skip)?,
        srcwin: a.srcwin.as_ref().map(|w| [w[0], w[1], w[2], w[3]]),
        skip_nodata: a.skipnodata,
        src_nodata: (!a.srcnodata.is_empty()).then(|| a.srcnodata.clone()),
        dst_nodata: (!a.dstnodata.is_empty()).then(|| a.dstnodata.clone()),
        delimiter: a.delimiter.clone().unwrap_or_else(|| if a.csv { ",".into() } else { " ".into() }),
    };
    let mut raster = super::rasterout::open_lazy(&a.input)?;
    // Checked before the output is created, so that a refused command leaves no file behind.
    terrio::ops::xyz::check_xyz(&raster, &options)?;
    if let Some(path) = a.output.as_deref().filter(|p| *p != "-")
        && (path == a.input || super::rasterout::same_file(path, &a.input))
    {
        return Err(Error::InvalidArgument("the output would overwrite the input".into()));
    }
    match a.output.as_deref() {
        None | Some("-") => {
            let stdout = std::io::stdout();
            let mut out = std::io::BufWriter::new(stdout.lock());
            write_xyz(&mut raster, &mut out, &options)?;
            out.flush()?;
        }
        Some(path) => {
            terrio::ops::xyz::write_xyz_file(&mut raster, std::path::Path::new(path), &options)?;
        }
    }
    Ok(())
}
