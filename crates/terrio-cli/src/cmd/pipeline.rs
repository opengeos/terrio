//! Shared pipeline parsing and execution. This module contains no algorithms.
use terrio::ops::pipeline::{InputKind, Pipeline, PipelineData};
use terrio::{Error, Result};

#[derive(clap::Args)]
pub struct Args {
    /// Load a .gdalg.json definition instead of supplying steps.
    #[arg(long, conflicts_with = "arguments")]
    load: Option<String>,
    /// Save a GDAL-readable pipeline definition without evaluating the input.
    #[arg(long, conflicts_with = "output")]
    save: Option<String>,
    /// Replace or append the final output destination (useful with --load).
    #[arg(long)]
    output: Option<String>,
    /// Explicit input family: auto, raster or vector.
    #[arg(long)]
    input_kind: Option<InputKind>,
    /// Replace the final output if it exists. Also available on each write step.
    #[arg(long)]
    overwrite: bool,
    /// Steps separated by !. Supported: read, filter, select, clip, resize,
    /// reproject, set-type, buffer, simplify, segmentize, swap-xy, dissolve,
    /// as-features, rasterize, materialize and write. Quote expressions as one argument.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    arguments: Vec<String>,
}
pub fn run(args: Args) -> Result<()> {
    let mut plan = if let Some(path) = args.load {
        Pipeline::open(&path)?
    } else if args.arguments.len() == 1 && args.arguments[0].to_ascii_lowercase().ends_with(".gdalg.json") {
        Pipeline::open(&args.arguments[0])?
    } else {
        Pipeline::from_tokens(&args.arguments)?
    };
    if let Some(kind) = args.input_kind {
        plan = plan.with_kind(kind);
    }
    if let Some(save) = args.save {
        plan = plan.with_output(&save, Some("GDALG"), args.overwrite);
    } else if let Some(output) = args.output {
        plan = plan.with_output(&output, None, args.overwrite);
    } else if args.overwrite {
        plan = plan.overwrite_output()?;
    }
    let result = plan.run()?;
    let value = match result.data {
        None => serde_json::json!({"type":"saved-pipeline","outputs":result.output_names()}),
        Some(PipelineData::Raster(ref reader)) => {
            serde_json::json!({"type":"raster","width":reader.width(),"height":reader.height(),"bands":reader.band_count(),"outputs":result.output_names()})
        }
        Some(PipelineData::Vector(ref layers)) => {
            serde_json::json!({"type":"vector","layers":layers.iter().map(|l|serde_json::json!({"name":l.name,"features":l.features.len()})).collect::<Vec<_>>(),"outputs":result.output_names()})
        }
    };
    println!("{}", serde_json::to_string_pretty(&value).map_err(|e| Error::InvalidData(e.to_string()))?);
    Ok(())
}
