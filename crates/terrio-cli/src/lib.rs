//! The `terrio` command-line tool, as a library: the `terrio` binary and the `terrio` command of
//! the Python package both run [`run`].

mod cmd;
mod mcp;

use clap::{Parser, Subcommand};
use std::ffi::OsString;

/// Pure-Rust geospatial raster, vector and point cloud data I/O.
#[derive(Parser)]
#[command(name = "terrio", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Summarize a raster, vector dataset or point cloud.
    Info(cmd::info::Args),
    /// Convert a dataset to another format, optionally reprojecting it. Point clouds convert to
    /// LAS, LAZ, COPC or vector point layers.
    Convert(cmd::convert::Args),
    /// List the formats terrio reads and writes.
    Formats(cmd::formats::Args),
    /// Describe a coordinate reference system, or transform coordinates between two.
    Crs(cmd::crs::Args),
    /// Burn vector geometries into a raster.
    Rasterize(cmd::rasterize::Args),
    /// Turn a raster band into polygons of connected pixels with the same value.
    Polygonize(cmd::polygonize::Args),
    /// Subset, resize, rescale or change the type of a raster (like gdal_translate).
    Translate(cmd::translate::Args),
    /// Reproject and resample rasters (like gdalwarp).
    Warp(cmd::warp::Args),
    /// Mosaic rasters into one, or into a virtual raster (like gdalbuildvrt and gdal_merge).
    Mosaic(cmd::mosaic::Args),
    /// Terrain analysis: hillshade, slope, aspect, color relief, TRI, TPI, roughness (like gdaldem).
    Dem(cmd::dem::Args),
    /// Raster algebra: band math in NumPy syntax over one or more rasters (like gdal_calc.py).
    Calc(cmd::calc::Args),
    /// Distance from every pixel to the nearest target pixel (like gdal_proximity.py).
    Proximity(cmd::proximity::Args),
    /// Merge raster areas smaller than a threshold into their largest neighbour (like gdal_sieve.py).
    Sieve(cmd::sieve::Args),
    /// Grid a point cloud into a raster: min, max, mean, IDW, count or standard deviation per cell
    /// (like PDAL's writers.gdal), or a TIN or nearest-neighbour interpolation (like gdal_grid).
    Grid(cmd::grid::Args),
    /// Filter a point cloud as PDAL's filters do: ground classification (SMRF, PMF, CSF), noise,
    /// heights above ground, crops, expressions, assignments, tiles, normals, clusters, sampling.
    Filter(cmd::filter::Args),
    /// Serve these commands to AI agents (Claude Code, Claude Desktop, Cursor, VS Code…) as a
    /// Model Context Protocol server over standard input and output.
    Mcp,
}

/// Runs the command line `args` (the program name first) and returns the process exit code: 0 on
/// success, 1 when the command fails, 2 for a usage error (as clap reports it).
pub fn run(args: Vec<OsString>) -> i32 {
    run_as(args, None)
}

/// [`run`], where `terrio` is the command line that runs terrio (a Python interpreter with
/// `-m terrio`, for the Python package), for `terrio mcp` to run its tools with. `None` is this
/// executable.
pub fn run_as(args: Vec<OsString>, terrio: Option<Vec<OsString>>) -> i32 {
    let cli = match Cli::try_parse_from(cmd::calc::rewrite_args(args)) {
        Ok(cli) => cli,
        Err(e) => {
            // --help and --version print to standard output and succeed; usage errors do not.
            let _ = e.print();
            return e.exit_code();
        }
    };
    let result = match cli.command {
        Command::Info(a) => cmd::info::run(a),
        Command::Convert(a) => cmd::convert::run(a),
        Command::Formats(a) => cmd::formats::run(a),
        Command::Crs(a) => cmd::crs::run(a),
        Command::Rasterize(a) => cmd::rasterize::run(a),
        Command::Polygonize(a) => cmd::polygonize::run(a),
        Command::Translate(a) => cmd::translate::run(a),
        Command::Warp(a) => cmd::warp::run(a),
        Command::Mosaic(a) => cmd::mosaic::run(a),
        Command::Dem(a) => cmd::dem::run(a),
        Command::Calc(a) => cmd::calc::run(a),
        Command::Proximity(a) => cmd::proximity::run(a),
        Command::Sieve(a) => cmd::sieve::run(a),
        Command::Grid(a) => cmd::grid::run(a),
        Command::Filter(a) => cmd::filter::run(a),
        Command::Mcp => terrio.map_or_else(mcp::this_binary, Ok).and_then(mcp::serve).map_err(terrio::Error::from),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("terrio: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_command_line_is_well_formed() {
        // Clap's checks: unique argument names across flattened option groups, among others.
        <super::Cli as clap::CommandFactory>::command().debug_assert();
    }
}
