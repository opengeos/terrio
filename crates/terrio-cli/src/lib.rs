// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/index.rst
// No code is translated from these sources.

//! The `terrio` command-line tool, as a library: the `terrio` binary and the `terrio` command of
//! the Python package both run [`run`].

mod cmd;
mod mcp;
mod usage;

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

/// Pure-Rust geospatial raster, vector and point cloud data I/O.
#[derive(Parser)]
#[command(name = "terrio", version, about)]
struct Cli {
    /// Print the versioned command catalog as JSON, without opening datasets.
    #[arg(id = "json_usage", long = "json-usage", global = true)]
    _json_usage: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[command(next_display_order = None)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Summarize a raster, vector dataset or point cloud.
    Info(cmd::info::Args),
    /// Create an A.TOC index from existing CADRG frames.
    ///
    /// The index is metadata-only.
    RpftocCreate(cmd::rpftoc::Args),
    /// Repack a GeoPackage in place.
    ///
    /// Preserves all encoded records and schema objects.
    GpkgRepack(cmd::gpkg::RepackArgs),
    /// Validate a self-contained GeoPackage.
    ///
    /// Checks core and extension requirements.
    GpkgValidate(cmd::gpkg::ValidateArgs),
    /// Identify many datasets from their headers.
    ///
    /// Reads bounded headers, without decoding payloads.
    Identify(cmd::inspection::IdentifyArgs),
    /// Copy a dataset and every associated file.
    ///
    /// Preserves encoded data.
    DatasetCopy(cmd::dataset_files::TransferArgs),
    /// Rename a dataset and its associated files.
    ///
    /// Directory datasets include the complete subtree.
    DatasetRename(cmd::dataset_files::TransferArgs),
    /// Delete a dataset's owned files.
    ///
    /// Preserves referenced VRT assets. Directory datasets include the complete subtree, including
    /// extra files.
    DatasetDelete(cmd::dataset_files::DeleteArgs),
    /// List the owned files of a dataset.
    DatasetFiles(cmd::dataset_files::FilesArgs),
    /// Create a metadata-only Parquet file.
    ///
    /// Built from explicit partitions.
    ParquetCreateMetadata(cmd::parquet_metadata::Args),
    /// Read all exposed content to detect decoding failures.
    Check(cmd::inspection::CheckArgs),
    /// Inspect native scientific groups, dimensions and arrays.
    ///
    /// Reports named dimensions, without reading payloads.
    Mdinfo(cmd::multidim::InfoArgs),
    /// Read a native multidimensional hyperslab.
    ///
    /// Supports named-axis slicing and transposition.
    Mdread(cmd::multidim::ReadArgs),
    /// Convert native numeric data between netCDF-4 and Zarr.
    ///
    /// Snapshots groups, coordinates and chunks.
    Mdconvert(cmd::multidim::ConvertArgs),
    /// Mosaic multidimensional arrays by dimension coordinates.
    Mdmosaic(cmd::multidim::MosaicArgs),
    /// Convert a dataset to another format.
    ///
    /// Optionally reprojects it. Point clouds convert to LAS, LAZ, COPC or vector point layers.
    Convert(cmd::convert::Args),
    /// List the formats terrio reads and writes.
    Formats(cmd::formats::Args),
    /// Describe a CRS, or transform coordinates between two.
    Crs(cmd::crs::Args),
    /// Burn vector geometries into a raster.
    Rasterize(cmd::rasterize::Args),
    /// Turn a raster band into polygons.
    ///
    /// Each polygon covers connected pixels with the same value.
    Polygonize(cmd::polygonize::Args),
    /// Compute one centroid per feature.
    ///
    /// Planar and two-dimensional, preserving attributes and CRS.
    #[command(visible_alias = "centroids")]
    Centroid(cmd::processing::GeometryArgs),
    /// Split multipart geometries and collections.
    ///
    /// Preserves attributes and Z/M.
    #[command(visible_alias = "multipart-to-singlepart")]
    Singlepart(cmd::processing::GeometryArgs),
    /// Extract vertices as points.
    ///
    /// Keeps source attributes and adds part, ring and vertex indices.
    #[command(visible_alias = "extract-vertices")]
    Vertices(cmd::processing::GeometryArgs),
    /// Compute the exact valid-data footprint of a raster.
    ///
    /// Includes holes and disconnected regions.
    Footprint(cmd::processing::FootprintArgs),
    /// Compare two rasters.
    ///
    /// Compares files, CRS, geotransform, sizes, bands, pixels, overviews, masks, metadata and
    /// subdatasets (like gdal raster compare and gdalcompare.py). The exit status is the number of
    /// differences.
    Compare(cmd::compare::Args),
    /// Query pixel values at given positions.
    ///
    /// Positions are pixel/line or georeferenced, from arguments, standard input or a vector
    /// dataset (like gdal raster pixel-info and gdallocationinfo).
    #[command(visible_alias = "locationinfo")]
    PixelInfo(cmd::pixel_info::Args),
    /// Check that a GeoTIFF is a Cloud Optimized GeoTIFF.
    ///
    /// Uses the checks and messages of rio-cogeo's cog_validate (GDAL's
    /// validate_cloud_optimized_geotiff.py): only headers and chunk offsets are read. The exit
    /// status is 1 when the file is not a valid COG.
    ValidateCog(cmd::validate_cog::Args),
    /// Write raster pixels as XYZ text (like gdal2xyz.py).
    ///
    /// Writes one line of coordinates and band values per pixel.
    Xyz(cmd::xyz::Args),
    /// Sample raster bands at points.
    ///
    /// Samples one or more bands, transforming coordinates for each raster.
    #[command(visible_alias = "point-sample")]
    Sample(cmd::processing::SampleArgs),
    /// Trace contour lines from a raster band.
    ///
    /// Uses interval or fixed elevation levels.
    #[command(visible_alias = "contours")]
    Contour(cmd::processing::ContourArgs),
    /// Compute per-polygon statistics of raster bands.
    ///
    /// Handles multiple bands and CRS transformations.
    #[command(visible_alias = "zonal")]
    ZonalStats(cmd::processing::ZonalArgs),
    /// Query vector layers with read-only SQL.
    ///
    /// Supports SELECT, WHERE, ORDER BY and LIMIT.
    Sql(cmd::processing::SqlArgs),
    /// Planar buffer in input CRS units, with round caps and joins.
    Buffer(cmd::processing::BufferArgs),
    /// Clip features to a bbox, geometry or template.
    ///
    /// Clips points, lines and polygons; a template is a polygon or raster dataset.
    VectorClip(cmd::vector_clip::Args),
    /// Overlay valid polygon layers.
    ///
    /// Works in a common CRS, preserving both sets of attributes.
    Overlay(cmd::processing::OverlayArgs),
    /// Polygon, line and point layer algebra (seven operations).
    LayerAlgebra(cmd::processing::LayerAlgebraArgs),
    /// Chain raster and vector operations.
    ///
    /// Runs without serialized intermediate files; loads and saves GDALG.
    Pipeline(cmd::pipeline::Args),
    /// Dissolve polygons across a layer or by selected attributes.
    Dissolve(cmd::processing::DissolveArgs),
    /// Report the validity of each feature's geometry.
    ///
    /// Includes the first reason for each feature.
    #[command(visible_alias = "validity")]
    IsValid(cmd::processing::GeometryArgs),
    /// Repair invalid geometries.
    ///
    /// Preserves attributes and reports failures.
    MakeValid(cmd::processing::GeometryArgs),
    /// Find errors in polygon coverages.
    ///
    /// Finds overlapping interiors, unmatched shared nodes and narrow gaps.
    CheckCoverage(cmd::processing::CheckCoverageArgs),
    /// Simplify shared polygon coverage boundaries.
    ///
    /// Preserves topology.
    SimplifyCoverage(cmd::processing::SimplifyCoverageArgs),
    /// Repair polygon coverages.
    ///
    /// Snaps shared boundaries and removes overlaps and enclosed narrow gaps.
    CleanCoverage(cmd::processing::CleanCoverageArgs),
    /// Clean up vector geometries.
    ///
    /// Snaps, deduplicates, closes rings, removes collapsed parts and repairs polygon topology.
    Clean(cmd::processing::CleanArgs),
    /// Simplify features with Douglas-Peucker.
    ///
    /// Topology-preserving, per feature (like gdal vector simplify and ogr2ogr -simplify).
    Simplify(cmd::vector_geom::SimplifyArgs),
    /// Add vertices to limit segment length.
    ///
    /// No segment stays longer than a given length (like gdal vector segmentize).
    Segmentize(cmd::vector_geom::SegmentizeArgs),
    /// Compute the convex hull of each feature.
    ///
    /// Like gdal vector convex-hull.
    ConvexHull(cmd::vector_geom::HullArgs),
    /// Compute the concave hull of each feature.
    ///
    /// For polygons, the hull is of the feature's polygons (like gdal vector concave-hull).
    ConcaveHull(cmd::vector_geom::ConcaveHullArgs),
    /// Swap the X and Y coordinates of every geometry.
    ///
    /// Like gdal vector swap-xy.
    SwapXy(cmd::vector_geom::HullArgs),
    /// Change geometry types and dimensions.
    ///
    /// Converts multi/single, to a given type, or XY/XYZ/XYM/XYZM (like gdal vector set-geom-type).
    SetGeomType(cmd::vector_geom::SetGeomTypeArgs),
    /// Combine geometries into collections.
    ///
    /// Groups by field values (like gdal vector combine).
    Combine(cmd::vector_geom::CombineArgs),
    /// Sort features spatially.
    ///
    /// Sorts along a Hilbert curve or an STR tree (like gdal vector sort).
    Sort(cmd::vector_geom::SortArgs),
    /// Concatenate vector layers and datasets.
    ///
    /// Writes one or several layers (like gdal vector concat and ogrmerge.py).
    Concat(cmd::vector_dataset::ConcatArgs),
    /// Split a vector dataset into files by field values.
    ///
    /// Writes Hive-style or flat (like gdal vector partition).
    Partition(cmd::vector_dataset::PartitionArgs),
    /// Change the type of vector fields.
    ///
    /// Converts their values as GDAL does (like gdal vector set-field-type).
    SetFieldType(cmd::vector_dataset::SetFieldTypeArgs),
    /// Make point geometries from coordinate fields.
    ///
    /// Like gdal vector make-point.
    MakePoint(cmd::vector_dataset::MakePointArgs),
    /// Create a vector dataset or layer.
    ///
    /// Starts from an OGR_SCHEMA, a template dataset or explicit fields (like gdal vector create).
    Create(cmd::vector_dataset::CreateArgs),
    /// Print the OGR_SCHEMA JSON of a vector dataset.
    ///
    /// Like gdal vector export-schema.
    ExportSchema(cmd::vector_dataset::ExportSchemaArgs),
    /// Rename vector layers.
    ///
    /// Works in place in GeoPackages or on output (like gdal vector rename-layer).
    RenameLayer(cmd::vector_dataset::RenameLayerArgs),
    /// Edit the CRS, geometry type and metadata of vector layers.
    ///
    /// Works in place in GeoPackages or on output (like gdal vector edit).
    Edit(cmd::vector_dataset::EditArgs),
    /// Update a vector dataset from another.
    ///
    /// Matches features by FID or key fields (like gdal vector update).
    Update(cmd::vector_dataset::UpdateArgs),
    /// Subset, resize, rescale or change the type of a raster.
    ///
    /// Like gdal_translate.
    Translate(cmd::translate::Args),
    /// Convert RGB Byte images to indexed color.
    RgbToPalette(cmd::palette::RgbArgs),
    /// Expand a palette-index image to RGB or RGBA.
    PaletteToRgb(cmd::palette::ExpandArgs),
    /// Attach a palette without changing index values.
    AttachPalette(cmd::palette::AttachArgs),
    /// Reproject and resample rasters (like gdalwarp).
    Warp(cmd::warp::Args),
    /// Read an indexed raster catalog.
    ///
    /// Selects tiles spatially and composites them in z-order.
    Catalog(cmd::catalog::Args),
    /// Create a raster footprint index, with optional GTI metadata.
    RasterIndex(cmd::tile_index::RasterArgs),
    /// Generate separate XYZ/TMS raster tiles in a directory.
    Tile(cmd::raster_tile::Args),
    /// Retile mosaics into overlapping files and pyramids.
    ///
    /// Builds recursive pyramid levels.
    Retile(cmd::raster_retile::Args),
    /// Create a virtual-raster footprint catalog (GTI).
    GtiCreate(cmd::tile_index::RasterArgs),
    /// Create a vector envelope index of vector dataset layers.
    VectorIndex(cmd::tile_index::VectorArgs),
    /// Mosaic rasters into one, or into a virtual raster.
    ///
    /// Like gdalbuildvrt and gdal_merge.
    Mosaic(cmd::mosaic::Args),
    /// Terrain analysis: hillshade, slope, aspect and more.
    ///
    /// Computes hillshade, slope, aspect, color relief, TRI, TPI and roughness (like gdaldem).
    Dem(cmd::dem::Args),
    /// Raster algebra: band math in NumPy syntax.
    ///
    /// Works over one or more rasters (like gdal_calc.py).
    Calc(cmd::calc::Args),
    /// Compute the distance to the nearest target pixel.
    ///
    /// Computed for every pixel (like gdal_proximity.py).
    Proximity(cmd::proximity::Args),
    /// Weighted Brovey pansharpening.
    ///
    /// Combines panchromatic and spectral bands.
    Pansharpen(cmd::pansharpen::Args),
    /// Single-observer visibility or minimum visible height.
    ///
    /// Like gdal_viewshed.
    Viewshed(cmd::viewshed::Args),
    /// Build, delete or refresh GeoTIFF overviews.
    ///
    /// Works in place or in a .ovr sidecar (like gdaladdo and gdal raster overview
    /// add/delete/refresh).
    #[command(visible_alias = "addo")]
    Overviews(cmd::overviews::Args),
    /// Edit a GeoTIFF in place.
    ///
    /// Edits CRS, geotransform, nodata, metadata, statistics and histograms (like gdal raster edit
    /// and gdal_edit.py), or moves its georeferencing to another CRS (gdalmove.py).
    EditRaster(cmd::edit::Args),
    /// Create a raster filled with burn values.
    ///
    /// Starts from scratch or like a template (like gdal raster create and gdal_create).
    CreateRaster(cmd::create::Args),
    /// Write a raster into an existing GeoTIFF.
    ///
    /// Reprojects it to the GeoTIFF's grid (like gdal raster update).
    UpdateRaster(cmd::update::Args),
    /// Merge small raster areas into their largest neighbour.
    ///
    /// Merges areas smaller than a threshold (like gdal_sieve.py).
    Sieve(cmd::sieve::Args),
    /// Fill raster holes by interpolation, as gdal_fillnodata.
    FillNodata(cmd::fill_nodata::Args),
    /// Apply band scales and offsets.
    ///
    /// Produces physical values (like gdal raster unscale).
    Unscale(cmd::pixel::UnscaleArgs),
    /// Blend two Byte images with alpha and opacity.
    Blend(cmd::pixel::BlendArgs),
    /// Replace nodata values with an alpha band.
    ///
    /// Like gdal raster nodata-to-alpha.
    NodataToAlpha(cmd::pixel::NodataToAlphaArgs),
    /// Map values or ranges of values to new values.
    ///
    /// Like gdal raster reclassify.
    Reclassify(cmd::pixel::ReclassifyArgs),
    /// Write one vector feature per pixel.
    ///
    /// Band values become fields (like gdal raster as-features).
    AsFeatures(cmd::pixel::AsFeaturesArgs),
    /// Focal statistics and convolution kernels.
    ///
    /// Computed over each pixel's neighbourhood (like gdal raster neighbors).
    #[command(visible_alias = "focal")]
    Neighbors(cmd::neighbors::Args),
    /// Clean the noisy collar of an image.
    ///
    /// Sets a nearly black or white collar to exact black or white, optionally transparent, by
    /// flood fill from the edges (like gdal raster clean-collar).
    CleanCollar(cmd::clean_collar::Args),
    /// Set nearly black or white borders to exact values.
    ///
    /// Sets them to exact black or white by two scan passes (like nearblack; clean-collar with
    /// nearblack's defaults).
    Nearblack(cmd::clean_collar::Args),
    /// Enhance contrast by lookup tables.
    ///
    /// Uses histogram equalization and LUT files (like gdalenhance).
    Enhance(cmd::enhance::Args),
    /// Grid a point cloud into a raster.
    ///
    /// Computes min, max, mean, IDW, count or standard deviation per cell (like PDAL's
    /// writers.gdal), or a TIN or nearest-neighbour interpolation (like gdal_grid). For vector
    /// points, see vector-grid.
    Grid(cmd::grid::Args),
    /// Grid scattered vector points into a raster.
    ///
    /// Uses inverse distance, nearest neighbour, moving average, linear (Delaunay) or data metrics
    /// in a search ellipse (like gdal vector grid and gdal_grid).
    VectorGrid(cmd::vector_grid::Args),
    /// Linear referencing (like ogrlineref).
    ///
    /// Cuts a route into parts from reference points, and converts between positions along it and
    /// coordinates.
    Lineref(cmd::lineref::Args),
    /// Filter a point cloud as PDAL's filters do.
    ///
    /// Covers ground classification (SMRF, PMF, CSF), noise, heights above ground, crops,
    /// expressions, assignments, tiles, normals, clusters, sampling, covariance features, outlier
    /// factors, raster values and polygon attributes.
    Filter(cmd::filter::Args),
    /// Manage local, in-memory, cloud and archive files.
    ///
    /// Works on local, in-memory (/vsimem/), cloud (S3, GCS, Azure), HTTP and archive (zip, tar)
    /// locations: list, copy, delete, move, sync (like `gdal vsi`).
    Vsi(cmd::vsi::Args),
    /// Serve these commands to AI agents over MCP.
    ///
    /// Runs a Model Context Protocol server over standard input and output, for Claude Code, Claude
    /// Desktop, Cursor, VS Code…
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
    let args = cmd::calc::rewrite_args(args);
    if let Some(catalog) = usage::requested(&args) {
        println!("{}", serde_json::to_string_pretty(&catalog).expect("JSON command catalog"));
        return 0;
    }
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // --help and --version print to standard output and succeed; usage errors do not.
            let _ = e.print();
            return e.exit_code();
        }
    };
    if cli._json_usage {
        eprintln!("JSON usage is not available for this command");
        return 2;
    }
    let result = match cli.command {
        Command::Info(a) => cmd::info::run(a),
        Command::RpftocCreate(a) => cmd::rpftoc::run(&a),
        Command::GpkgRepack(a) => cmd::gpkg::repack(a),
        Command::GpkgValidate(a) => cmd::gpkg::validate(a),
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
        Command::VectorClip(a) => cmd::vector_clip::run(a),
        Command::Overlay(a) => cmd::processing::overlay(a),
        Command::LayerAlgebra(a) => cmd::processing::layer_algebra(a),
        Command::Pipeline(a) => cmd::pipeline::run(a),
        Command::Dissolve(a) => cmd::processing::dissolve(a),
        Command::IsValid(a) => cmd::processing::validity(a),
        Command::MakeValid(a) => cmd::processing::make_valid(a),
        Command::CheckCoverage(a) => cmd::processing::check_coverage(a),
        Command::SimplifyCoverage(a) => cmd::processing::simplify_coverage(a),
        Command::CleanCoverage(a) => cmd::processing::clean_coverage(a),
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
        Command::Tile(a) => cmd::raster_tile::run(&a),
        Command::Retile(a) => cmd::raster_retile::run(a),
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

    /// The names a command's `--help` lists under "Commands:", in the order shown.
    fn listed_subcommands(help: &str) -> Vec<&str> {
        help.lines()
            .skip_while(|line| *line != "Commands:")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
            .filter_map(|line| line.split_whitespace().next())
            .collect()
    }

    /// The longest summary a command list may show, to keep each entry on one line of a terminal.
    const MAX_SUMMARY_CHARS: usize = 60;

    fn assert_help_sorted_and_brief(command: &mut clap::Command) {
        let help = command.render_help().to_string();
        let names = listed_subcommands(&help);
        assert_eq!(names.len(), command.get_subcommands().filter(|c| !c.is_hide_set()).count(), "{help}");
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "subcommands of `{}`", command.get_name());
        for subcommand in command.get_subcommands() {
            let summary = subcommand.get_about().map(ToString::to_string).unwrap_or_default();
            assert!(!summary.is_empty() && summary.chars().count() <= MAX_SUMMARY_CHARS, "`{}`: {summary}", subcommand.get_name());
        }
        // Clap generates each `help` command's list in declaration order, without the enum's setting.
        command.get_subcommands_mut().filter(|c| c.get_name() != "help").for_each(assert_help_sorted_and_brief);
    }

    #[test]
    fn help_lists_subcommands_alphabetically_with_brief_summaries() {
        std::thread::Builder::new()
            .stack_size(super::STACK_SIZE)
            .spawn(|| {
                let mut command = <super::Cli as clap::CommandFactory>::command();
                command.build();
                assert_help_sorted_and_brief(&mut command);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
