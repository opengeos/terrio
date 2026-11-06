// Written to match the behavior of:
//   PDAL 2.10.2:
//     doc/stages/filters.*.md
// No code is translated from these sources.

//! `terrio filter`: PDAL's point cloud filters.

use terrio::formats::VectorDriver;
use terrio::pointcloud::filters::{
    self, AssignOptions, BandDimension, ClusterOptions, CovarianceFeaturesOptions, CropBox, CropOptions, CsfOptions, DbscanOptions,
    DimRange, EigenvaluesOptions, FeatureMode, HagOptions, HexbinOptions, Neighbourhood, NnDistanceMode, NormalOptions, OutlierMethod,
    OutlierOptions, OverlayOptions, PmfOptions, SampleOptions, SmrfOptions, SplitterOptions, VoxelMode,
};
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
    /// The output: LAS, LAZ, COPC, PLY, E57 or EPT (`ept.json`), by its extension.
    output: String,
    /// Assign this CRS to the points, without reprojecting (when the file has none or a wrong one).
    #[arg(long = "a-srs", value_name = "CRS", help_heading = "Point cloud options")]
    a_srs: Option<String>,
    #[command(flatten)]
    points: super::points::PointArgs,
    #[command(flatten)]
    write: super::points::PointWriteArgs,
}

#[derive(clap::Subcommand)]
#[command(next_display_order = None)]
enum Filter {
    /// Classify ground with the Simple Morphological Filter.
    ///
    /// Of the returns classified, ground points become class 2 and others class 1 (PDAL's
    /// filters.smrf).
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
    /// Classify outliers as noise.
    ///
    /// Statistical (mean distance to the nearest neighbours) or radius (too few neighbours within a
    /// radius) (PDAL's filters.outlier).
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
    /// Keep the points within ranges of dimensions.
    ///
    /// Reads and writes in chunks (PDAL's filters.range).
    Range {
        #[command(flatten)]
        io: Io,
        /// The ranges, as PDAL writes them: "Classification[2:2],Z[0:100)".
        #[arg(long)]
        limits: String,
    },
    /// Poisson sampling (PDAL's filters.sample).
    ///
    /// Keeps the points at least a radius from those kept before them.
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
    /// Keep the first point of each voxel.
    ///
    /// PDAL's filters.voxeldownsize.
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
    /// Keep the point nearest the center of each voxel.
    ///
    /// PDAL's filters.voxelcenternearestneighbor.
    #[command(name = "voxelcenternearestneighbor")]
    VoxelCenterNearestNeighbor {
        #[command(flatten)]
        io: Io,
        /// The voxel size.
        #[arg(long, default_value_t = 1.0)]
        cell: f64,
    },
    /// Classify ground with the Progressive Morphological Filter.
    ///
    /// Of the returns classified, ground points become class 2 and others class 1 (PDAL's
    /// filters.pmf).
    Pmf {
        #[command(flatten)]
        io: Io,
        /// The cell size.
        #[arg(long = "cell-size", default_value_t = 1.0)]
        cell: f64,
        /// Grow the windows linearly rather than exponentially.
        #[arg(long)]
        linear: bool,
        /// The initial height threshold.
        #[arg(long = "initial-distance", default_value_t = 0.15)]
        initial_distance: f64,
        /// The largest height threshold.
        #[arg(long = "max-distance", default_value_t = 2.5)]
        max_distance: f64,
        /// The largest window size.
        #[arg(long = "max-window-size", default_value_t = 33.0)]
        max_window: f64,
        /// The slope.
        #[arg(long, default_value_t = 1.0)]
        slope: f64,
        /// Leave alone the points in these ranges (PDAL's ignore), such as "Classification[7:7]".
        #[arg(long, value_name = "RANGES")]
        ignore: Option<String>,
        /// The returns classified (comma-separated): first, last, intermediate, only.
        #[arg(long = "pmf-returns", value_name = "RETURNS", value_delimiter = ',', default_value = "last,only")]
        pmf_returns: Vec<String>,
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
    /// Classify ground with the Cloth Simulation Filter.
    ///
    /// Of the returns classified, ground points become class 2 and others class 1 (PDAL's
    /// filters.csf). A local input directory processes its LAS/LAZ files (including
    /// COPC) without recursion, keeping their names in the output directory. Existing
    /// output files and the input directory itself are refused.
    Csf {
        #[command(flatten)]
        io: Io,
        /// The cloth's resolution (PDAL's resolution).
        #[arg(long = "cloth-resolution", default_value_t = 1.0)]
        cloth_resolution: f64,
        /// The cloth's rigidness (1 to 3).
        #[arg(long, default_value_t = 3)]
        rigidness: usize,
        /// The time step.
        #[arg(long, default_value_t = 0.65)]
        step: f64,
        /// The largest distance of ground points from the cloth.
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,
        /// The height difference threshold of slope handling.
        #[arg(long, default_value_t = 0.3)]
        hdiff: f64,
        /// The most simulation steps.
        #[arg(long, default_value_t = 500)]
        iterations: usize,
        /// No slope handling after the simulation.
        #[arg(long = "no-smooth")]
        no_smooth: bool,
        /// Leave alone the points in these ranges (PDAL's ignore), such as "Classification[7:7]".
        #[arg(long, value_name = "RANGES")]
        ignore: Option<String>,
        /// The returns classified (comma-separated): first, last, intermediate, only.
        #[arg(long = "csf-returns", value_name = "RETURNS", value_delimiter = ',', default_value = "last,only")]
        csf_returns: Vec<String>,
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
    /// Classify low noise with the Extended Local Minimum.
    ///
    /// PDAL's filters.elm.
    Elm {
        #[command(flatten)]
        io: Io,
        /// The cell size.
        #[arg(long, default_value_t = 10.0)]
        cell: f64,
        /// The elevation difference above which the lower point is noise.
        #[arg(long, default_value_t = 1.0)]
        threshold: f64,
        /// The class of noise points (7: low point).
        #[arg(long = "noise-class", value_name = "CLASS", default_value_t = 7)]
        class: u8,
    },
    /// Compute heights above the nearest ground points.
    ///
    /// Written to HeightAboveGround (PDAL's filters.hag_nn).
    #[command(name = "hag_nn")]
    HagNn {
        #[command(flatten)]
        io: Io,
        #[command(flatten)]
        hag: HagArgs,
        /// The ground points used for each point.
        #[arg(long, default_value_t = 1)]
        count: usize,
        /// Ground points farther than this are not used (0: no limit).
        #[arg(long = "max-distance", default_value_t = 0.0)]
        max_distance: f64,
    },
    /// Compute heights above a Delaunay ground triangulation.
    ///
    /// Triangulates the nearest ground points; written to HeightAboveGround (PDAL's
    /// filters.hag_delaunay).
    #[command(name = "hag_delaunay")]
    HagDelaunay {
        #[command(flatten)]
        io: Io,
        #[command(flatten)]
        hag: HagArgs,
        /// The ground points triangulated for each point.
        #[arg(long, default_value_t = 10)]
        count: usize,
    },
    /// Keep the points in boxes, polygons, circles or spheres.
    ///
    /// PDAL's filters.crop.
    Crop {
        #[command(flatten)]
        io: Io,
        /// A box, as PDAL writes them: "([xmin, xmax], [ymin, ymax])" or with "[zmin, zmax]".
        #[arg(long = "crop-bounds", value_name = "BOUNDS")]
        crop_bounds: Vec<String>,
        /// A polygon or multipolygon, as WKT or GeoJSON.
        #[arg(long)]
        polygon: Vec<String>,
        /// The centre of a circle (x,y) or sphere (x,y,z), with --distance.
        #[arg(long, value_name = "X,Y[,Z]", allow_hyphen_values = true)]
        point: Vec<String>,
        /// The radius of the circles and spheres.
        #[arg(long, default_value_t = 0.0)]
        distance: f64,
        /// Keep the points outside instead.
        #[arg(long)]
        outside: bool,
        /// The CRS of the geometries, when it is not the points'.
        #[arg(long = "geometry-srs", value_name = "SRS")]
        geometry_srs: Option<String>,
    },
    /// Keep every n-th point (PDAL's filters.decimation).
    Decimation {
        #[command(flatten)]
        io: Io,
        /// The step (points to skip between those kept, plus one; a fraction is allowed).
        #[arg(long, default_value_t = 1.0)]
        step: f64,
        /// The index of the first point considered (PDAL's offset).
        #[arg(long, default_value_t = 0)]
        start: usize,
        /// The index past the last point considered (PDAL's limit).
        #[arg(long)]
        end: Option<usize>,
    },
    /// Keep the points for which an expression is true.
    ///
    /// Reads and writes in chunks, such as "Classification == 2 && Z > 100" (PDAL's
    /// filters.expression).
    Expression {
        #[command(flatten)]
        io: Io,
        /// The expression.
        #[arg(long)]
        expression: String,
    },
    /// Assign values to dimensions (PDAL's filters.assign).
    Assign {
        #[command(flatten)]
        io: Io,
        /// A range and a value: "Classification[1:1]=2".
        #[arg(long)]
        assignment: Vec<String>,
        /// Only change the points in this range, such as "ReturnNumber[1:1]".
        #[arg(long)]
        condition: Option<String>,
        /// An expression: "Classification = 2 WHERE HeightAboveGround < 0.5".
        #[arg(long)]
        value: Vec<String>,
    },
    /// Merge point clouds into one (PDAL's filters.merge).
    Merge {
        /// The point clouds, then the output. `@FILE` reads input paths from FILE, one per line.
        #[arg(num_args = 2.., required = true)]
        paths: Vec<String>,
        /// Assign this CRS to the points, without reprojecting (when the files have none or a
        /// wrong one).
        #[arg(long = "a-srs", value_name = "CRS", help_heading = "Point cloud options")]
        a_srs: Option<String>,
        #[command(flatten)]
        points: super::points::PointArgs,
        #[command(flatten)]
        write: super::points::PointWriteArgs,
    },
    /// Split points into squares.
    ///
    /// Writes a file per square when the output's file name has a '#' (replaced by 1, 2...), else
    /// all in one file, square after square (PDAL's filters.splitter).
    Splitter {
        #[command(flatten)]
        io: Io,
        /// The side of the squares.
        #[arg(long, default_value_t = 1000.0)]
        length: f64,
        /// The squares' origin (by default, the first point).
        #[arg(long, num_args = 2, value_names = ["X", "Y"], allow_negative_numbers = true)]
        origin: Option<Vec<f64>>,
        /// A buffer around each square whose points it takes too.
        #[arg(long, default_value_t = 0.0)]
        buffer: f64,
    },
    /// Split points into squarish chips.
    ///
    /// Each chip has at most a number of points: a file per chip when the output's file name has a
    /// '#', else all in one file (PDAL's filters.chipper).
    Chipper {
        #[command(flatten)]
        io: Io,
        /// The most points of a chip.
        #[arg(long, default_value_t = 5000)]
        capacity: usize,
    },
    /// Compute normals and curvature.
    ///
    /// Written to NormalX, NormalY, NormalZ and Curvature (PDAL's filters.normal).
    Normal {
        #[command(flatten)]
        io: Io,
        /// The neighbours of each point.
        #[arg(long, default_value_t = 8)]
        knn: usize,
        /// Orient the normals towards this point (rather than up).
        #[arg(long, num_args = 3, value_names = ["X", "Y", "Z"], allow_negative_numbers = true)]
        viewpoint: Option<Vec<f64>>,
        /// Do not orient the normals up.
        #[arg(long = "not-always-up")]
        not_always_up: bool,
    },
    /// Label Euclidean clusters.
    ///
    /// Written to ClusterID (PDAL's filters.cluster).
    Cluster {
        #[command(flatten)]
        io: Io,
        /// The fewest points of a cluster (PDAL's min_points).
        #[arg(long = "min-cluster-points", default_value_t = 1)]
        min_cluster_points: u64,
        /// The most points of a cluster (PDAL's max_points).
        #[arg(long = "max-cluster-points")]
        max_cluster_points: Option<u64>,
        /// The distance within which points are neighbours.
        #[arg(long, default_value_t = 1.0)]
        tolerance: f64,
        /// Measure distances in x and y only.
        #[arg(long = "2d")]
        two_d: bool,
    },
    /// Label density-based clusters.
    ///
    /// Written to ClusterID: from 0, -1 for noise (PDAL's filters.dbscan).
    Dbscan {
        #[command(flatten)]
        io: Io,
        /// The fewest points (itself included) within eps of a core point.
        #[arg(long = "min-cluster-points", default_value_t = 6)]
        min_cluster_points: u64,
        /// The distance within which points are neighbours.
        #[arg(long, default_value_t = 1.0)]
        eps: f64,
        /// The dimensions distances are measured in (comma-separated).
        #[arg(long, value_delimiter = ',', default_value = "X,Y,Z")]
        dimensions: Vec<String>,
    },
    /// Compute covariance features of each point's neighbours.
    ///
    /// Linearity, Planarity, Scattering, Verticality and others (PDAL's
    /// filters.covariancefeatures).
    #[command(name = "covariancefeatures")]
    CovarianceFeatures {
        #[command(flatten)]
        io: Io,
        #[command(flatten)]
        nb: NeighbourArgs,
        /// The neighbours of each point.
        #[arg(long, default_value_t = 10)]
        knn: usize,
        /// The features (comma-separated): Dimensionality (Linearity, Planarity, Scattering and
        /// Verticality), All, or Linearity, Planarity, Scattering, Verticality, Omnivariance,
        /// Anisotropy, Eigenentropy, EigenvalueSum, SurfaceVariation, DemantkeVerticality, Density.
        #[arg(long = "feature-set", value_delimiter = ',', default_value = "Dimensionality")]
        feature_set: Vec<String>,
        /// How the eigenvalues are scaled: raw, sqrt or normalized.
        #[arg(long, default_value = "sqrt")]
        mode: String,
        /// Use each point's OptimalKNN nearest points.
        #[arg(long)]
        optimized: bool,
    },
    /// Compute covariance eigenvalues of each point's neighbours.
    ///
    /// Written to Eigenvalue0, Eigenvalue1 and Eigenvalue2, ascending (PDAL's filters.eigenvalues).
    Eigenvalues {
        #[command(flatten)]
        io: Io,
        #[command(flatten)]
        nb: NeighbourArgs,
        /// The neighbours of each point.
        #[arg(long, default_value_t = 8)]
        knn: usize,
        /// Divide the eigenvalues by their sum.
        #[arg(long)]
        normalize: bool,
    },
    /// Label approximately coplanar points.
    ///
    /// Written to Coplanar (PDAL's filters.approximatecoplanar).
    #[command(name = "approximatecoplanar")]
    ApproximateCoplanar {
        #[command(flatten)]
        io: Io,
        /// The nearest points (itself included) of each point.
        #[arg(long, default_value_t = 8)]
        knn: usize,
        /// The least ratio of the middle eigenvalue to the smallest.
        #[arg(long, default_value_t = 25.0)]
        thresh1: f64,
        /// The most ratio of the largest eigenvalue to the middle one.
        #[arg(long, default_value_t = 6.0)]
        thresh2: f64,
    },
    /// Estimate the rank of each point's neighbourhood.
    ///
    /// Written to Rank (PDAL's filters.estimaterank).
    #[command(name = "estimaterank")]
    EstimateRank {
        #[command(flatten)]
        io: Io,
        /// The nearest points (itself included) of each point.
        #[arg(long, default_value_t = 8)]
        knn: usize,
        /// The smallest singular value counted, relative to the largest.
        #[arg(long, default_value_t = 0.01)]
        thresh: f64,
    },
    /// Compute local outlier factors.
    ///
    /// Written to NNDistance, LocalReachabilityDistance and LocalOutlierFactor (PDAL's
    /// filters.lof).
    Lof {
        #[command(flatten)]
        io: Io,
        /// The neighbours of each point.
        #[arg(long, default_value_t = 10)]
        minpts: usize,
    },
    /// Compute the distance to each point's nearest neighbours.
    ///
    /// Written to NNDistance (PDAL's filters.nndistance).
    #[command(name = "nndistance")]
    NnDistance {
        #[command(flatten)]
        io: Io,
        /// The neighbours of each point.
        #[arg(short, long, default_value_t = 10)]
        k: usize,
        /// kth (the distance to the k-th) or avg (the mean distance).
        #[arg(long, default_value = "kth")]
        mode: String,
    },
    /// Compute the density of points around each point.
    ///
    /// Written to RadialDensity (PDAL's filters.radialdensity).
    #[command(name = "radialdensity")]
    RadialDensity {
        #[command(flatten)]
        io: Io,
        /// The radius of the sphere counted.
        #[arg(long, default_value_t = 1.0)]
        radius: f64,
    },
    /// Compute the share of non-reciprocal neighbours.
    ///
    /// The percentage of each point's neighbours that do not have it as theirs, written to
    /// Reciprocity (PDAL's filters.reciprocity).
    Reciprocity {
        #[command(flatten)]
        io: Io,
        /// The neighbours of each point.
        #[arg(long, default_value_t = 8)]
        knn: usize,
    },
    /// Compute the distance to the neighbours' miniball centre.
    ///
    /// How far each point is from the centre of the smallest ball around its neighbours, written to
    /// Miniball (PDAL's filters.miniball).
    Miniball {
        #[command(flatten)]
        io: Io,
        /// The neighbours of each point.
        #[arg(long, default_value_t = 8)]
        knn: usize,
    },
    /// Smooth elevations.
    ///
    /// Writes a percentile of the Z of the neighbours in x and y to a dimension (PDAL's
    /// filters.zsmooth).
    Zsmooth {
        #[command(flatten)]
        io: Io,
        /// The dimension of the statistic (doubles).
        #[arg(long)]
        dim: String,
        /// The radius in x and y of the neighbours.
        #[arg(long, default_value_t = 1.0)]
        radius: f64,
        /// The percentile: 0 the lowest, 50 the median, 100 the highest.
        #[arg(long = "medianpercent", default_value_t = 50.0)]
        median_percent: f64,
    },
    /// Classify ground points by skewness balancing.
    ///
    /// The points come out sorted by Z, as PDAL writes them (PDAL's filters.skewnessbalancing).
    #[command(name = "skewnessbalancing")]
    SkewnessBalancing {
        #[command(flatten)]
        io: Io,
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
    /// Keep the first points (PDAL's filters.head).
    Head {
        #[command(flatten)]
        io: Io,
        /// The points kept.
        #[arg(long, default_value_t = 10)]
        count: usize,
        /// Drop them instead.
        #[arg(long)]
        invert: bool,
    },
    /// Keep the last points (PDAL's filters.tail).
    Tail {
        #[command(flatten)]
        io: Io,
        /// The points kept.
        #[arg(long, default_value_t = 10)]
        count: usize,
        /// Drop them instead.
        #[arg(long)]
        invert: bool,
    },
    /// Sort the points.
    ///
    /// Sorts by the first dimension, then stably by each of the others in turn (PDAL's
    /// filters.sort).
    Sort {
        #[command(flatten)]
        io: Io,
        /// The dimensions (comma-separated).
        #[arg(long, value_delimiter = ',', required = true)]
        dimensions: Vec<String>,
        /// asc or desc.
        #[arg(long, default_value = "asc")]
        order: String,
    },
    /// Keep the point of a dimension's largest or smallest value.
    ///
    /// PDAL's filters.locate.
    Locate {
        #[command(flatten)]
        io: Io,
        /// The dimension.
        #[arg(long)]
        dimension: String,
        /// max or min.
        #[arg(long, default_value = "max")]
        minmax: String,
    },
    /// Keep the points within a dimension's interquartile fences.
    ///
    /// PDAL's filters.iqr.
    Iqr {
        #[command(flatten)]
        io: Io,
        /// The dimension.
        #[arg(long)]
        dimension: String,
        /// The fences are this many interquartile ranges outside the quartiles.
        #[arg(short, long, default_value_t = 1.5)]
        k: f64,
    },
    /// Keep the points within MADs of a dimension's median.
    ///
    /// MAD is the median absolute deviation (PDAL's filters.mad).
    Mad {
        #[command(flatten)]
        io: Io,
        /// The dimension.
        #[arg(long)]
        dimension: String,
        /// The number of deviations.
        #[arg(short, long, default_value_t = 2.0)]
        k: f64,
        /// The factor of the median absolute deviation.
        #[arg(long = "mad-multiplier", default_value_t = 1.4862)]
        mad_multiplier: f64,
    },
    /// Keep the points within a range of a raster's values.
    ///
    /// The raster is taken to be in the points' CRS (PDAL's filters.dem).
    Dem {
        #[command(flatten)]
        io: Io,
        /// The raster.
        #[arg(long)]
        raster: String,
        /// The dimension and how far below and above the raster's value: "Z[20:20]".
        #[arg(long)]
        limits: String,
        /// The band, from 1.
        #[arg(long, default_value_t = 1)]
        band: usize,
    },
    /// Set dimensions from raster bands.
    ///
    /// Sets Red, Green and Blue from bands 1, 2 and 3 by default; the raster is taken to be in the
    /// points' CRS (PDAL's filters.colorization).
    Colorization {
        #[command(flatten)]
        io: Io,
        /// The raster.
        #[arg(long)]
        raster: String,
        /// The dimensions, as Name:band:scale, comma-separated: "Red:1:1.0, Green:2, Blue:3".
        #[arg(long, default_value = "Red,Green,Blue")]
        dimensions: String,
    },
    /// Set a dimension from attributes of enclosing polygons.
    ///
    /// Uses an attribute of the polygons the points are in (PDAL's filters.overlay).
    Overlay {
        #[command(flatten)]
        io: Io,
        /// The vector dataset.
        #[arg(long)]
        datasource: String,
        /// The dimension set (the points must have it).
        #[arg(long)]
        dimension: String,
        /// The attribute (by default, the second, as PDAL takes it).
        #[arg(long)]
        column: Option<String>,
        /// The layer (by default, the first).
        #[arg(long)]
        layer: Option<String>,
    },
    /// Trace the points' boundary on a grid of hexagons.
    ///
    /// Reads in chunks: the union of the hexagons with at least --threshold points, polygons with
    /// holes, written to a vector dataset as one multipolygon feature (with its area, density,
    /// point and hexagon counts). The grid is anchored at the first point, as PDAL's filters.hexbin does.
    Hexbin {
        /// The point cloud (LAS, LAZ, COPC, EPT, PLY or E57; a path, URL or `s3://` object).
        input: String,
        /// The boundary's vector dataset (GeoJSON, GeoPackage, FlatGeobuf...), by its extension.
        output: String,
        /// Assign this CRS to the points (when the file has none or a wrong one).
        #[arg(long = "a-srs", value_name = "CRS", help_heading = "Point cloud options")]
        a_srs: Option<String>,
        #[command(flatten)]
        points: super::points::PointArgs,
        /// The hexagons' edge length (PDAL's edge_length, or edge_size). By default, estimated as
        /// PDAL estimates it: the hexagons' height is --threshold times the mean distance between
        /// consecutive points of the first --sample-size.
        #[arg(long = "edge-length", visible_alias = "edge-size", value_name = "LENGTH", default_value_t = 0.0)]
        edge_length: f64,
        /// The points the edge length is estimated from.
        #[arg(long = "sample-size", value_name = "N", default_value_t = 5000)]
        sample_size: usize,
        /// The fewest points of a hexagon in the boundary.
        #[arg(long, default_value_t = 15)]
        threshold: u64,
        /// Do not smooth the boundary (PDAL's smooth=false): keep every hexagon edge.
        #[arg(long = "no-smooth")]
        no_smooth: bool,
        /// When smoothing, drop polygons and holes smaller than this (by default 6 × tolerance²,
        /// the tolerance 1.1 × √3 × edge length / 2).
        #[arg(long = "hole-cull-area-tolerance", value_name = "AREA")]
        hole_cull_area_tolerance: Option<f64>,
        /// The grid's origin, a hexagon's lower-left corner (by default, the first point).
        #[arg(long, num_args = 2, value_names = ["X", "Y"], allow_negative_numbers = true)]
        origin: Option<Vec<f64>>,
        /// The decimals of the boundary's coordinates (as PDAL rounds them, before smoothing).
        #[arg(long, default_value_t = 8)]
        precision: usize,
        /// Also write the hexagons with at least --threshold points, with their counts, to this
        /// vector dataset (PDAL's density).
        #[arg(long, value_name = "OUTPUT")]
        density: Option<String>,
        /// The output format's short name, for the boundary and --density. By default, chosen
        /// from each output's extension.
        #[arg(short = 'f', long = "format")]
        format: Option<String>,
        /// The points read at a time (about a million by default).
        #[arg(long = "chunk-points", value_name = "N", default_value_t = 0, help_heading = "Point cloud options")]
        chunk_points: usize,
    },
}

/// How the covariance filters find neighbours, besides their `knn`.
#[derive(clap::Args)]
struct NeighbourArgs {
    /// Take every n-th of the nearest points.
    #[arg(long, default_value_t = 1)]
    stride: usize,
    /// Take the points within this radius instead of the nearest.
    #[arg(long)]
    radius: Option<f64>,
    /// With a radius, the fewest points (itself included) of a neighbourhood computed.
    #[arg(long = "min-k", default_value_t = 3)]
    min_k: usize,
}

impl NeighbourArgs {
    fn to_neighbourhood(&self, knn: usize) -> Neighbourhood {
        Neighbourhood { knn, stride: self.stride, radius: self.radius, min_k: self.min_k }
    }
}

/// The options of the height above ground filters.
#[derive(clap::Args)]
struct HagArgs {
    /// Do not extrapolate: points outside the ground points' bounds get a height of 0.
    #[arg(long = "no-extrapolation")]
    no_extrapolation: bool,
    /// The class of ground points.
    #[arg(long = "ground-class", default_value_t = 2)]
    ground_class: u8,
}

/// The output path with the last '#' of its file name replaced by `k`, `None` when the file name
/// has none (a '#' in a directory name is kept).
fn numbered(output: &str, k: usize) -> Option<String> {
    let name_start = output.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let hash = name_start + output[name_start..].rfind('#')?;
    Some(format!("{}{k}{}", &output[..hash], &output[hash + 1..]))
}

/// Writes clouds to one file each (the last '#' of the output's file name replaced by 1, 2...),
/// or all to one file.
fn write_many(io: &Io, clouds: Vec<PointCloud>) -> terrio::Result<()> {
    let options = io.write.to_options(None)?;
    if numbered(&io.output, 0).is_some() {
        if clouds.is_empty() {
            // No points: one empty file, numbered 1, as one file holds none without a '#'.
            let path = numbered(&io.output, 1).unwrap_or_else(|| io.output.clone());
            return PointWriter::create(&path, &read_template(io)?, &options)?.finish();
        }
        for (k, c) in clouds.iter().enumerate() {
            let path = numbered(&io.output, k + 1).unwrap_or_else(|| io.output.clone());
            pointcloud::write_points(c, &path, &options)?;
        }
        return Ok(());
    }
    // The input's dimensions and CRS, read again only when no part has any.
    let template;
    let first = match clouds.first() {
        Some(c) => c,
        None => {
            template = read_template(io)?;
            &template
        }
    };
    let mut writer = PointWriter::create(&io.output, first, &options)?;
    for c in &clouds {
        writer.write(c)?;
    }
    writer.finish()
}

/// The input's dimensions and CRS, without its points.
fn read_template(io: &Io) -> terrio::Result<PointCloud> {
    let mut template = pointcloud::open_points(&io.input)?.template()?;
    assign_srs(&io.a_srs, &mut template)?;
    Ok(template)
}

/// Assigns `--a-srs`, when given, to a cloud.
fn assign_srs(a_srs: &Option<String>, cloud: &mut PointCloud) -> terrio::Result<()> {
    if let Some(s) = a_srs {
        cloud.assign_crs(terrio::crs::SpatialRef::parse(s)?);
    }
    Ok(())
}

/// A point: "x,y" or "x,y,z".
fn parse_point(s: &str) -> terrio::Result<Vec<f64>> {
    let v: Vec<f64> = s
        .split([',', ' '])
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| terrio::Error::InvalidArgument(format!("invalid point '{s}': use x,y or x,y,z")))?;
    if v.len() != 2 && v.len() != 3 {
        return Err(terrio::Error::InvalidArgument(format!("invalid point '{s}': use x,y or x,y,z")));
    }
    Ok(v)
}

fn read(io: &Io) -> terrio::Result<PointCloud> {
    Ok(read_path(&io.input, io)?.0)
}

fn read_path(path: &str, io: &Io) -> terrio::Result<(PointCloud, Option<pointcloud::PointFormat>)> {
    let mut reader = pointcloud::open_points(path)?;
    let format = pointcloud::PointFormat::from_name(reader.format_name());
    let mut cloud = reader.read(&io.points.to_options()?)?;
    assign_srs(&io.a_srs, &mut cloud)?;
    Ok((cloud, format))
}

fn write(io: &Io, cloud: &PointCloud) -> terrio::Result<()> {
    pointcloud::write_points(cloud, &io.output, &io.write.to_options(None)?)
}

/// Classify a single point cloud or a non-recursive batch of local LAS/LAZ tiles.
/// Batch outputs preserve tile names and detected COPC content without clobbering.
fn run_csf(io: &Io, options: &CsfOptions) -> terrio::Result<()> {
    use std::path::Path;
    use terrio::Error;

    let input = Path::new(&io.input);
    if io.input.starts_with("ept://") || !input.is_dir() {
        return write(io, &filters::csf(&read(io)?, options)?);
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(input)? {
        let entry = entry?;
        let path = entry.path();
        let las = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("las") || ext.eq_ignore_ascii_case("laz"));
        // AppleDouble metadata on macOS removable/network volumes is not a tile.
        let apple_double = entry.file_name().as_encoded_bytes().starts_with(b"._");
        if las && !apple_double && entry.file_type()?.is_file() {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(Error::InvalidArgument(format!("input directory '{}' contains no regular LAS/LAZ files", io.input)));
    }
    let location = terrio::vfs::Location::parse(&io.output);
    let local = match &location {
        // `ept://` parses as a local path but selects the EPT writer.
        terrio::vfs::Location::Local(_) => !io.output.starts_with("ept://"),
        // A directory name ending in .zip is not an archive output request.
        terrio::vfs::Location::Zip { archive, inner: None } => {
            matches!(archive.as_ref(), terrio::vfs::Location::Local(_)) && !io.output.starts_with("/vsizip/")
        }
        _ => false,
    };
    if !local {
        return Err(Error::InvalidArgument("CSF directory output must be a local directory".into()));
    }
    let output = Path::new(&io.output);
    if output.exists() && !output.is_dir() {
        return Err(Error::InvalidArgument(format!("CSF directory input requires an output directory, not '{}'", io.output)));
    }
    // Resolve aliases only after creating parents: input/new/.. can name the
    // input directory even though it did not exist before creating `new`.
    std::fs::create_dir_all(output)?;
    if input.canonicalize()? == output.canonicalize()? {
        return Err(Error::InvalidArgument("CSF output must differ from the input directory".into()));
    }
    // Check every collision before processing the first tile, including dangling
    // symlinks: following them here could truncate a source or unrelated file.
    let staging = reserve_csf_names(output, &files)?;
    let mut paths = Vec::new();
    for file in files {
        let destination = output.join(file.file_name().expect("directory entry has a filename"));
        match destination.symlink_metadata() {
            Ok(_) => return Err(Error::InvalidArgument(format!("CSF output '{}' already exists", destination.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let input_path = file.to_str().ok_or_else(|| Error::InvalidArgument("CSF input filename is not UTF-8".into()))?.to_owned();
        let output_path = destination.to_str().ok_or_else(|| Error::InvalidArgument("CSF output filename is not UTF-8".into()))?.to_owned();
        paths.push((input_path, output_path));
    }
    let write_options = io.write.to_options(None)?;
    for (input_path, output_path) in paths {
        let (cloud, format) = read_path(&input_path, io)?;
        let mut tile_options = write_options.clone();
        if tile_options.format.is_none() {
            tile_options.format = if format == Some(pointcloud::PointFormat::Copc) && output_path.to_ascii_lowercase().ends_with(".laz") {
                Some(pointcloud::PointFormat::Copc)
            } else {
                pointcloud::PointFormat::for_path(&output_path)
            };
        }
        let classified = filters::csf(&cloud, options)?;
        publish_csf_tile(&classified, &output_path, &tile_options, staging.path())?;
    }
    Ok(())
}

/// Probe the output filesystem, including its case/Unicode filename rules,
/// before processing tiles. The reservations live only in a private directory.
fn reserve_csf_names(output: &std::path::Path, files: &[std::path::PathBuf]) -> terrio::Result<tempfile::TempDir> {
    let staging = tempfile::Builder::new().prefix(".terrio-csf-").tempdir_in(output)?;
    let names = staging.path().join("names");
    std::fs::create_dir(&names)?;
    for file in files {
        let name = file.file_name().expect("directory entry has a filename");
        match std::fs::OpenOptions::new().write(true).create_new(true).open(names.join(name)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(terrio::Error::InvalidArgument(format!(
                    "CSF output filenames collide on the output filesystem: '{}'",
                    file.display()
                )));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(staging)
}

fn publish_csf_tile(
    cloud: &PointCloud,
    output: &str,
    options: &pointcloud::PointWriteOptions,
    staging: &std::path::Path,
) -> terrio::Result<()> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("tile-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Match File::create permissions; the process umask still applies.
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let temporary = builder.tempfile_in(staging)?;
    let path = temporary.path().to_str().ok_or_else(|| terrio::Error::InvalidArgument("CSF output filename is not UTF-8".into()))?;
    pointcloud::write_points(cloud, path, options)?;
    // Exclusive publication also catches destinations created after preflight.
    // Dropping the temporary on any failure removes the incomplete tile.
    temporary.persist_noclobber(output).map_err(|error| terrio::Error::from(error.error))?;
    Ok(())
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
                // As when classifying: when every point would be an outlier, none is.
                if outliers.iter().all(|o| *o) {
                    return write(&io, &cloud);
                }
                let keep: Vec<bool> = outliers.iter().map(|o| !o).collect();
                return write(&io, &cloud.select(&keep));
            }
            write(&io, &filters::outlier(&cloud, &options)?)
        }
        Filter::Range { io, limits } => {
            // Ranges look at one point at a time: read and write in chunks.
            let ranges = DimRange::parse_list(&limits)?;
            let mut reader = pointcloud::open_points(&io.input)?;
            let mut template = reader.template()?;
            assign_srs(&io.a_srs, &mut template)?;
            let mut writer = PointWriter::create(&io.output, &template, &io.write.to_options(None)?)?;
            reader.start_chunks(&io.points.to_options()?, io.write.chunk_points)?;
            while let Some(chunk) = reader.next_chunk()? {
                writer.write(&filters::range(&chunk, &ranges)?)?;
            }
            writer.finish_with_waveforms(&mut reader)
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
        Filter::Pmf {
            io,
            cell,
            linear,
            initial_distance,
            max_distance,
            max_window,
            slope,
            ignore,
            pmf_returns,
            ground_class,
            other_class,
            only_ground,
        } => {
            let options = PmfOptions {
                cell,
                exponential: !linear,
                initial_distance,
                max_distance,
                max_window,
                slope,
                ignore: ignore.as_deref().map(DimRange::parse_list).transpose()?.unwrap_or_default(),
                returns: pmf_returns.iter().map(|r| ReturnKind::parse(r)).collect::<terrio::Result<_>>()?,
                ground_class,
                other_class,
                only_ground,
            };
            let cloud = read(&io)?;
            write(&io, &filters::pmf(&cloud, &options)?)
        }
        Filter::Csf {
            io,
            cloth_resolution,
            rigidness,
            step,
            threshold,
            hdiff,
            iterations,
            no_smooth,
            ignore,
            csf_returns,
            ground_class,
            other_class,
            only_ground,
        } => {
            let options = CsfOptions {
                smooth: !no_smooth,
                step,
                threshold,
                hdiff,
                resolution: cloth_resolution,
                rigidness,
                iterations,
                ignore: ignore.as_deref().map(DimRange::parse_list).transpose()?.unwrap_or_default(),
                returns: csf_returns.iter().map(|r| ReturnKind::parse(r)).collect::<terrio::Result<_>>()?,
                ground_class,
                other_class,
                only_ground,
            };
            run_csf(&io, &options)
        }
        Filter::Elm { io, cell, threshold, class } => {
            let cloud = read(&io)?;
            write(&io, &filters::elm(&cloud, cell, threshold, class)?)
        }
        Filter::HagNn { io, hag, count, max_distance } => {
            let options = HagOptions { count, max_distance, allow_extrapolation: !hag.no_extrapolation, class: hag.ground_class };
            let cloud = read(&io)?;
            write(&io, &filters::hag_nn(&cloud, &options)?)
        }
        Filter::HagDelaunay { io, hag, count } => {
            let options =
                HagOptions { count, allow_extrapolation: !hag.no_extrapolation, class: hag.ground_class, ..HagOptions::delaunay() };
            let cloud = read(&io)?;
            write(&io, &filters::hag_delaunay(&cloud, &options)?)
        }
        Filter::Crop { io, crop_bounds, polygon, point, distance, outside, geometry_srs } => {
            let options = CropOptions {
                bounds: crop_bounds.iter().map(|b| CropBox::parse(b)).collect::<terrio::Result<_>>()?,
                polygons: polygon,
                points: point.iter().map(|p| parse_point(p)).collect::<terrio::Result<_>>()?,
                distance,
                outside,
                a_srs: geometry_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose()?,
            };
            let cloud = read(&io)?;
            write(&io, &filters::crop(&cloud, &options)?)
        }
        Filter::Decimation { io, step, start, end } => {
            let cloud = read(&io)?;
            write(&io, &filters::decimation(&cloud, step, start, end)?)
        }
        Filter::Expression { io, expression } => {
            // A condition looks at one point at a time: read and write in chunks.
            let condition = filters::expr::Condition::parse(&expression)?;
            let mut reader = pointcloud::open_points(&io.input)?;
            let mut template = reader.template()?;
            assign_srs(&io.a_srs, &mut template)?;
            let mut writer = PointWriter::create(&io.output, &template, &io.write.to_options(None)?)?;
            reader.start_chunks(&io.points.to_options()?, io.write.chunk_points)?;
            while let Some(chunk) = reader.next_chunk()? {
                writer.write(&chunk.select(&condition.mask(&chunk)?))?;
            }
            writer.finish_with_waveforms(&mut reader)
        }
        Filter::Assign { io, assignment, condition, value } => {
            let options =
                AssignOptions { assignments: assignment, condition: condition.as_deref().map(DimRange::parse).transpose()?, values: value };
            let cloud = read(&io)?;
            write(&io, &filters::assign(&cloud, &options)?)
        }
        Filter::Merge { mut paths, a_srs, points, write: w } => {
            let output = paths.pop().unwrap_or_default();
            let paths = super::expand_file_lists(&paths)?;
            let read_opts = points.to_options()?;
            let mut clouds = paths.iter().map(|p| pointcloud::read_points(p, &read_opts)).collect::<terrio::Result<Vec<_>>>()?;
            for c in &mut clouds {
                assign_srs(&a_srs, c)?;
            }
            pointcloud::write_points(&filters::merge(&clouds)?, &output, &w.to_options(None)?)
        }
        Filter::Splitter { io, length, origin, buffer } => {
            let options = SplitterOptions { length, origin: origin.map(|o| [o[0], o[1]]), buffer };
            let cloud = read(&io)?;
            write_many(&io, filters::splitter(&cloud, &options)?.into_iter().map(|(_, c)| c).collect())
        }
        Filter::Chipper { io, capacity } => {
            let cloud = read(&io)?;
            write_many(&io, filters::chipper(&cloud, capacity)?)
        }
        Filter::Normal { io, knn, viewpoint, not_always_up } => {
            let options = NormalOptions { knn, viewpoint: viewpoint.map(|v| [v[0], v[1], v[2]]), always_up: !not_always_up };
            let cloud = read(&io)?;
            write(&io, &filters::normal(&cloud, &options)?)
        }
        Filter::Cluster { io, min_cluster_points, max_cluster_points, tolerance, two_d } => {
            let options = ClusterOptions {
                min_points: min_cluster_points,
                max_points: max_cluster_points.unwrap_or(u64::MAX),
                tolerance,
                is_3d: !two_d,
            };
            let cloud = read(&io)?;
            write(&io, &filters::cluster(&cloud, &options)?)
        }
        Filter::Dbscan { io, min_cluster_points, eps, dimensions } => {
            let options = DbscanOptions { min_points: min_cluster_points, eps, dimensions };
            let cloud = read(&io)?;
            write(&io, &filters::dbscan(&cloud, &options)?)
        }
        Filter::CovarianceFeatures { io, nb, knn, feature_set, mode, optimized } => {
            let options = CovarianceFeaturesOptions {
                neighbours: nb.to_neighbourhood(knn),
                feature_set,
                mode: FeatureMode::parse(&mode)?,
                optimized,
            };
            let cloud = read(&io)?;
            write(&io, &filters::covariance_features(&cloud, &options)?)
        }
        Filter::Eigenvalues { io, nb, knn, normalize } => {
            let options = EigenvaluesOptions { neighbours: nb.to_neighbourhood(knn), normalize };
            let cloud = read(&io)?;
            write(&io, &filters::eigenvalues(&cloud, &options)?)
        }
        Filter::ApproximateCoplanar { io, knn, thresh1, thresh2 } => {
            let cloud = read(&io)?;
            write(&io, &filters::approximate_coplanar(&cloud, knn, thresh1, thresh2)?)
        }
        Filter::EstimateRank { io, knn, thresh } => {
            let cloud = read(&io)?;
            write(&io, &filters::estimate_rank(&cloud, knn, thresh)?)
        }
        Filter::Lof { io, minpts } => {
            let cloud = read(&io)?;
            write(&io, &filters::lof(&cloud, minpts)?)
        }
        Filter::NnDistance { io, k, mode } => {
            let mode = NnDistanceMode::parse(&mode)?;
            let cloud = read(&io)?;
            write(&io, &filters::nndistance(&cloud, k, mode)?)
        }
        Filter::RadialDensity { io, radius } => {
            let cloud = read(&io)?;
            write(&io, &filters::radial_density(&cloud, radius)?)
        }
        Filter::Reciprocity { io, knn } => {
            let cloud = read(&io)?;
            write(&io, &filters::reciprocity(&cloud, knn)?)
        }
        Filter::Miniball { io, knn } => {
            let cloud = read(&io)?;
            write(&io, &filters::miniball(&cloud, knn)?)
        }
        Filter::Zsmooth { io, dim, radius, median_percent } => {
            let cloud = read(&io)?;
            write(&io, &filters::zsmooth(&cloud, &dim, radius, median_percent)?)
        }
        Filter::SkewnessBalancing { io, ground_class, other_class, only_ground } => {
            let cloud = read(&io)?;
            write(&io, &filters::skewness_balancing(&cloud, ground_class, other_class, only_ground)?)
        }
        Filter::Head { io, count, invert } => {
            let cloud = read(&io)?;
            write(&io, &filters::head(&cloud, count, invert))
        }
        Filter::Tail { io, count, invert } => {
            let cloud = read(&io)?;
            write(&io, &filters::tail(&cloud, count, invert))
        }
        Filter::Sort { io, dimensions, order } => {
            let descending = match order.to_ascii_lowercase().as_str() {
                "asc" => false,
                "desc" => true,
                _ => return Err(terrio::Error::InvalidArgument(format!("invalid order '{order}': use asc or desc"))),
            };
            let cloud = read(&io)?;
            write(&io, &filters::sort(&cloud, &dimensions, descending)?)
        }
        Filter::Locate { io, dimension, minmax } => {
            let min = match minmax.to_ascii_lowercase().as_str() {
                "min" => true,
                "max" => false,
                _ => return Err(terrio::Error::InvalidArgument(format!("invalid minmax '{minmax}': use min or max"))),
            };
            let cloud = read(&io)?;
            write(&io, &filters::locate(&cloud, &dimension, min)?)
        }
        Filter::Iqr { io, dimension, k } => {
            let cloud = read(&io)?;
            write(&io, &filters::iqr(&cloud, &dimension, k)?)
        }
        Filter::Mad { io, dimension, k, mad_multiplier } => {
            let cloud = read(&io)?;
            write(&io, &filters::mad(&cloud, &dimension, k, mad_multiplier)?)
        }
        Filter::Dem { io, raster, limits, band } => {
            let limits = DimRange::parse(&limits)?;
            let raster = super::quiet(|| terrio::open_raster(&raster))?;
            let cloud = read(&io)?;
            write(&io, &filters::dem(&cloud, &raster, &limits, band)?)
        }
        Filter::Colorization { io, raster, dimensions } => {
            let dimensions = BandDimension::parse_list(&dimensions)?;
            let raster = super::quiet(|| terrio::open_raster(&raster))?;
            let cloud = read(&io)?;
            write(&io, &filters::colorization(&cloud, &raster, &dimensions)?)
        }
        Filter::Overlay { io, datasource, dimension, column, layer } => {
            let options = terrio::VectorOpenOptions { layers: layer.into_iter().collect(), ..Default::default() };
            let ds = terrio::open_vector(&datasource, &options)?;
            let layer = ds.layers.first().ok_or_else(|| terrio::Error::InvalidArgument(format!("'{datasource}' has no layer")))?;
            let cloud = read(&io)?;
            write(&io, &filters::overlay(&cloud, layer, &OverlayOptions { dimension, column })?)
        }
        Filter::Hexbin {
            input,
            output,
            a_srs,
            points,
            edge_length,
            sample_size,
            threshold,
            no_smooth,
            hole_cull_area_tolerance,
            origin,
            precision,
            density,
            format,
            chunk_points,
        } => {
            let options = HexbinOptions {
                edge_length,
                sample_size,
                threshold,
                smooth: !no_smooth,
                hole_cull_area_tolerance,
                origin: origin.map(|o| [o[0], o[1]]),
                precision,
                ..Default::default()
            };
            // Hexagons count one point at a time: read in chunks.
            let mut binner = filters::HexBinner::new(&options)?;
            let mut reader = pointcloud::open_points(&input)?;
            reader.start_chunks(&points.to_options()?, chunk_points)?;
            while let Some(mut chunk) = reader.next_chunk()? {
                assign_srs(&a_srs, &mut chunk)?;
                binner.add(&chunk)?;
            }
            if let Some(s) = &a_srs {
                binner.set_crs(terrio::crs::SpatialRef::parse(s)?);
            } else {
                binner.set_crs(reader.crs().clone());
            }
            let result = binner.finish()?;
            let driver = format
                .as_deref()
                .map(|f| VectorDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown vector format '{f}'"))))
                .transpose()?;
            let write = terrio::VectorWriteOptions { driver, ..Default::default() };
            terrio::write_vector(&[result.boundary_layer(None)], &output, &write)?;
            if let Some(path) = density {
                terrio::write_vector(&[result.density_layer(None)], &path, &write)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, numbered, run};
    use clap::Parser;
    use terrio::crs::SpatialRef;
    use terrio::pointcloud::{self, Dimension, PointCloud, Values};

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn csf_classifies_each_las_in_a_directory_without_changing_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("Downloads with spaces");
        let output = input.join("Nova pasta");
        std::fs::create_dir(&input).unwrap();
        std::fs::create_dir(input.join("nested")).unwrap();
        std::fs::create_dir(input.join("ignored.las")).unwrap();
        std::fs::write(input.join("notes.txt"), "not a point cloud").unwrap();
        std::fs::write(input.join("._a.las"), "AppleDouble metadata, not a tile").unwrap();
        std::fs::write(input.join("nested/ignored.las"), "not scanned recursively").unwrap();
        let dims = vec![
            Dimension::new("x", Values::F64((0..64).map(|i| (i % 8) as f64).collect())),
            Dimension::new("y", Values::F64((0..64).map(|i| (i / 8) as f64).collect())),
            Dimension::new("z", Values::F64((0..64).map(|i| if i % 8 < 6 { 0.0 } else { 3.0 }).collect())),
        ];
        let cloud = PointCloud::new(dims, SpatialRef::from_epsg(32614).unwrap());
        let mut originals = Vec::new();
        for (index, name) in ["a.las", "b.LAZ"].iter().enumerate() {
            let path = input.join(name);
            let tile = if index == 0 { cloud.clone() } else { cloud.slice(0, 32) };
            pointcloud::write_points(&tile, path.to_str().unwrap(), &Default::default()).unwrap();
            originals.push(std::fs::read(path).unwrap());
        }
        let argv = ["terrio", "csf", "--cloth-resolution=2", input.to_str().unwrap(), output.to_str().unwrap()];
        run(Cli::try_parse_from(argv).unwrap().args).unwrap();
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 2);
        for (index, name) in ["a.las", "b.LAZ"].iter().enumerate() {
            assert_eq!(std::fs::read(input.join(name)).unwrap(), originals[index]);
            let single = dir.path().join(format!("single-{name}"));
            run(Cli::try_parse_from([
                "terrio",
                "csf",
                "--cloth-resolution=2",
                input.join(name).to_str().unwrap(),
                single.to_str().unwrap(),
            ])
            .unwrap()
            .args)
            .unwrap();
            let batch = pointcloud::read_points(output.join(name).to_str().unwrap(), &Default::default()).unwrap();
            let reference = pointcloud::read_points(single.to_str().unwrap(), &Default::default()).unwrap();
            assert_eq!(batch.len(), if index == 0 { 64 } else { 32 });
            assert_eq!(batch.crs.epsg, Some(32614));
            for dimension in ["x", "y", "z", "classification"] {
                assert_eq!(batch.dim(dimension).unwrap().to_f64(), reference.dim(dimension).unwrap().to_f64(), "{dimension}");
            }
        }
    }

    #[test]
    fn csf_directory_rejects_empty_input_and_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        std::fs::create_dir(&input).unwrap();
        let run_batch = |output: &std::path::Path| {
            run(Cli::try_parse_from(["terrio", "csf", input.to_str().unwrap(), output.to_str().unwrap()]).unwrap().args)
        };
        let error = run_batch(&output).unwrap_err().to_string();
        assert!(error.contains("LAS/LAZ"), "{error}");
        assert!(!output.exists());
        // Preflight must reject collisions before reading or writing any dataset.
        std::fs::write(input.join("a.las"), "input a").unwrap();
        std::fs::write(input.join("b.las"), "input b").unwrap();
        let error = run_batch(std::path::Path::new("s3://bucket/classified")).unwrap_err().to_string();
        assert!(error.contains("local directory"), "{error}");
        let ept_output = dir.path().join("ept-out");
        let error = run_batch(std::path::Path::new(&format!("ept://{}", ept_output.display()))).unwrap_err().to_string();
        assert!(error.contains("local directory"), "{error}");
        assert!(!ept_output.exists());
        assert!(!std::path::Path::new("ept:").exists());
        let error = run_batch(&input).unwrap_err().to_string();
        assert!(error.contains("input directory"), "{error}");
        let error = run_batch(&input.join("new/..")).unwrap_err().to_string();
        assert!(error.contains("input directory"), "{error}");
        std::fs::write(&output, "an existing file").unwrap();
        let error = run_batch(&output).unwrap_err().to_string();
        assert!(error.contains("directory"), "{error}");
        std::fs::remove_file(&output).unwrap();
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("b.las"), "keep this result").unwrap();
        let error = run_batch(&output).unwrap_err().to_string();
        assert!(error.contains("already exists"), "{error}");
        assert!(!output.join("a.las").exists());
        assert_eq!(std::fs::read_to_string(output.join("b.las")).unwrap(), "keep this result");
        assert_eq!(std::fs::read_to_string(input.join("a.las")).unwrap(), "input a");
    }

    #[test]
    fn csf_keeps_copc_content_in_an_ordinary_laz_filename_and_accepts_zip_directory() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::create_dir(&input).unwrap();
        let output = dir.path().join("tiles.zip");
        let cloud = PointCloud::new(
            vec![
                Dimension::new("x", Values::F64(vec![0., 1., 2., 3.])),
                Dimension::new("y", Values::F64(vec![0., 1., 0., 1.])),
                Dimension::new("z", Values::F64(vec![0.; 4])),
            ],
            SpatialRef::default(),
        );
        pointcloud::write_points(
            &cloud,
            input.join("tile.laz").to_str().unwrap(),
            &pointcloud::PointWriteOptions { format: Some(pointcloud::PointFormat::Copc), ..Default::default() },
        )
        .unwrap();
        std::fs::copy(input.join("tile.laz"), input.join("tile.las")).unwrap();
        run(Cli::try_parse_from(["terrio", "csf", input.to_str().unwrap(), output.to_str().unwrap()]).unwrap().args).unwrap();
        let mut las_reader = pointcloud::open_points(output.join("tile.las").to_str().unwrap()).unwrap();
        assert_eq!(las_reader.format_name(), "LAS");
        assert_eq!(las_reader.read(&Default::default()).unwrap().len(), 4);
        let mut reader = pointcloud::open_points(output.join("tile.laz").to_str().unwrap()).unwrap();
        assert_eq!(reader.format_name(), "COPC");
        let result = reader.read(&Default::default()).unwrap();
        assert_eq!(result.len(), 4);
        assert!(result.dim("classification").is_some());
        // Single-file mode is an explicit conversion: its output extension wins.
        let single = dir.path().join("single.laz");
        run(Cli::try_parse_from(["terrio", "csf", input.join("tile.laz").to_str().unwrap(), single.to_str().unwrap()]).unwrap().args)
            .unwrap();
        let mut reader = pointcloud::open_points(single.to_str().unwrap()).unwrap();
        assert_eq!(reader.format_name(), "LAZ");
        let reference = reader.read(&Default::default()).unwrap();
        for dimension in ["x", "y", "z", "classification"] {
            let mut batch = result.dim(dimension).unwrap().to_f64();
            let mut single = reference.dim(dimension).unwrap().to_f64();
            batch.sort_by(f64::total_cmp);
            single.sort_by(f64::total_cmp);
            assert_eq!(batch, single, "{dimension}");
        }
    }

    #[test]
    fn csf_stages_failed_writes_and_never_replaces_a_late_destination() {
        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir_in(dir.path()).unwrap();
        let output = dir.path().join("tile.las");
        let cloud = PointCloud::new(
            vec![
                Dimension::new("x", Values::F64(vec![0.])),
                Dimension::new("y", Values::F64(vec![0.])),
                Dimension::new("z", Values::F64(vec![0.])),
            ],
            SpatialRef::default(),
        );
        let options = pointcloud::PointWriteOptions { format: Some(pointcloud::PointFormat::Las), ..Default::default() };
        let invalid = pointcloud::PointWriteOptions { point_format: Some(255), ..options.clone() };
        assert!(super::publish_csf_tile(&cloud, output.to_str().unwrap(), &invalid, staging.path()).is_err());
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
        // Simulate a destination created after the batch's preflight.
        std::fs::write(&output, "a concurrent result").unwrap();
        assert!(super::publish_csf_tile(&cloud, output.to_str().unwrap(), &options, staging.path()).is_err());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "a concurrent result");
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
        std::fs::remove_file(&output).unwrap();
        super::publish_csf_tile(&cloud, output.to_str().unwrap(), &options, staging.path()).unwrap();
        assert_eq!(pointcloud::read_points(output.to_str().unwrap(), &Default::default()).unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ordinary = dir.path().join("plain.las");
            std::fs::write(&ordinary, "normal file-create permissions").unwrap();
            assert_eq!(output.metadata().unwrap().permissions().mode() & 0o777, ordinary.metadata().unwrap().permissions().mode() & 0o777,);
        }
    }

    #[test]
    fn csf_mid_batch_error_keeps_completed_tiles_and_removes_staging() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        std::fs::create_dir(&input).unwrap();
        let cloud = PointCloud::new(
            vec![
                Dimension::new("x", Values::F64(vec![0., 1., 2., 3.])),
                Dimension::new("y", Values::F64(vec![0., 1., 0., 1.])),
                Dimension::new("z", Values::F64(vec![0.; 4])),
            ],
            SpatialRef::default(),
        );
        pointcloud::write_points(&cloud, input.join("a.las").to_str().unwrap(), &Default::default()).unwrap();
        std::fs::write(input.join("b.las"), "invalid second tile").unwrap();
        let original = std::fs::read(input.join("a.las")).unwrap();
        let result = run(Cli::try_parse_from(["terrio", "csf", input.to_str().unwrap(), output.to_str().unwrap()]).unwrap().args);
        assert!(result.is_err());
        let completed = pointcloud::read_points(output.join("a.las").to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(completed.len(), 4);
        assert!(completed.dim("classification").is_some());
        assert!(!output.join("b.las").exists());
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
        assert_eq!(std::fs::read(input.join("a.las")).unwrap(), original);
        assert_eq!(std::fs::read_to_string(input.join("b.las")).unwrap(), "invalid second tile");
    }

    #[test]
    fn csf_name_reservations_follow_the_output_filesystem_rules() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe");
        std::fs::write(&probe, "").unwrap();
        let case_sensitive = !dir.path().join("PROBE").exists();
        std::fs::remove_file(&probe).unwrap();
        let files = [std::path::PathBuf::from("a.las"), std::path::PathBuf::from("A.las")];
        let result = super::reserve_csf_names(dir.path(), &files);
        assert_eq!(result.is_ok(), case_sensitive);
        drop(result);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // Always exercise the collision error, including on case-sensitive hosts.
        let result = super::reserve_csf_names(dir.path(), &[files[0].clone(), files[0].clone()]);
        assert!(result.unwrap_err().to_string().contains("collide"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn csf_skips_source_symlinks_and_refuses_destination_symlinks_and_non_utf8_names() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        std::fs::create_dir(&input).unwrap();
        let run_batch = || run(Cli::try_parse_from(["terrio", "csf", input.to_str().unwrap(), output.to_str().unwrap()]).unwrap().args);
        symlink("missing", input.join("ignored.las")).unwrap();
        let error = run_batch().unwrap_err().to_string();
        assert!(error.contains("no regular LAS/LAZ"), "{error}");
        std::fs::write(input.join("a.las"), "not read before preflight").unwrap();
        std::fs::create_dir(&output).unwrap();
        symlink("missing", output.join("a.las")).unwrap();
        assert!(run_batch().unwrap_err().to_string().contains("already exists"));
        assert!(output.join("a.las").symlink_metadata().unwrap().file_type().is_symlink());
        std::fs::remove_file(output.join("a.las")).unwrap();
        let name = std::ffi::OsString::from_vec(b"invalid-\xff.las".to_vec());
        std::fs::write(input.join(name), "invalid UTF-8 name").unwrap();
        assert!(run_batch().unwrap_err().to_string().contains("not UTF-8"));
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 0);
    }

    #[test]
    fn file_only_filters_report_directory_inputs_before_creating_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("Downloads");
        std::fs::create_dir(&input).unwrap();
        for (filter, extra) in [
            ("smrf", vec![]),
            ("range", vec!["--limits", "Classification[2:2]"]),
            ("decimation", vec!["--step", "2"]),
            ("merge", vec![]),
            ("hexbin", vec![]),
        ] {
            let output = dir.path().join(format!("{filter}.las"));
            let mut argv = vec!["terrio", filter, input.to_str().unwrap(), output.to_str().unwrap()];
            argv.extend(extra);
            let error = run(Cli::try_parse_from(argv).unwrap().args).unwrap_err();
            assert!(matches!(error, terrio::Error::InvalidArgument(_)), "{filter}: {error:?}");
            assert!(error.to_string().contains("select a point-cloud file"), "{filter}: {error}");
            assert!(!output.exists(), "{filter} created an output");
        }
    }

    #[test]
    fn numbers_the_file_name_only() {
        assert_eq!(numbered("out/tile_#.laz", 3).as_deref(), Some("out/tile_3.laz"));
        assert_eq!(numbered("out#1/tile_#.laz", 2).as_deref(), Some("out#1/tile_2.laz"));
        assert_eq!(numbered("a#b#.laz", 4).as_deref(), Some("a#b4.laz"));
        assert_eq!(numbered("out#1/tile.laz", 1), None);
        assert_eq!(numbered("tile.laz", 1), None);
    }

    #[test]
    fn runs_the_neighbourhood_raster_and_vector_filters() {
        let dir = std::env::temp_dir().join(format!("terrio-filter-more-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        // A 20 x 20 grid of points 1 m apart on a tilted plane, without a CRS (the GeoJSON polygon is
        // then taken to be in the points' CRS).
        let n = 400;
        let xs: Vec<f64> = (0..n).map(|i| 1000.0 + (i % 20) as f64).collect();
        let ys: Vec<f64> = (0..n).map(|i| 2000.0 + (i / 20) as f64).collect();
        let zs: Vec<f64> = (0..n).map(|i| 10.0 + 0.1 * (i % 20) as f64).collect();
        let dims = vec![
            Dimension::new("x", Values::F64(xs)),
            Dimension::new("y", Values::F64(ys)),
            Dimension::new("z", Values::F64(zs)),
            Dimension::new("classification", Values::U8(vec![1; n])),
        ];
        let input = path("in.las");
        pointcloud::write_points(&PointCloud::new(dims, SpatialRef::default()), &input, &Default::default()).unwrap();
        // A raster of the plane (a row below the points: the bottom edge is outside), and a polygon
        // over half the points.
        let cfg = terrio::RasterConfig { cols: 20, rows: 21, x_min: 1000.0, y_min: 1999.0, cell_size: 1.0, ..Default::default() };
        let surface: Vec<f64> = (0..n + 20).map(|i| 10.05 + 0.1 * (i % 20) as f64).collect();
        let raster = path("plane.tif");
        terrio::write_raster(&terrio::Raster::from_data(cfg, surface).unwrap(), &raster, &Default::default()).unwrap();
        let geojson = path("zone.geojson");
        std::fs::write(
            &geojson,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"a","zone":5},
               "geometry":{"type":"Polygon","coordinates":[[[999,1999],[1009.5,1999],[1009.5,2030],[999,2030],[999,1999]]]}}]}"#,
        )
        .unwrap();
        let run_filter = |filter: &str, extra: &[&str]| {
            let output = path(&format!("{filter}.las"));
            let mut argv = vec!["terrio", filter, input.as_str(), output.as_str()];
            argv.extend(extra);
            run(Cli::try_parse_from(argv).unwrap().args).unwrap();
            pointcloud::read_points(&output, &Default::default()).unwrap()
        };
        let c = run_filter("dbscan", &["--eps", "1.5", "--min-cluster-points", "3"]);
        assert!(c.dim("cluster_id").unwrap().to_f64().iter().all(|v| *v == 0.0));
        let c = run_filter("covariancefeatures", &["--feature-set", "Linearity,Planarity", "--mode", "raw"]);
        assert!(c.dim("planarity").is_some() && c.dim("scattering").is_none());
        for (filter, extra, dim) in [
            ("eigenvalues", vec!["--knn", "6"], "eigenvalue2"),
            ("approximatecoplanar", vec![], "coplanar"),
            ("estimaterank", vec![], "rank"),
            ("lof", vec!["--minpts", "5"], "local_outlier_factor"),
            ("nndistance", vec!["-k", "3", "--mode", "avg"], "nndistance"),
            ("radialdensity", vec!["--radius", "2"], "radial_density"),
            ("reciprocity", vec![], "reciprocity"),
            ("miniball", vec![], "miniball"),
            ("zsmooth", vec!["--dim", "Smoothed", "--radius", "1.5"], "smoothed"),
        ] {
            assert!(run_filter(filter, &extra).dim(dim).is_some(), "{filter}");
        }
        assert_eq!(run_filter("head", &["--count", "7"]).len(), 7);
        assert_eq!(run_filter("tail", &["--count", "7", "--invert"]).len(), n - 7);
        assert_eq!(run_filter("locate", &["--dimension", "Z", "--minmax", "min"]).len(), 1);
        assert_eq!(run_filter("iqr", &["--dimension", "Z"]).len(), n);
        assert!(run_filter("mad", &["--dimension", "Z", "-k", "1"]).len() < n);
        let sorted = run_filter("sort", &["--dimensions", "Z", "--order", "desc"]).dim("z").unwrap().to_f64();
        assert!(sorted.windows(2).all(|w| w[0] >= w[1]));
        assert_eq!(run_filter("skewnessbalancing", &[]).len(), n);
        assert_eq!(run_filter("dem", &["--raster", &raster, "--limits", "Z[0.1:0.1]"]).len(), n);
        let c = run_filter("colorization", &["--raster", &raster, "--dimensions", "Intensity:1:10"]);
        assert!(c.dim("intensity").unwrap().to_f64().iter().all(|v| *v >= 100.0));
        let c = run_filter("overlay", &["--datasource", &geojson, "--dimension", "Classification", "--column", "zone"]);
        let classes = c.dim("classification").unwrap().to_f64();
        assert_eq!(classes.iter().filter(|v| **v == 5.0).count(), 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_the_hexbin_boundary_and_density() {
        let dir = std::env::temp_dir().join(format!("terrio-filter-hexbin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        // A 20 x 20 grid of points 0.5 m apart, in UTM.
        let n = 400;
        let dims = vec![
            Dimension::new("x", Values::F64((0..n).map(|i| 500_000.0 + 0.5 * (i % 20) as f64).collect())),
            Dimension::new("y", Values::F64((0..n).map(|i| 3_300_000.0 + 0.5 * (i / 20) as f64).collect())),
            Dimension::new("z", Values::F64(vec![10.0; n])),
        ];
        let input = path("in.las");
        pointcloud::write_points(&PointCloud::new(dims, SpatialRef::from_epsg(32614).unwrap()), &input, &Default::default()).unwrap();
        let (boundary, density) = (path("boundary.geojson"), path("hexagons.geojson"));
        let argv = ["terrio", "hexbin", &input, &boundary, "--edge-size", "1.4", "--threshold", "3", "--no-smooth", "--density", &density];
        run(Cli::try_parse_from(argv).unwrap().args).unwrap();
        let b = terrio::open_vector(&boundary, &Default::default()).unwrap();
        let layer = &b.layers[0];
        assert_eq!(layer.features.len(), 1);
        assert!(matches!(layer.features[0].geometry, Some(terrio::Geometry::MultiPolygon(_))));
        assert_eq!(SpatialRef::from_layer(layer).epsg, Some(32614));
        let d = terrio::open_vector(&density, &Default::default()).unwrap();
        assert!(!d.layers[0].features.is_empty());
        // The edge length estimated, as PDAL's (overwriting the boundary).
        std::fs::remove_file(&boundary).unwrap();
        run(Cli::try_parse_from(["terrio", "hexbin", &input, &boundary, "--threshold", "3"]).unwrap().args).unwrap();
        let b = terrio::open_vector(&boundary, &Default::default()).unwrap();
        let edge = &b.layers[0].features[0].attributes[5];
        // hexer's height: 3 × (380 steps of 0.5 and 19 row jumps) / 400 points.
        let height = 3.0 * (380.0 * 0.5 + 19.0 * (9.5f64 * 9.5 + 0.25).sqrt()) / 400.0;
        assert!(matches!(edge, terrio::FieldValue::Float(e) if (*e - height / 3f64.sqrt()).abs() < 1e-9), "{edge:?}");
        // --format applies to both outputs, whose extensions do not name a format.
        let (boundary, density) = (path("boundary.out"), path("hexagons.out"));
        let argv =
            ["terrio", "hexbin", &input, &boundary, "--edge-size", "1.4", "--threshold", "3", "-f", "GeoJSON", "--density", &density];
        run(Cli::try_parse_from(argv).unwrap().args).unwrap();
        for (out, features) in [(&boundary, 1), (&density, d.layers[0].features.len())] {
            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap();
            assert_eq!(json["type"], "FeatureCollection", "{out}");
            assert_eq!(json["features"].as_array().unwrap().len(), features, "{out}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assigns_a_crs_to_points_without_one() {
        let dir = std::env::temp_dir().join(format!("terrio-filter-a-srs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.las").to_string_lossy().into_owned();
        let coords = |base: f64| Values::F64((0..10).map(|i| base + i as f64).collect());
        let dims = vec![
            Dimension::new("x", coords(2_830_000.0)),
            Dimension::new("y", coords(1_168_000.0)),
            Dimension::new("z", coords(500.0)),
            Dimension::new("classification", Values::U8(vec![2; 10])),
        ];
        pointcloud::write_points(&PointCloud::new(dims, SpatialRef::default()), &input, &Default::default()).unwrap();
        // Filters reading the points whole, in chunks, and from several files.
        for (filter, extra) in [("decimation", vec!["--step", "2"]), ("range", vec!["--limits", "Classification[2:2]"]), ("merge", vec![])]
        {
            let output = dir.join(format!("{filter}.las")).to_string_lossy().into_owned();
            let mut argv = vec!["terrio", filter, &input];
            if filter == "merge" {
                argv.push(&input);
            }
            argv.extend([output.as_str(), "--a-srs", "EPSG:2056"]);
            argv.extend(extra);
            run(Cli::try_parse_from(argv).unwrap().args).unwrap();
            assert_eq!(pointcloud::open_points(&output).unwrap().crs().epsg, Some(2056), "{filter}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merges_point_clouds_listed_in_a_file() {
        let dir = std::env::temp_dir().join(format!("terrio-filter-merge-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let coords = |base: f64| Values::F64((0..10).map(|i| base + i as f64).collect());
        for (name, base) in [("a.las", 0.0), ("b.las", 100.0)] {
            let dims = vec![Dimension::new("x", coords(base)), Dimension::new("y", coords(base)), Dimension::new("z", coords(base))];
            pointcloud::write_points(&PointCloud::new(dims, SpatialRef::default()), &path(name), &Default::default()).unwrap();
        }
        std::fs::write(dir.join("list.txt"), format!("{}\n{}\n", path("a.las"), path("b.las"))).unwrap();
        let (list, output) = (format!("@{}", path("list.txt")), path("merged.las"));
        run(Cli::try_parse_from(["terrio", "merge", &list, &output]).unwrap().args).unwrap();
        assert_eq!(pointcloud::read_points(&output, &Default::default()).unwrap().len(), 20);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
