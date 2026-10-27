//! `terrio vector-grid`: scattered vector points to a raster, like `gdal vector grid` and
//! `gdal_grid`.

use terrio::VectorOpenOptions;
use terrio::crs::SpatialRef;
use terrio::ops::vector_grid::{GridAlgorithm, GridParams, VectorGridOptions};

#[derive(clap::Args)]
pub struct Args {
    /// The algorithm: invdist, invdistnn, average, nearest, linear, minimum, maximum, range,
    /// count, average-distance or average-distance-points. A GDAL algorithm string
    /// (`invdist:power=3:radius1=500:radius2=500`) is accepted too; options override it.
    algorithm: String,
    /// The vector dataset of points (the vertices of lines and polygons count as points).
    input: String,
    /// The output raster.
    output: String,
    /// Take the values from this field rather than from the geometries' Z.
    #[arg(long = "zfield", visible_alias = "z-field", value_name = "FIELD")]
    zfield: Option<String>,
    /// Add this to the values (before --zmultiply).
    #[arg(long = "zoffset", visible_alias = "z-offset", default_value_t = 0.0, allow_negative_numbers = true)]
    zoffset: f64,
    /// Multiply the values by this (after --zoffset).
    #[arg(long = "zmultiply", visible_alias = "z-multiply", default_value_t = 1.0, allow_negative_numbers = true)]
    zmultiply: f64,
    /// The layers to grid, one band each. Needed when the dataset has several.
    #[arg(short, long = "layer", conflicts_with = "sql")]
    layers: Vec<String>,
    /// Grid the result of "SELECT fields FROM layer [WHERE ...]" instead of a layer.
    #[arg(long, value_name = "STATEMENT")]
    sql: Option<String>,
    /// Only grid the features matching this OGR SQL expression.
    #[arg(long = "where", value_name = "EXPR")]
    where_clause: Option<String>,
    /// Only grid the points strictly inside this rectangle (gdal_grid's -clipsrc).
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true)]
    bbox: Option<Vec<f64>>,
    /// The extent of the grid. By default, that of the layer.
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true)]
    extent: Option<Vec<f64>>,
    /// The size in pixels (256 x 256 by default).
    #[arg(long, num_args = 2, value_names = ["WIDTH", "HEIGHT"], conflicts_with = "resolution")]
    size: Option<Vec<usize>>,
    /// The pixel size, with --extent: (|max - min| + res / 2) / res pixels each way.
    #[arg(long, num_args = 2, value_names = ["XRES", "YRES"], requires = "extent")]
    resolution: Option<Vec<f64>>,
    /// The output data type: Byte, Int8, UInt16, Int16, UInt32, Int32, UInt64, Int64, Float32
    /// or Float64 (the default).
    #[arg(long, value_name = "TYPE")]
    ot: Option<String>,
    /// The output CRS (by default, the layer's). Points are not reprojected.
    #[arg(long, visible_alias = "a-srs", value_name = "CRS")]
    crs: Option<String>,
    /// The weighting power (invdist, invdistnn; 2 by default).
    #[arg(long, allow_negative_numbers = true)]
    power: Option<f64>,
    /// The smoothing (invdist, invdistnn; 0 by default).
    #[arg(long, allow_negative_numbers = true)]
    smoothing: Option<f64>,
    /// The search radius: of a circle (both radius1 and radius2), or invdistnn's (1 by default)
    /// and linear's nearest-neighbour fallback (0: nodata outside the triangulation; by default
    /// the nearest point however far).
    #[arg(long, conflicts_with_all = ["radius1", "radius2"], allow_negative_numbers = true)]
    radius: Option<f64>,
    /// The first semi-axis of the search ellipse (with --radius2). 0 searches every point.
    #[arg(long, requires = "radius2", allow_negative_numbers = true)]
    radius1: Option<f64>,
    /// The second semi-axis of the search ellipse (with --radius1).
    #[arg(long, requires = "radius1", allow_negative_numbers = true)]
    radius2: Option<f64>,
    /// The rotation of the search ellipse, in degrees counter-clockwise.
    #[arg(long, allow_negative_numbers = true)]
    angle: Option<f64>,
    /// The fewest points for a value; with fewer, the nodata value.
    #[arg(long = "min-points")]
    min_points: Option<u32>,
    /// The most points used (invdist with a radius, invdistnn: 12 by default, and average with
    /// per-quadrant limits).
    #[arg(long = "max-points")]
    max_points: Option<u32>,
    /// The fewest points in each quadrant around the node.
    #[arg(long = "min-points-per-quadrant")]
    min_points_per_quadrant: Option<u32>,
    /// The most points used from each quadrant.
    #[arg(long = "max-points-per-quadrant")]
    max_points_per_quadrant: Option<u32>,
    /// The value of nodes without enough points (0 by default). When given (here or in the
    /// algorithm string), it is declared as the bands' nodata value, as gdal_grid does.
    #[arg(long, allow_negative_numbers = true)]
    nodata: Option<f64>,
    #[command(flatten)]
    out: super::rasterout::OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let params = GridParams {
        power: a.power,
        smoothing: a.smoothing,
        radius: a.radius,
        radius1: a.radius1,
        radius2: a.radius2,
        angle: a.angle,
        min_points: a.min_points,
        max_points: a.max_points,
        min_points_per_quadrant: a.min_points_per_quadrant,
        max_points_per_quadrant: a.max_points_per_quadrant,
        nodata: a.nodata,
    };
    let algorithm = GridAlgorithm::with_params(&a.algorithm, &params)?;
    let layers = super::quiet(|| {
        terrio::ops::vector_filter::open_layers(&a.input, &VectorOpenOptions { layers: a.layers.clone(), ..Default::default() })
    })?;
    let four = |v: &Option<Vec<f64>>| v.as_deref().map(|v| (v[0], v[1], v[2], v[3]));
    let options = VectorGridOptions {
        algorithm,
        z_field: a.zfield,
        z_offset: a.zoffset,
        z_multiply: a.zmultiply,
        where_clause: a.where_clause,
        bbox: four(&a.bbox),
        extent: four(&a.extent),
        size: a.size.as_deref().map(|v| (v[0], v[1])),
        resolution: a.resolution.as_deref().map(|v| (v[0], v[1])),
        data_type: a.ot.as_deref().map(terrio::ops::parse_data_type).transpose()?.unwrap_or(terrio::DataType::F64),
        crs: a.crs.as_deref().map(SpatialRef::parse).transpose()?,
    };
    let out = terrio::ops::vector_grid::vector_grid_from_layers(&layers, a.sql.as_deref(), !a.layers.is_empty(), &options)?;
    a.out.write(&out, &a.output)
}
