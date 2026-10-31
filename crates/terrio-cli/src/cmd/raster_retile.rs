//! Native argument adaptation for the shared retile engine.
use terrio::ops::raster_retile::{self as retile, RetileOptions, RetileSource};

#[derive(clap::Args)]
pub struct Args {
    /// Input rasters, in overwrite order (later inputs win).
    #[arg(required=true, num_args=1..)]
    inputs: Vec<String>,
    /// Output directory; must be new unless --resume.
    #[arg(short = 'o', long, visible_alias = "target-dir")]
    output: std::path::PathBuf,
    /// Output tile width and height in pixels.
    #[arg(long,num_args=2,value_names=["WIDTH","HEIGHT"],default_values=["256","256"])]
    tile_size: Vec<usize>,
    #[arg(long, default_value_t = 0)]
    overlap: usize,
    /// Additional recursive pyramid levels.
    #[arg(long, default_value_t = 0)]
    levels: u8,
    #[arg(long)]
    pyramid_only: bool,
    #[arg(long, visible_alias = "use-dir-for-each-row")]
    row_directories: bool,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(short = 'f', long, default_value = "GTiff")]
    format: String,
    #[arg(long, visible_alias = "ot")]
    output_type: Option<String>,
    #[arg(short = 'r', long, default_value = "nearest")]
    resampling: String,
    #[arg(long, visible_alias = "s-srs")]
    source_srs: Option<String>,
    /// Raster writer creation option, NAME=VALUE; repeat as needed.
    #[arg(long = "co")]
    creation_options: Vec<String>,
    /// Per-level CSV basename.
    #[arg(long)]
    csv: Option<String>,
    #[arg(long, default_value = ";")]
    csv_delimiter: String,
    /// Per-level .shp, .geojson or .gpkg tile index basename.
    #[arg(long)]
    tile_index: Option<String>,
    #[arg(long, default_value = "location")]
    tile_index_field: String,
    #[arg(long, default_value_t = 1_000_000)]
    max_tiles: u64,
    #[arg(long)]
    resume: bool,
    /// Validate metadata and report the layout without reading pixels or writing files.
    #[arg(long)]
    dry_run: bool,
}
pub fn run(args: Args) -> terrio::Result<()> {
    let creation_options = args
        .creation_options
        .iter()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .filter(|(k, _)| !k.is_empty())
                .ok_or_else(|| terrio::Error::InvalidArgument("--co requires NAME=VALUE".into()))
        })
        .collect::<terrio::Result<_>>()?;
    let options = RetileOptions {
        tile_size: [args.tile_size[0], args.tile_size[1]],
        overlap: args.overlap,
        levels: args.levels,
        pyramid_only: args.pyramid_only,
        row_directories: args.row_directories,
        prefix: args.prefix,
        format: args.format,
        output_type: args.output_type,
        resampling: args.resampling,
        source_srs: args.source_srs,
        creation_options,
        csv: args.csv,
        csv_delimiter: args.csv_delimiter,
        tile_index: args.tile_index,
        tile_index_field: args.tile_index_field,
        max_tiles: args.max_tiles,
    };
    let mut readers = args.inputs.iter().map(|s| super::rasterout::open_lazy(s)).collect::<terrio::Result<Vec<_>>>()?;
    let mut sources: Vec<_> =
        readers.iter_mut().zip(&args.inputs).map(|(source, name)| RetileSource { name: name.clone(), source }).collect();
    let value = if args.dry_run {
        serde_json::to_value(retile::plan(&mut sources, &options)?)
    } else {
        serde_json::to_value(retile::to_directory(&mut sources, &options, &args.output, args.resume)?)
    }
    .map_err(|e| terrio::Error::InvalidData(e.to_string()))?;
    println!("{}", serde_json::to_string_pretty(&value).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    Ok(())
}
