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

/// Read-only GeoPackage validation, also exposed as an MCP tool.
#[derive(clap::Args)]
pub struct ValidateArgs {
    /// Closed/checkpointed native GeoPackage or /vsimem/ image.
    input: String,
    /// Add advisory attribute range, length, date and storage-class checks.
    #[arg(long)]
    full_check: bool,
    /// Treat warnings as validation failures.
    #[arg(long)]
    strict: bool,
    /// Unicode case functions for files created with ICU-enabled SQLite.
    #[arg(long)]
    unicode_case: bool,
    /// Print validity, completeness, version and structured diagnostics as JSON.
    #[arg(long)]
    json: bool,
}
pub fn validate(args: ValidateArgs) -> terrio::Result<()> {
    let options = terrio::GpkgValidationOptions { full_check: args.full_check, strict: args.strict, unicode_case: args.unicode_case };
    let report = super::quiet(|| terrio::validate_gpkg(&args.input, &options))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    } else {
        for error in &report.errors {
            println!("Error {}: {}", error.requirement, error.message);
        }
        for warning in &report.warnings {
            println!("Warning: {warning}");
        }
        for why in &report.unsupported_checks {
            println!("Incomplete check: {why}");
        }
        println!(
            "{}: {}",
            args.input,
            if report.is_valid {
                "valid GeoPackage"
            } else if !report.is_complete {
                "validation incomplete"
            } else {
                "invalid GeoPackage"
            }
        );
    }
    super::set_exit_code(if !report.is_complete {
        2
    } else if report.is_valid {
        0
    } else {
        1
    });
    Ok(())
}
