//! `terrio validate-cog` (also exposed automatically as an MCP tool).

#[derive(clap::Args)]
pub struct Args {
    /// The file to check: a local path, a `/vsizip/` path or an HTTP(S) or S3 URL.
    input: String,
    /// Treat the warnings (a large image without overviews) as errors.
    #[arg(long)]
    strict: bool,
    /// Validation rules: rio-cogeo compatibility or GDAL 3.13.
    #[arg(long, default_value = "rio-cogeo", value_parser = ["rio-cogeo", "gdal"])]
    profile: String,
    /// In the GDAL profile, inspect every block's leaders, trailers and ordering.
    /// Auto enables full checks locally and disables them for remote files.
    /// Malformed XML or GDAL metadata exceeding 4 MiB returns an error instead of a verdict.
    #[arg(long, default_value = "auto", value_parser = ["auto", "yes", "no"])]
    full_check: String,
    /// Print the result as JSON: validity, errors, warnings, IFD and data offsets, overview
    /// factors, GDAL's structural metadata and layout notes.
    #[arg(long)]
    json: bool,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let options = terrio::CogValidationOptions {
        strict: a.strict,
        profile: if a.profile == "gdal" { terrio::CogValidationProfile::Gdal } else { terrio::CogValidationProfile::RioCogeo },
        full_check: match a.full_check.as_str() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        },
    };
    let v = super::quiet(|| terrio::validate_cog_with_options(&a.input, &options))?;
    if a.json {
        let report = serde_json::json!({
            "valid": v.is_valid,
            "errors": v.errors,
            "warnings": v.warnings,
            "ifd_offsets": v.ifd_offsets,
            "data_offsets": v.data_offsets,
            "overview_factors": v.overview_factors,
            "structural_metadata": v.structural_metadata,
            "layout_notes": v.layout_notes,
        });
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| terrio::Error::InvalidArgument(e.to_string()))?);
    } else {
        if !v.errors.is_empty() {
            println!("The following errors were found:");
            for e in &v.errors {
                println!(" - {e}");
            }
        }
        if !v.warnings.is_empty() {
            println!("The following warnings were found:");
            for w in &v.warnings {
                println!(" - {w}");
            }
        }
        for n in &v.layout_notes {
            println!("Note: {n}");
        }
        if v.is_valid {
            println!("{} is a valid cloud optimized GeoTIFF", a.input);
        } else {
            println!("{} is NOT a valid cloud optimized GeoTIFF", a.input);
        }
    }
    // As GDAL's script: 1 when the file is not a valid COG.
    super::set_exit_code(if v.is_valid { 0 } else { 1 });
    Ok(())
}
