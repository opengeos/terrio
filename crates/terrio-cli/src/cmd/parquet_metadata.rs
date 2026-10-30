// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_driver_parquet_create_metadata_file.rst
// No code is translated from these sources.

//! Metadata-only Parquet aggregation, also exposed through MCP.
#[derive(clap::Args)]
pub struct Args {
    /// Explicit partition files in row-group order.
    #[arg(required = true)]
    inputs: Vec<String>,
    /// Metadata-only output, conventionally named _metadata.
    #[arg(short, long)]
    output: String,
    /// Replace an existing metadata output.
    #[arg(long)]
    overwrite: bool,
    /// Emit row-group counts, rows, bytes and omitted metadata keys as JSON.
    #[arg(long)]
    json: bool,
}
pub fn run(args: Args) -> terrio::Result<()> {
    let report = super::quiet(|| {
        terrio::parquet_metadata::create(
            &args.inputs,
            &args.output,
            &terrio::parquet_metadata::MetadataOptions { overwrite: args.overwrite },
        )
    })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    } else {
        println!("{} files, {} row groups, {} rows, {} bytes", report.files, report.row_groups, report.rows, report.bytes);
    }
    Ok(())
}
