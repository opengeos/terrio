//! `terrio grid`: points to a raster, like PDAL's `writers.gdal`, or interpolated like GDAL's
//! `gdal_grid`.

use terrio::pointcloud::grid::{GridMethod, GridOptions, GridStat};

#[derive(clap::Args)]
pub struct Args {
    /// The point cloud (LAS, LAZ, COPC, EPT, PLY or E57; a path, URL or `s3://` object).
    input: String,
    /// The output raster.
    output: String,
    /// The cell size.
    #[arg(long, value_name = "RES")]
    tr: f64,
    /// How cells are computed: statistics of the points near each cell, as PDAL's writers.gdal
    /// (the default; the points are read in chunks, in bounded memory); linear, interpolated in
    /// the points' Delaunay triangulation (a TIN), as gdal_grid -a linear; or nearest, the
    /// nearest point's value, as gdal_grid -a nearest.
    #[arg(long, value_name = "METHOD", default_value = "statistics")]
    method: String,
    /// The statistics, one band each (comma-separated): min, max, mean, idw, count, stdev, or all.
    /// Bands are in that order.
    #[arg(long, value_name = "STATS", default_value = "mean")]
    stat: String,
    /// The dimension gridded.
    #[arg(long = "dim", value_name = "DIMENSION", default_value = "z")]
    dimension: String,
    /// Statistics: points closer than this to a cell centre count towards it (by default the
    /// cell diagonal). Linear: the search radius of the nearest point outside the triangulation
    /// (0: nodata there). Nearest: the half-side of the square searched (by default, all
    /// points).
    #[arg(long, allow_negative_numbers = true)]
    radius: Option<f64>,
    /// The power of the inverse distance weights.
    #[arg(long, default_value_t = 1.0)]
    power: f64,
    /// Fill empty cells from the cells within this many cells.
    #[arg(long = "window-size", default_value_t = 0)]
    window_size: usize,
    /// Count each point in the cell it falls in only, ignoring the radius.
    #[arg(long)]
    bin: bool,
    /// The grid: the first cell's corner (XMIN, YMIN), and floor((MAX - MIN) / RES) + 1 cells
    /// each way, as in PDAL. By default, the points' bounds.
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true)]
    te: Option<Vec<f64>>,
    /// The nodata value.
    #[arg(long = "a-nodata", value_name = "VALUE", default_value_t = -9999.0, allow_negative_numbers = true)]
    nodata: f64,
    /// The output data type (Byte, Int16, UInt16, Int32, Float32, Float64...).
    #[arg(long, value_name = "TYPE")]
    ot: Option<String>,
    /// Assign this CRS to the points (and the raster), when the file has none or a wrong one.
    #[arg(long = "a-srs", value_name = "CRS")]
    a_srs: Option<String>,
    #[command(flatten)]
    points: super::points::PointArgs,
    #[command(flatten)]
    out: super::rasterout::OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let options = GridOptions {
        resolution: a.tr,
        method: GridMethod::parse(&a.method)?,
        stats: GridStat::parse_list(&a.stat)?,
        dimension: a.dimension,
        radius: a.radius,
        power: a.power,
        window_size: a.window_size,
        bin_mode: a.bin,
        bounds: a.te.as_deref().map(|b| [b[0], b[1], b[2], b[3]]),
        nodata: a.nodata,
        data_type: a.ot.as_deref().map(terrio::ops::parse_data_type).transpose()?.unwrap_or(terrio::DataType::F64),
    };
    let mut reader = terrio::pointcloud::open_points(&a.input)?;
    let srs = super::rasterout::srs(&a.a_srs)?;
    let ds = terrio::pointcloud::grid::grid_reader(&mut reader, &a.points.to_options()?, &options, srs.as_ref())?;
    a.out.write(&ds, &a.output)
}
