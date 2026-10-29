//! Read-only dataset format identification.
use terrio::inspection::{IdentifyOptions, identify_many};

#[derive(clap::Args)]
pub struct IdentifyArgs {
    /// Input files, local dataset directories or individual remote objects.
    #[arg(required = true)]
    inputs: Vec<String>,
    /// Recursively scan local directories, stopping at recognized dataset directories.
    #[arg(short, long)]
    recursive: bool,
    /// Also descend into recognized directories, such as Zarr stores.
    #[arg(long)]
    force_recursive: bool,
    /// Permit dataset readers when a bounded header cannot identify the format.
    /// Fallback readers may materialize formats without lazy metadata support.
    #[arg(long)]
    deep: bool,
    /// Include unidentified inputs and I/O failures.
    #[arg(long)]
    report_failures: bool,
    /// Emit structured records as JSON.
    #[arg(long)]
    json: bool,
}

pub fn identify(args: IdentifyArgs) -> terrio::Result<()> {
    let records = super::quiet(|| {
        identify_many(
            &args.inputs,
            &IdentifyOptions {
                recursive: args.recursive,
                force_recursive: args.force_recursive,
                deep: args.deep,
                report_failures: args.report_failures,
            },
        )
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&records).map_err(|error| terrio::Error::InvalidData(error.to_string()))?);
    } else {
        for record in records {
            match record.identification {
                Some(format) => println!("{}: {}", record.name, format.driver),
                None => println!("{}: unrecognized ({})", record.name, record.error.unwrap_or_default()),
            }
        }
    }
    Ok(())
}
