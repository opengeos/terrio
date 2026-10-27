//! `terrio compare` (also exposed automatically as an MCP tool).
use terrio::ops::compare::{CompareOptions, compare};

#[derive(clap::Args)]
pub struct Args {
    /// The reference (golden) raster.
    reference: String,
    /// The raster compared with it.
    input: String,
    /// Skip every optional comparison: only sizes, band counts, band descriptions, data types,
    /// color interpretations, nodata values and pixels are compared. Takes precedence over individual skip flags.
    #[arg(long)]
    skip_all_optional: bool,
    /// Skip the byte-by-byte comparison of the files.
    #[arg(long)]
    skip_binary: bool,
    /// Skip the CRS comparison.
    #[arg(long)]
    skip_crs: bool,
    /// Skip the geotransform comparison.
    #[arg(long)]
    skip_geotransform: bool,
    /// Skip the overview comparison.
    #[arg(long)]
    skip_overview: bool,
    /// Skip the dataset and band metadata comparison (default domain).
    #[arg(long)]
    skip_metadata: bool,
    /// Skip the RPC metadata comparison.
    #[arg(long)]
    skip_rpc: bool,
    /// Skip the GEOLOCATION metadata comparison.
    #[arg(long)]
    skip_geolocation: bool,
    /// Skip the subdataset comparison.
    #[arg(long)]
    skip_subdataset: bool,
    /// Print the report as JSON: the difference count, the differences, and the pixels compared,
    /// differing and the largest absolute difference of every band, overview and mask.
    #[arg(long)]
    json: bool,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let options = if a.skip_all_optional {
        CompareOptions::skip_all_optional()
    } else {
        CompareOptions {
            skip_binary: a.skip_binary,
            skip_crs: a.skip_crs,
            skip_geotransform: a.skip_geotransform,
            skip_overview: a.skip_overview,
            skip_metadata: a.skip_metadata,
            skip_rpc: a.skip_rpc,
            skip_geolocation: a.skip_geolocation,
            skip_subdataset: a.skip_subdataset,
        }
    };
    let c = super::quiet(|| compare(&a.reference, &a.input, &options))?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&c.to_json()).map_err(|e| terrio::Error::InvalidArgument(e.to_string()))?);
    } else {
        print!("{}", c.to_text());
    }
    // The exit status is the difference count, as GDAL's (at most 255, the largest status).
    super::set_exit_code(c.difference_count().min(255) as i32);
    Ok(())
}
