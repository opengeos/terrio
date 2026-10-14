//! The `terrio` command-line tool.

mod cmd;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

/// Pure-Rust geospatial raster and vector data I/O.
#[derive(Parser)]
#[command(name = "terrio", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarize a raster or vector dataset.
    Info(cmd::info::Args),
    /// Convert a dataset to another format, optionally reprojecting it.
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
}

fn main() -> ExitCode {
    // Exit quietly when the reader of our output goes away (`terrio formats | head`).
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
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
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("terrio: {e}");
            ExitCode::FAILURE
        }
    }
}
