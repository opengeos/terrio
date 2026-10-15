//! `terrio filter`: PDAL's point cloud filters.

use terrio::pointcloud::filters::{self, DimRange, OutlierMethod, OutlierOptions, SampleOptions, SmrfOptions, VoxelMode};
use terrio::pointcloud::{self, PointCloud, PointWriter, ReturnKind};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    filter: Filter,
}

/// The input and output of a filter.
#[derive(clap::Args)]
struct Io {
    /// The point cloud (LAS, LAZ, COPC, EPT, PLY or E57; a path, URL or `s3://` object).
    input: String,
    /// The output: LAS, LAZ, COPC, PLY or E57, by its extension.
    output: String,
    #[command(flatten)]
    points: super::points::PointArgs,
    #[command(flatten)]
    write: super::points::PointWriteArgs,
}

#[derive(clap::Subcommand)]
enum Filter {
    /// Classify ground points with the Simple Morphological Filter (PDAL's filters.smrf): of the
    /// returns classified, ground points become class 2 and others class 1.
    Smrf {
        #[command(flatten)]
        io: Io,
        /// The cell size.
        #[arg(long, default_value_t = 1.0)]
        cell: f64,
        /// The slope (rise over run).
        #[arg(long, default_value_t = 0.15)]
        slope: f64,
        /// The largest window, in CRS units (18 cells by default).
        #[arg(long)]
        window: Option<f64>,
        /// The elevation scalar.
        #[arg(long, default_value_t = 1.25)]
        scalar: f64,
        /// The elevation threshold.
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
        /// The cut net size (0: no net cutting).
        #[arg(long, default_value_t = 0.0)]
        cut: f64,
        /// Leave alone the points in these ranges (PDAL's ignore), such as "Classification[7:7]".
        #[arg(long, value_name = "RANGES")]
        ignore: Option<String>,
        /// The returns classified (comma-separated): first, last, intermediate, only.
        #[arg(long = "smrf-returns", value_name = "RETURNS", value_delimiter = ',', default_value = "last,only")]
        smrf_returns: Vec<String>,
        /// The class of ground points.
        #[arg(long = "ground-class", default_value_t = 2)]
        ground_class: u8,
        /// The class of other points.
        #[arg(long = "other-class", default_value_t = 1)]
        other_class: u8,
        /// Change the class of ground points only.
        #[arg(long = "only-ground")]
        only_ground: bool,
    },
    /// Classify outliers as noise (PDAL's filters.outlier): statistical (mean distance to the
    /// nearest neighbours) or radius (too few neighbours within a radius).
    Outlier {
        #[command(flatten)]
        io: Io,
        /// statistical or radius.
        #[arg(long, default_value = "statistical")]
        method: String,
        /// Radius: the fewest neighbours within the radius for a point to be kept.
        #[arg(long = "min-k", default_value_t = 2)]
        min_k: usize,
        /// Radius: the radius.
        #[arg(long, default_value_t = 1.0)]
        radius: f64,
        /// Statistical: the number of neighbours.
        #[arg(long = "mean-k", default_value_t = 8)]
        mean_k: usize,
        /// Statistical: the number of standard deviations.
        #[arg(long, default_value_t = 2.0)]
        multiplier: f64,
        /// The class of outliers (7: low point, noise).
        #[arg(long = "class-of-outliers", value_name = "CLASS", default_value_t = 7)]
        class: u8,
        /// Remove the outliers rather than classify them.
        #[arg(long)]
        remove: bool,
    },
    /// Keep the points within ranges of dimensions (PDAL's filters.range), read and written in
    /// chunks.
    Range {
        #[command(flatten)]
        io: Io,
        /// The ranges, as PDAL writes them: "Classification[2:2],Z[0:100)".
        #[arg(long)]
        limits: String,
    },
    /// Poisson sampling (PDAL's filters.sample): keep the points at least a radius from those kept
    /// before them.
    Sample {
        #[command(flatten)]
        io: Io,
        /// The smallest distance between points.
        #[arg(long, conflicts_with = "cell")]
        radius: Option<f64>,
        /// Or the voxel size (the radius is then cell / 2 × √3).
        #[arg(long = "voxel", value_name = "CELL")]
        cell: Option<f64>,
    },
    /// Keep the first point of each voxel (PDAL's filters.voxeldownsize).
    #[command(name = "voxeldownsize")]
    VoxelDownsize {
        #[command(flatten)]
        io: Io,
        /// The voxel size.
        #[arg(long, default_value_t = 0.001)]
        cell: f64,
        /// center (move the point to the voxel's center) or first.
        #[arg(long, default_value = "center")]
        mode: String,
    },
    /// Keep the point nearest the center of each voxel (PDAL's
    /// filters.voxelcenternearestneighbor).
    #[command(name = "voxelcenternearestneighbor")]
    VoxelCenterNearestNeighbor {
        #[command(flatten)]
        io: Io,
        /// The voxel size.
        #[arg(long, default_value_t = 1.0)]
        cell: f64,
    },
}

fn read(io: &Io) -> terrio::Result<PointCloud> {
    pointcloud::read_points(&io.input, &io.points.to_options()?)
}

fn write(io: &Io, cloud: &PointCloud) -> terrio::Result<()> {
    pointcloud::write_points(cloud, &io.output, &io.write.to_options(None)?)
}

pub fn run(a: Args) -> terrio::Result<()> {
    match a.filter {
        Filter::Smrf { io, cell, slope, window, scalar, threshold, cut, ignore, smrf_returns, ground_class, other_class, only_ground } => {
            let options = SmrfOptions {
                cell,
                slope,
                window,
                scalar,
                threshold,
                cut,
                ignore: ignore.as_deref().map(DimRange::parse_list).transpose()?.unwrap_or_default(),
                returns: smrf_returns.iter().map(|r| ReturnKind::parse(r)).collect::<terrio::Result<_>>()?,
                ground_class,
                other_class,
                only_ground,
            };
            let cloud = read(&io)?;
            write(&io, &filters::smrf(&cloud, &options)?)
        }
        Filter::Outlier { io, method, min_k, radius, mean_k, multiplier, class, remove } => {
            let options = OutlierOptions { method: OutlierMethod::parse(&method)?, min_k, radius, mean_k, multiplier, class };
            let cloud = read(&io)?;
            if remove {
                let outliers = filters::outlier_mask(&cloud, &options)?;
                let keep: Vec<bool> = outliers.iter().map(|o| !o).collect();
                return write(&io, &cloud.select(&keep));
            }
            write(&io, &filters::outlier(&cloud, &options)?)
        }
        Filter::Range { io, limits } => {
            // Ranges look at one point at a time: read and write in chunks.
            let ranges = DimRange::parse_list(&limits)?;
            let mut reader = pointcloud::open_points(&io.input)?;
            let template = reader.template()?;
            let mut writer = PointWriter::create(&io.output, &template, &io.write.to_options(None)?)?;
            reader.start_chunks(&io.points.to_options()?, io.write.chunk_points)?;
            while let Some(chunk) = reader.next_chunk()? {
                writer.write(&filters::range(&chunk, &ranges)?)?;
            }
            writer.finish()
        }
        Filter::Sample { io, radius, cell } => {
            let cloud = read(&io)?;
            write(&io, &filters::sample(&cloud, &SampleOptions { cell, radius, origin: None })?)
        }
        Filter::VoxelDownsize { io, cell, mode } => {
            let cloud = read(&io)?;
            write(&io, &filters::voxel_downsize(&cloud, cell, VoxelMode::parse(&mode)?)?)
        }
        Filter::VoxelCenterNearestNeighbor { io, cell } => {
            let cloud = read(&io)?;
            write(&io, &filters::voxel_center_nearest_neighbor(&cloud, cell)?)
        }
    }
}
