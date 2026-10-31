// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_raster_tile.rst
// No code is translated from these sources.

//! Directory tiles using the shared incremental renderer.
use terrio::ops::raster_tile::{self as tile, Convention, GridOptions, TileLimits, TileOptions};

#[derive(clap::Args)]
pub struct Args {
    /// Input raster (local, remote or archive member).
    input: String,
    /// Output directory (must be new unless --resume).
    output: std::path::PathBuf,
    /// WebMercatorQuad, WorldCRS84Quad or raster.
    #[arg(long, default_value = "WebMercatorQuad")]
    tiling_scheme: String,
    #[arg(long)]
    min_zoom: Option<u8>,
    #[arg(long)]
    max_zoom: Option<u8>,
    #[arg(long, default_value_t = 256)]
    tile_size: usize,
    /// Minimum column at maximum zoom.
    #[arg(long)]
    min_x: Option<u64>,
    #[arg(long)]
    max_x: Option<u64>,
    /// Minimum top-origin row at maximum zoom, also with TMS filenames.
    #[arg(long)]
    min_y: Option<u64>,
    #[arg(long)]
    max_y: Option<u64>,
    #[arg(long)]
    no_intersection_ok: bool,
    #[arg(long, default_value = "xyz", value_parser = ["xyz", "tms"])]
    convention: String,
    /// PNG (Byte gray/RGB/palette) or GTiff (retains sample types).
    #[arg(short = 'f', long, default_value = "PNG")]
    format: String,
    #[arg(short = 'r', long, default_value = "cubic")]
    resampling: String,
    /// Lower zooms resample directly from source, rather than previously encoded tiles.
    #[arg(long)]
    overview_resampling: Option<String>,
    #[arg(long, conflicts_with = "no_alpha")]
    add_alpha: bool,
    #[arg(long)]
    no_alpha: bool,
    #[arg(long, allow_hyphen_values = true)]
    output_nodata: Option<f64>,
    #[arg(long)]
    skip_blank: bool,
    /// Fill missing files; existing tiles are trusted without checking their contents/options.
    #[arg(long)]
    resume: bool,
    #[arg(long, default_value_t = 1_000_000)]
    max_tiles: u64,
    /// Report grid without reading pixels or creating output.
    #[arg(long)]
    dry_run: bool,
}
pub fn run(args: &Args) -> terrio::Result<()> {
    let options = TileOptions {
        tiling_scheme: args.tiling_scheme.clone(),
        grid: GridOptions {
            tile_size: args.tile_size,
            min_zoom: args.min_zoom,
            max_zoom: args.max_zoom,
            limits: TileLimits { min_x: args.min_x, max_x: args.max_x, min_y: args.min_y, max_y: args.max_y },
            no_intersection_ok: args.no_intersection_ok,
        },
        convention: if args.convention == "tms" { Convention::Tms } else { Convention::Xyz },
        format: args.format.clone(),
        resampling: args.resampling.clone(),
        overview_resampling: args.overview_resampling.clone(),
        alpha: if args.add_alpha {
            Some(true)
        } else if args.no_alpha {
            Some(false)
        } else {
            None
        },
        output_nodata: args.output_nodata,
        skip_blank: args.skip_blank,
        max_tiles: args.max_tiles,
    };
    let mut source = super::rasterout::open_lazy(&args.input)?;
    if args.dry_run {
        let (plan, _) = tile::plan(&source.header(), &options)?;
        println!("{}", serde_json::to_string_pretty(&plan).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    } else {
        let report = tile::to_directory(&mut source, &options, &args.output, args.resume)?;
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    }
    Ok(())
}
