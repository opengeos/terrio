//! GeoPackage storage maintenance, shared with the MCP command surface.
#[derive(clap::Args)]
pub struct RepackArgs {
    /// Native GeoPackage or an in-memory /vsimem/ file to compact in place.
    input: String,
    /// Emit before/after physical storage sizes as JSON.
    #[arg(long)]
    json: bool,
}
pub fn repack(args: RepackArgs) -> terrio::Result<()> {
    let report = super::quiet(|| terrio::gpkg_maintenance::repack(&args.input))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).map_err(|error| terrio::Error::InvalidData(error.to_string()))?);
    } else {
        println!("{} -> {} bytes ({} -> {} pages)", report.bytes_before, report.bytes_after, report.pages_before, report.pages_after);
    }
    Ok(())
}
