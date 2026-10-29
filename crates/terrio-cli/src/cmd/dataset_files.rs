//! Dataset-aware file operations; CLI commands also provide the MCP tools.
use terrio::dataset_files::{self, FileOptions};

#[derive(clap::Args)]
pub struct TransferArgs {
    /// Source dataset. Companion files are included; this does not convert the format.
    source: String,
    /// Exact destination dataset name (not a directory to copy into).
    destination: String,
    /// Restrict the input driver, for example ENVI or "ESRI Shapefile".
    #[arg(long, visible_alias = "format")]
    input_format: Option<String>,
    /// Replace the destination dataset and remove its obsolete companion files.
    #[arg(long)]
    overwrite: bool,
    /// Emit the affected file set and copied bytes as JSON.
    #[arg(long)]
    json: bool,
}
#[derive(clap::Args)]
pub struct DeleteArgs {
    /// Datasets to remove, including owned companions; VRT source assets are retained.
    #[arg(required = true)]
    inputs: Vec<String>,
    /// Restrict the input driver.
    #[arg(long, visible_alias = "format")]
    input_format: Option<String>,
    /// Emit one report per dataset as JSON.
    #[arg(long)]
    json: bool,
}
#[derive(clap::Args)]
pub struct FilesArgs {
    /// Dataset whose owned files should be listed.
    input: String,
    /// Restrict the input driver.
    #[arg(long, visible_alias = "format")]
    input_format: Option<String>,
    /// Emit a structured file inventory.
    #[arg(long)]
    json: bool,
}
fn emit(report: &serde_json::Value) -> terrio::Result<()> {
    println!("{}", serde_json::to_string_pretty(report).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    Ok(())
}
pub fn transfer(args: TransferArgs, rename: bool) -> terrio::Result<()> {
    let options = FileOptions { format: args.input_format, overwrite: args.overwrite };
    let report = super::quiet(|| {
        if rename {
            dataset_files::rename(&args.source, &args.destination, &options)
        } else {
            dataset_files::copy(&args.source, &args.destination, &options)
        }
    })?;
    if args.json {
        emit(&serde_json::json!(report))?;
    }
    Ok(())
}
pub fn delete(args: DeleteArgs) -> terrio::Result<()> {
    let reports = super::quiet(|| dataset_files::delete_many(&args.inputs, args.input_format.as_deref()))?;
    if args.json {
        emit(&serde_json::json!(reports))?;
    }
    Ok(())
}
pub fn files(args: FilesArgs) -> terrio::Result<()> {
    let report = super::quiet(|| dataset_files::file_list(&args.input, args.input_format.as_deref()))?;
    if args.json {
        emit(&serde_json::json!(report))?;
    } else {
        for file in report.display_names() {
            println!("{file}");
        }
    }
    Ok(())
}
