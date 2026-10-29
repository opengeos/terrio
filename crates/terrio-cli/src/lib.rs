//! The `terrio` command-line tool, as a library: the `terrio` binary and the `terrio` command of
//! the Python package both run [`run`].

mod cmd;
mod mcp;

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

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
    /// Repack a GeoPackage in place, preserving all encoded records and schema objects.
    GpkgRepack(cmd::gpkg::RepackArgs),
    /// Identify many datasets from bounded headers, without decoding their payloads.
    Identify(cmd::inspection::IdentifyArgs),
    /// Copy a dataset and every associated file, preserving encoded data.
    DatasetCopy(cmd::dataset_files::TransferArgs),
    /// Rename a dataset and its associated files; directory datasets include the complete subtree.
    DatasetRename(cmd::dataset_files::TransferArgs),
    /// Delete owned dataset files, preserving referenced VRT assets.
    /// Directory datasets include the complete subtree, including extra files.
    DatasetDelete(cmd::dataset_files::DeleteArgs),
    /// List the owned files of a dataset.
    DatasetFiles(cmd::dataset_files::FilesArgs),
    /// Create a Parquet metadata-only file from explicit partitions.
    ParquetCreateMetadata(cmd::parquet_metadata::Args),
    /// Read all exposed dataset content to detect decoding failures.
    Check(cmd::inspection::CheckArgs),
    /// Inspect native scientific groups, named dimensions and arrays without reading payloads.
    Mdinfo(cmd::multidim::InfoArgs),
    /// Read a native multidimensional hyperslab with named-axis slicing and transposition.
    Mdread(cmd::multidim::ReadArgs),
    /// Snapshot native numeric groups, coordinates and chunks between netCDF-4 and Zarr.
    Mdconvert(cmd::multidim::ConvertArgs),
    /// Mosaic multidimensional arrays by their dimension coordinates.
    Mdmosaic(cmd::multidim::MosaicArgs),
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
    /// One planar, two-dimensional centroid per feature, preserving attributes and CRS.
    #[command(visible_alias = "centroids")]
    Centroid(cmd::processing::GeometryArgs),
    /// Split multipart geometries and collections, preserving attributes and Z/M.
    #[command(visible_alias = "multipart-to-singlepart")]
    Singlepart(cmd::processing::GeometryArgs),
    /// Extract vertices as points, with source attributes and part/ring/vertex indices.
    #[command(visible_alias = "extract-vertices")]
    Vertices(cmd::processing::GeometryArgs),
    /// Exact valid-data footprint of a raster, with holes and disconnected regions.
    Footprint(cmd::processing::FootprintArgs),
    /// Compare two rasters: files, CRS, geotransform, sizes, bands, pixels, overviews, masks,
    /// metadata and subdatasets (like gdal raster compare and gdalcompare.py). The exit status is
    /// the number of differences.
    Compare(cmd::compare::Args),
    /// Query pixel values at pixel/line or georeferenced positions, from arguments, standard input
    /// or a vector dataset (like gdal raster pixel-info and gdallocationinfo).
    #[command(visible_alias = "locationinfo")]
    PixelInfo(cmd::pixel_info::Args),
    /// Check that a GeoTIFF is a Cloud Optimized GeoTIFF, with the checks and messages of
    /// rio-cogeo's cog_validate (GDAL's validate_cloud_optimized_geotiff.py): only headers and
    /// chunk offsets are read. The exit status is 1 when the file is not a valid COG.
    ValidateCog(cmd::validate_cog::Args),
    /// Write raster pixels as XYZ text: one line of coordinates and band values per pixel (like
    /// gdal2xyz.py).
    Xyz(cmd::xyz::Args),
    /// Sample one or more raster bands at points, transforming coordinates for each raster.
    #[command(visible_alias = "point-sample")]
    Sample(cmd::processing::SampleArgs),
    /// Contour lines from a raster band, with interval or fixed elevation levels.
    #[command(visible_alias = "contours")]
    Contour(cmd::processing::ContourArgs),
    /// Per-polygon statistics across multiple raster bands, with CRS transformations.
    #[command(visible_alias = "zonal")]
    ZonalStats(cmd::processing::ZonalArgs),
    /// Query vector layers with read-only SELECT, WHERE, ORDER BY and LIMIT.
    Sql(cmd::processing::SqlArgs),
    /// Planar buffer in input CRS units, with round caps and joins.
    Buffer(cmd::processing::BufferArgs),
    /// Overlay valid polygon layers in a common CRS, preserving both sets of attributes.
    Overlay(cmd::processing::OverlayArgs),
    /// Dissolve polygons across a layer or by selected attributes.
    Dissolve(cmd::processing::DissolveArgs),
    /// Report geometry validity and the first reason for each feature.
    #[command(visible_alias = "validity")]
    IsValid(cmd::processing::GeometryArgs),
    /// Repair invalid geometries, preserving attributes and reporting failures.
    MakeValid(cmd::processing::GeometryArgs),
    /// Snap, deduplicate, close rings, remove collapsed parts and repair polygon topology.
    Clean(cmd::processing::CleanArgs),
    /// Topology-preserving Douglas-Peucker simplification per feature (like gdal vector
    /// simplify and ogr2ogr -simplify).
    Simplify(cmd::vector_geom::SimplifyArgs),
    /// Add vertices so that no segment is longer than a length (like gdal vector segmentize).
    Segmentize(cmd::vector_geom::SegmentizeArgs),
    /// The convex hull of each feature's geometry (like gdal vector convex-hull).
    ConvexHull(cmd::vector_geom::HullArgs),
    /// The concave hull of each feature's geometry, of its polygons for polygons (like gdal
    /// vector concave-hull).
    ConcaveHull(cmd::vector_geom::ConcaveHullArgs),
    /// Swap the X and Y coordinates of every geometry (like gdal vector swap-xy).
    SwapXy(cmd::vector_geom::HullArgs),
    /// Change geometry types and dimensions: multi/single, a given type, XY/XYZ/XYM/XYZM (like
    /// gdal vector set-geom-type).
    SetGeomType(cmd::vector_geom::SetGeomTypeArgs),
    /// Combine geometries into collections, by groups of field values (like gdal vector combine).
    Combine(cmd::vector_geom::CombineArgs),
    /// Sort features spatially, along a Hilbert curve or an STR tree (like gdal vector sort).
    Sort(cmd::vector_geom::SortArgs),
    /// Concatenate vector layers and datasets into one or several layers (like gdal vector concat
    /// and ogrmerge.py).
    Concat(cmd::vector_dataset::ConcatArgs),
    /// Split a vector dataset into files by field values, Hive-style or flat (like gdal vector
    /// partition).
    Partition(cmd::vector_dataset::PartitionArgs),
    /// Change the type of vector fields, converting their values as GDAL does (like gdal vector
    /// set-field-type).
    SetFieldType(cmd::vector_dataset::SetFieldTypeArgs),
    /// Point geometries from coordinate fields (like gdal vector make-point).
    MakePoint(cmd::vector_dataset::MakePointArgs),
    /// Create a vector dataset or layer from an OGR_SCHEMA, a template dataset or explicit fields
    /// (like gdal vector create).
    Create(cmd::vector_dataset::CreateArgs),
    /// Print the OGR_SCHEMA JSON of a vector dataset (like gdal vector export-schema).
    ExportSchema(cmd::vector_dataset::ExportSchemaArgs),
    /// Rename vector layers, in place in GeoPackages or on output (like gdal vector
    /// rename-layer).
    RenameLayer(cmd::vector_dataset::RenameLayerArgs),
    /// Edit the CRS, geometry type and metadata of vector layers, in place in GeoPackages or on
    /// output (like gdal vector edit).
    Edit(cmd::vector_dataset::EditArgs),
    /// Update an existing vector dataset from another, matching features by FID or key fields
    /// (like gdal vector update).
    Update(cmd::vector_dataset::UpdateArgs),
    /// Subset, resize, rescale or change the type of a raster (like gdal_translate).
    Translate(cmd::translate::Args),
    /// Convert RGB Byte images to indexed color.
    RgbToPalette(cmd::palette::RgbArgs),
    /// Expand a palette-index image to RGB or RGBA.
    PaletteToRgb(cmd::palette::ExpandArgs),
    /// Attach a palette without changing index values.
    AttachPalette(cmd::palette::AttachArgs),
    /// Reproject and resample rasters (like gdalwarp).
    Warp(cmd::warp::Args),
    /// Read an indexed raster catalog, selecting tiles spatially and compositing in z-order.
    Catalog(cmd::catalog::Args),
    /// Create a raster footprint index, with optional GTI metadata.
    RasterIndex(cmd::tile_index::RasterArgs),
    /// Create a raster footprint catalog with GTI virtual-raster metadata.
    GtiCreate(cmd::tile_index::RasterArgs),
    /// Create a vector envelope index of vector dataset layers.
    VectorIndex(cmd::tile_index::VectorArgs),
    /// Mosaic rasters into one, or into a virtual raster (like gdalbuildvrt and gdal_merge).
    Mosaic(cmd::mosaic::Args),
    /// Terrain analysis: hillshade, slope, aspect, color relief, TRI, TPI, roughness (like gdaldem).
    Dem(cmd::dem::Args),
    /// Raster algebra: band math in NumPy syntax over one or more rasters (like gdal_calc.py).
    Calc(cmd::calc::Args),
    /// Distance from every pixel to the nearest target pixel (like gdal_proximity.py).
    Proximity(cmd::proximity::Args),
    /// Weighted Brovey pansharpening of panchromatic and spectral bands.
    Pansharpen(cmd::pansharpen::Args),
    /// Single-observer visibility or minimum visible height (like gdal_viewshed).
    Viewshed(cmd::viewshed::Args),
    /// Build, delete or refresh the overviews of a GeoTIFF in place or in its .ovr sidecar (like
    /// gdaladdo and gdal raster overview add/delete/refresh).
    #[command(visible_alias = "addo")]
    Overviews(cmd::overviews::Args),
    /// Edit a GeoTIFF in place: CRS, geotransform, nodata, metadata, statistics, histograms (like
    /// gdal raster edit and gdal_edit.py), or move its georeferencing to another CRS (gdalmove.py).
    EditRaster(cmd::edit::Args),
    /// Create a raster filled with burn values, from scratch or like a template (like gdal raster
    /// create and gdal_create).
    CreateRaster(cmd::create::Args),
    /// Write a raster into an existing GeoTIFF, reprojected to its grid (like gdal raster update).
    UpdateRaster(cmd::update::Args),
    /// Merge raster areas smaller than a threshold into their largest neighbour (like gdal_sieve.py).
    Sieve(cmd::sieve::Args),
    /// Fill raster holes by interpolation, as gdal_fillnodata.
    FillNodata(cmd::fill_nodata::Args),
    /// Apply band scales and offsets to get physical values (like gdal raster unscale).
    Unscale(cmd::pixel::UnscaleArgs),
    /// Blend two Byte images with alpha and opacity.
    Blend(cmd::pixel::BlendArgs),
    /// Replace nodata values with an alpha band (like gdal raster nodata-to-alpha).
    NodataToAlpha(cmd::pixel::NodataToAlphaArgs),
    /// Map values or ranges of values to new values (like gdal raster reclassify).
    Reclassify(cmd::pixel::ReclassifyArgs),
    /// One vector feature per pixel, with band values as fields (like gdal raster as-features).
    AsFeatures(cmd::pixel::AsFeaturesArgs),
    /// Focal statistics and convolution kernels over each pixel's neighbourhood (like
    /// gdal raster neighbors).
    #[command(visible_alias = "focal")]
    Neighbors(cmd::neighbors::Args),
    /// Set the noisy, nearly black or white collar of an image to exact black or white,
    /// optionally transparent, by flood fill from the edges (like gdal raster clean-collar).
    CleanCollar(cmd::clean_collar::Args),
    /// Set nearly black or white borders to exact black or white, by two scan passes (like
    /// nearblack; clean-collar with nearblack's defaults).
    Nearblack(cmd::clean_collar::Args),
    /// Contrast enhancement by lookup tables: histogram equalization and LUT files (like
    /// gdalenhance).
    Enhance(cmd::enhance::Args),
    /// Grid a point cloud into a raster: min, max, mean, IDW, count or standard deviation per cell
    /// (like PDAL's writers.gdal), or a TIN or nearest-neighbour interpolation (like gdal_grid).
    /// For vector points, see vector-grid.
    Grid(cmd::grid::Args),
    /// Grid scattered vector points into a raster: inverse distance, nearest neighbour, moving
    /// average, linear (Delaunay) or data metrics in a search ellipse (like gdal vector grid and
    /// gdal_grid).
    VectorGrid(cmd::vector_grid::Args),
    /// Linear referencing (like ogrlineref): cut a route into parts from reference points, and
    /// convert between positions along it and coordinates.
    Lineref(cmd::lineref::Args),
    /// Filter a point cloud as PDAL's filters do: ground classification (SMRF, PMF, CSF), noise,
    /// heights above ground, crops, expressions, assignments, tiles, normals, clusters, sampling,
    /// covariance features, outlier factors, raster values and polygon attributes.
    Filter(cmd::filter::Args),
    /// Manage files on local, in-memory (/vsimem/), cloud (S3, GCS, Azure), HTTP and archive
    /// (zip, tar) locations: list, copy, delete, move, sync (like `gdal vsi`).
    Vsi(cmd::vsi::Args),
    /// Serve these commands to AI agents (Claude Code, Claude Desktop, Cursor, VS Code…) as a
    /// Model Context Protocol server over standard input and output.
    Mcp,
}

/// Runs the command line `args` (the program name first) and returns the process exit code: 0 on
/// success, 1 when the command fails, 2 for a usage error (as clap reports it). `compare` exits
/// with its difference count and `pixel-info` (text and value outputs) with 1 for positions off
/// the raster, as GDAL's tools do.
pub fn run(args: Vec<OsString>) -> i32 {
    run_as(args, None)
}

/// [`run`], where `terrio` is the command line that runs terrio (a Python interpreter with
/// `-m terrio`, for the Python package), for `terrio mcp` to run its tools with. `None` is this
/// executable.
pub fn run_as(args: Vec<OsString>, terrio: Option<Vec<OsString>>) -> i32 {
    // On a thread of its own with Linux's 8 MiB stack: Windows gives its main thread 1 MiB, which
    // the debug build's deep frames (clap's parser, `terrio mcp`) can overflow.
    // Should the thread not start (its stack refused under a memory limit), the command runs here.
    let job = Arc::new(Mutex::new(Some((args, terrio))));
    let take = move |job: &Mutex<Option<_>>| {
        let job = job.lock().unwrap().take();
        job.map(|(args, terrio)| run_on_this_thread(args, terrio))
    };
    let on_thread = Arc::clone(&job);
    match std::thread::Builder::new().name("terrio".into()).stack_size(STACK_SIZE).spawn(move || take(&on_thread)) {
        Ok(t) => t.join().map_or(101, |code| code.unwrap_or(1)),
        Err(_) => take(&job).unwrap_or(1),
    }
}

/// The stack of the thread that runs a command: Linux's default for its main thread.
const STACK_SIZE: usize = 8 << 20;

/// [`run_as`], on the calling thread.
fn run_on_this_thread(args: Vec<OsString>, terrio: Option<Vec<OsString>>) -> i32 {
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
        Command::GpkgRepack(a) => cmd::gpkg::repack(a),
        Command::Identify(a) => cmd::inspection::identify(a),
        Command::DatasetCopy(a) => cmd::dataset_files::transfer(a, false),
        Command::DatasetRename(a) => cmd::dataset_files::transfer(a, true),
        Command::DatasetDelete(a) => cmd::dataset_files::delete(a),
        Command::DatasetFiles(a) => cmd::dataset_files::files(a),
        Command::ParquetCreateMetadata(a) => cmd::parquet_metadata::run(a),
        Command::Check(a) => cmd::inspection::check(a),
        Command::Mdinfo(a) => cmd::multidim::info(a),
        Command::Mdread(a) => cmd::multidim::read(a),
        Command::Mdconvert(a) => cmd::multidim::convert(a),
        Command::Mdmosaic(a) => cmd::multidim::mosaic(a),
        Command::Convert(a) => cmd::convert::run(a),
        Command::Formats(a) => cmd::formats::run(a),
        Command::Crs(a) => cmd::crs::run(a),
        Command::Rasterize(a) => cmd::rasterize::run(a),
        Command::Polygonize(a) => cmd::polygonize::run(a),
        Command::Centroid(a) => cmd::processing::centroid(a),
        Command::Singlepart(a) => cmd::processing::singlepart(a),
        Command::Vertices(a) => cmd::processing::vertices(a),
        Command::Footprint(a) => cmd::processing::footprint(a),
        Command::Compare(a) => cmd::compare::run(a),
        Command::PixelInfo(a) => cmd::pixel_info::run(a),
        Command::ValidateCog(a) => cmd::validate_cog::run(a),
        Command::Xyz(a) => cmd::xyz::run(a),
        Command::Sample(a) => cmd::processing::sample(a),
        Command::Contour(a) => cmd::processing::contour(a),
        Command::ZonalStats(a) => cmd::processing::zonal(a),
        Command::Sql(a) => cmd::processing::sql(a),
        Command::Buffer(a) => cmd::processing::buffer(a),
        Command::Overlay(a) => cmd::processing::overlay(a),
        Command::Dissolve(a) => cmd::processing::dissolve(a),
        Command::IsValid(a) => cmd::processing::validity(a),
        Command::MakeValid(a) => cmd::processing::make_valid(a),
        Command::Clean(a) => cmd::processing::clean(a),
        Command::Simplify(a) => cmd::vector_geom::simplify(a),
        Command::Segmentize(a) => cmd::vector_geom::segmentize(a),
        Command::ConvexHull(a) => cmd::vector_geom::convex_hull(a),
        Command::ConcaveHull(a) => cmd::vector_geom::concave_hull(a),
        Command::SwapXy(a) => cmd::vector_geom::swap_xy(a),
        Command::SetGeomType(a) => cmd::vector_geom::set_geom_type(a),
        Command::Combine(a) => cmd::vector_geom::combine(a),
        Command::Sort(a) => cmd::vector_geom::sort(a),
        Command::Concat(a) => cmd::vector_dataset::concat(a),
        Command::Partition(a) => cmd::vector_dataset::partition(a),
        Command::SetFieldType(a) => cmd::vector_dataset::set_field_type(a),
        Command::MakePoint(a) => cmd::vector_dataset::make_point(a),
        Command::Create(a) => cmd::vector_dataset::create(a),
        Command::ExportSchema(a) => cmd::vector_dataset::export_schema(a),
        Command::RenameLayer(a) => cmd::vector_dataset::rename_layer(a),
        Command::Edit(a) => cmd::vector_dataset::edit(a),
        Command::Update(a) => cmd::vector_dataset::update(a),
        Command::Translate(a) => cmd::translate::run(a),
        Command::RgbToPalette(a) => cmd::palette::rgb(a),
        Command::PaletteToRgb(a) => cmd::palette::expand(a),
        Command::AttachPalette(a) => cmd::palette::attach(a),
        Command::Warp(a) => cmd::warp::run(a),
        Command::Catalog(a) => cmd::catalog::run(a),
        Command::RasterIndex(a) | Command::GtiCreate(a) => cmd::tile_index::raster(a),
        Command::VectorIndex(a) => cmd::tile_index::vector(a),
        Command::Mosaic(a) => cmd::mosaic::run(a),
        Command::Dem(a) => cmd::dem::run(a),
        Command::Calc(a) => cmd::calc::run(a),
        Command::Proximity(a) => cmd::proximity::run(a),
        Command::Pansharpen(a) => cmd::pansharpen::run(a),
        Command::Viewshed(a) => cmd::viewshed::run(a),
        Command::Overviews(a) => cmd::overviews::run(a),
        Command::EditRaster(a) => cmd::edit::run(a),
        Command::CreateRaster(a) => cmd::create::run(a),
        Command::UpdateRaster(a) => cmd::update::run(a),
        Command::Sieve(a) => cmd::sieve::run(a),
        Command::FillNodata(a) => cmd::fill_nodata::run(a),
        Command::Unscale(a) => cmd::pixel::unscale(a),
        Command::Blend(a) => cmd::pixel::blend(a),
        Command::NodataToAlpha(a) => cmd::pixel::nodata_to_alpha(a),
        Command::Reclassify(a) => cmd::pixel::reclassify(a),
        Command::AsFeatures(a) => cmd::pixel::as_features(a),
        Command::Neighbors(a) => cmd::neighbors::run(a),
        Command::CleanCollar(a) => cmd::clean_collar::run(a, true),
        Command::Nearblack(a) => cmd::clean_collar::run(a, false),
        Command::Enhance(a) => cmd::enhance::run(a),
        Command::Grid(a) => cmd::grid::run(a),
        Command::VectorGrid(a) => cmd::vector_grid::run(a),
        Command::Lineref(a) => cmd::lineref::run(a),
        Command::Filter(a) => cmd::filter::run(a),
        Command::Vsi(a) => cmd::vsi::run(a),
        Command::Mcp => serve_mcp(terrio),
    };
    for w in terrio::warnings::take() {
        eprintln!("terrio: warning: {w}");
    }
    match result {
        Ok(()) => {
            let code = cmd::take_exit_code();
            if code != 0 && std::env::var_os("TERRIO_MCP_TOOL").is_some() {
                // MCP child status distinguishes execution failure from a successful query
                // reporting differences or off-raster positions. Keep GDAL CLI exit codes.
                eprintln!("(terrio tool exited with status {code})");
                0
            } else {
                code
            }
        }
        Err(e) => {
            cmd::take_exit_code();
            eprintln!("terrio: {e}");
            1
        }
    }
}

/// `terrio mcp`, running its tools with `terrio`, or this executable.
fn serve_mcp(terrio: Option<Vec<OsString>>) -> terrio::Result<()> {
    let terrio = match terrio {
        Some(t) => t,
        None => mcp::this_binary()?,
    };
    Ok(mcp::serve(terrio)?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_command_line_is_well_formed() {
        // Clap's checks: unique argument names across flattened option groups, among others.
        // Use the production CLI worker's stack, including on Windows test threads.
        std::thread::Builder::new()
            .stack_size(super::STACK_SIZE)
            .spawn(|| <super::Cli as clap::CommandFactory>::command().debug_assert())
            .unwrap()
            .join()
            .unwrap();
    }
}
