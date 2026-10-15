//! Options shared by the commands that read point clouds, and `terrio convert` for them.

use terrio::crs::SpatialRef;
use terrio::pointcloud::{self, ConvertOptions, PlyEncoding, PointFilter, PointFormat, PointReadOptions, PointWriteOptions, ReturnKind};

/// Which points to read.
#[derive(clap::Args)]
pub struct PointArgs {
    /// Keep the points in these bounds, in the input's CRS.
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true, help_heading = "Point cloud options")]
    bounds: Option<Vec<f64>>,
    /// Keep the points with z in this range.
    #[arg(long = "z-range", num_args = 2, value_names = ["MIN", "MAX"], allow_negative_numbers = true, help_heading = "Point cloud options")]
    z_range: Option<Vec<f64>>,
    /// Keep the points of these classifications (comma-separated, such as 2,6).
    #[arg(long = "class", value_name = "CLASSES", value_delimiter = ',', help_heading = "Point cloud options")]
    classes: Vec<u8>,
    /// Keep these returns of each pulse (comma-separated): first, last, intermediate, only.
    #[arg(long, value_name = "RETURNS", value_delimiter = ',', help_heading = "Point cloud options")]
    returns: Vec<String>,
    /// COPC and EPT: read down to this octree level (0 is the root).
    #[arg(long = "max-level", value_name = "LEVEL", help_heading = "Point cloud options")]
    max_level: Option<u32>,
    /// COPC and EPT: read the octree levels needed for this point spacing.
    #[arg(long, value_name = "SPACING", help_heading = "Point cloud options")]
    resolution: Option<f64>,
    /// Read at most about this many points: COPC and EPT down to the deepest level that fits,
    /// others by keeping every n-th point.
    #[arg(long = "max-points", value_name = "N", help_heading = "Point cloud options")]
    max_points: Option<u64>,
}

impl PointArgs {
    /// The library's read options.
    pub fn to_options(&self) -> terrio::Result<PointReadOptions> {
        Ok(PointReadOptions {
            filter: PointFilter {
                bounds: self.bounds.as_deref().map(|b| [b[0], b[1], b[2], b[3]]),
                z_range: self.z_range.as_deref().map(|z| (z[0], z[1])),
                classes: self.classes.clone(),
                returns: self.returns.iter().map(|r| ReturnKind::parse(r)).collect::<terrio::Result<_>>()?,
            },
            max_level: self.max_level,
            resolution: self.resolution,
            max_points: self.max_points,
        })
    }
}

/// How to write point clouds.
#[derive(clap::Args)]
pub struct PointWriteArgs {
    /// The output point data record format (0 to 10). By default, the input's (6 to 8 for COPC).
    #[arg(long = "point-format", value_name = "N", help_heading = "Point cloud options")]
    point_format: Option<u8>,
    /// The output LAS version, such as 1.2 or 1.4. By default, the input's, raised as the point
    /// format needs.
    #[arg(long = "las-version", value_name = "1.N", help_heading = "Point cloud options")]
    las_version: Option<String>,
    /// The scale of the stored X, Y and Z.
    #[arg(long, num_args = 3, value_names = ["X", "Y", "Z"], help_heading = "Point cloud options")]
    scale: Option<Vec<f64>>,
    /// The offset of the stored X, Y and Z.
    #[arg(long, num_args = 3, value_names = ["X", "Y", "Z"], allow_negative_numbers = true, help_heading = "Point cloud options")]
    offset: Option<Vec<f64>>,
    /// For vector, PLY and E57 outputs, the dimensions written (comma-separated). All by default.
    #[arg(long, value_name = "DIMS", value_delimiter = ',', help_heading = "Point cloud options")]
    dims: Vec<String>,
    /// PLY outputs: ascii (the default, as PDAL's), binary_little_endian or binary_big_endian.
    #[arg(long = "ply-encoding", value_name = "ENCODING", help_heading = "Point cloud options")]
    ply_encoding: Option<String>,
    /// PLY ASCII outputs: the digits after the decimal point of floats (by default, as many as
    /// needed to read the same values back).
    #[arg(long = "ply-precision", value_name = "DIGITS", help_heading = "Point cloud options")]
    ply_precision: Option<usize>,
    /// The points read and written at a time: point clouds are converted in chunks, in bounded
    /// memory, whatever their size (about a million points by default).
    #[arg(long = "chunk-points", value_name = "N", default_value_t = 0, help_heading = "Point cloud options")]
    pub chunk_points: usize,
}

impl PointWriteArgs {
    /// The library's write options.
    pub fn to_options(&self, format: Option<&str>) -> terrio::Result<PointWriteOptions> {
        let minor = match self.las_version.as_deref() {
            None => None,
            Some(s) => Some(
                s.strip_prefix("1.")
                    .and_then(|m| m.parse::<u8>().ok())
                    .filter(|m| *m <= 4)
                    .ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown LAS version '{s}' (1.0 to 1.4)")))?,
            ),
        };
        let format = match format {
            None => None,
            Some(f) => match PointFormat::from_name(f) {
                Some(p) => Some(p),
                None => return Err(terrio::Error::InvalidArgument(format!("unknown point cloud format '{f}' (LAS, LAZ, COPC, PLY, E57)"))),
            },
        };
        Ok(PointWriteOptions {
            format,
            point_format: self.point_format,
            minor_version: minor,
            scale: self.scale.as_deref().map(|s| [s[0], s[1], s[2]]),
            offset: self.offset.as_deref().map(|o| [o[0], o[1], o[2]]),
            ply_encoding: self.ply_encoding.as_deref().map(PlyEncoding::parse).transpose()?.unwrap_or_default(),
            ply_precision: self.ply_precision,
            dimensions: self.dims.clone(),
            ..Default::default()
        })
    }
}

/// Converts a point cloud.
pub fn convert(
    input: &str,
    output: &str,
    format: Option<&str>,
    t_srs: Option<SpatialRef>,
    vector: &super::vector_opts::VectorArgs,
    points: &PointArgs,
    write: &PointWriteArgs,
) -> terrio::Result<()> {
    let v = vector.to_options(None)?;
    if v.where_clause.is_some()
        || v.sql.is_some()
        || v.select.is_some()
        || v.limit.is_some()
        || v.layer_name.is_some()
        || v.src_srs.is_some()
    {
        return Err(terrio::Error::InvalidArgument("of the vector options, point clouds take --spat and --a-srs only".into()));
    }
    let mut read = points.to_options()?;
    if let Some((x0, y0, x1, y1)) = v.spatial_filter {
        read.filter.bounds = Some([x0, y0, x1, y1]);
    }
    let options = ConvertOptions {
        read,
        assign_srs: v.assign_srs,
        target_srs: t_srs,
        write: write.to_options(format)?,
        layer: pointcloud::vector::ToLayerOptions { dimensions: write.dims.clone(), ..Default::default() },
        chunk_points: write.chunk_points,
    };
    pointcloud::convert(input, output, &options)
}
