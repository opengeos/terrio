//! Metadata-only RPF table of contents creation.
use std::path::PathBuf;
use terrio::ops::rpftoc::{RpfTocOptions, create};

#[derive(clap::Args)]
pub struct Args {
    /// Directory recursively containing existing CADRG frames; symlinks are skipped.
    /// Input/output directories must remain stable; synchronize concurrent directory changes.
    input: PathBuf,
    /// TOC filename, default input/A.TOC; parent must be input or an ancestor.
    output: Option<PathBuf>,
    /// Reciprocal scale override, otherwise inferred from each data-series code.
    #[arg(long)]
    scale: Option<u32>,
    #[arg(long, default_value = "TERRI")]
    producer_id: String,
    #[arg(long, default_value = "Terrio")]
    producer_name: String,
    #[arg(long, default_value = "")]
    country_code: String,
    #[arg(long, default_value="U", value_parser=["U","R","C","S","T"])]
    classification: String,
    /// Atomically replace any existing output file after input validation; validated input frames are protected.
    /// Preserves Unix mode bits, not ownership, extended ACLs or extended attributes.
    #[arg(long)]
    overwrite: bool,
}
pub fn run(args: &Args) -> terrio::Result<()> {
    let options = RpfTocOptions {
        scale: args.scale,
        producer_id: args.producer_id.clone(),
        producer_name: args.producer_name.clone(),
        country_code: args.country_code.clone(),
        classification: args.classification.chars().next().unwrap_or('U'),
    };
    let report = create(&args.input, args.output.as_deref(), &options, args.overwrite)?;
    println!("{}", serde_json::to_string(&report).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    Ok(())
}
