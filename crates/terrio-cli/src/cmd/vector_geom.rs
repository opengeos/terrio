//! Vector geometry commands, after GDAL 3.13's `gdal vector simplify`, `segmentize`,
//! `convex-hull`, `concave-hull`, `swap-xy`, `set-geom-type`, `combine` and `sort`.

use super::processing::{VectorInput, VectorOutput};
use terrio::formats::VectorDriver;
use terrio::ops::vector_geom::{
    self, CombineOptions, ConcaveHullOptions, Dimension, ExtraFields, GeometryOp, GeometryTypeSpec, SetGeomTypeOptions, SortMethod,
};
use terrio::{Error, Result};

/// Runs a per-feature operation: a chunk of features at a time when the formats allow it (as
/// `terrio convert` streams), otherwise on whole layers.
fn run_op(input: &VectorInput, output: &VectorOutput, op: GeometryOp) -> Result<()> {
    op.validate()?;
    let driver = output
        .format
        .as_deref()
        .map(|f| VectorDriver::from_name(f).ok_or_else(|| Error::InvalidArgument(format!("unknown vector format '{f}'"))))
        .transpose()?;
    if !output.append && !output.overwrite {
        let options = terrio::stream::ConvertOptions {
            open: terrio::VectorOpenOptions { layers: input.layers.clone(), ..Default::default() },
            translate: terrio::ops::vector_filter::VectorTranslateOptions {
                layer_name: output.nln.clone(),
                geometry_op: Some(op.clone()),
                strict_geometry_op: true,
                ..Default::default()
            },
            write: terrio::VectorWriteOptions { driver, ..Default::default() },
            ..Default::default()
        };
        if terrio::stream::can_convert_stream(&input.input, &output.output, &options) {
            return super::quiet(|| terrio::stream::convert(&input.input, &output.output, &options));
        }
    }
    let layers = input.open()?.iter().map(|l| vector_geom::apply(l, &op)).collect::<Result<Vec<_>>>()?;
    output.write(layers)
}

#[derive(clap::Args)]
pub struct SimplifyArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Distance tolerance in CRS units (degrees in a geographic CRS), at least 0.
    #[arg(long, allow_negative_numbers = true)]
    tolerance: f64,
}

pub fn simplify(a: SimplifyArgs) -> Result<()> {
    run_op(&a.input, &a.output, GeometryOp::Simplify { tolerance: a.tolerance })
}

#[derive(clap::Args)]
pub struct SegmentizeArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Maximum segment length in CRS units (degrees in a geographic CRS), above 0.
    #[arg(long = "max-length", allow_negative_numbers = true)]
    max_length: f64,
}

pub fn segmentize(a: SegmentizeArgs) -> Result<()> {
    run_op(&a.input, &a.output, GeometryOp::Segmentize { max_length: a.max_length })
}

#[derive(clap::Args)]
pub struct HullArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
}

pub fn convex_hull(a: HullArgs) -> Result<()> {
    run_op(&a.input, &a.output, GeometryOp::ConvexHull)
}

#[derive(clap::Args)]
pub struct ConcaveHullArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Concavity from 0 (most concave; the polygons themselves for polygons) to 1 (convex hull).
    #[arg(long, allow_negative_numbers = true)]
    ratio: f64,
    /// Allow holes in the hulls.
    #[arg(long = "allow-holes")]
    allow_holes: bool,
    /// For polygons: make the hull follow the outer boundaries of the input polygons.
    #[arg(long)]
    tight: bool,
}

pub fn concave_hull(a: ConcaveHullArgs) -> Result<()> {
    let options = ConcaveHullOptions { ratio: a.ratio, allow_holes: a.allow_holes, tight: a.tight };
    run_op(&a.input, &a.output, GeometryOp::ConcaveHull(options))
}

pub fn swap_xy(a: HullArgs) -> Result<()> {
    run_op(&a.input, &a.output, GeometryOp::SwapXy)
}

#[derive(clap::Args)]
pub struct SetGeomTypeArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Force geometries and the layer to this type: POINT, LINESTRING, POLYGON, MULTIPOINT,
    /// MULTILINESTRING, MULTIPOLYGON, GEOMETRYCOLLECTION or GEOMETRY, with an optional Z, M or ZM.
    #[arg(long = "geometry-type", value_name = "TYPE", conflicts_with_all = ["multi", "single", "linear", "curve", "dim"])]
    geometry_type: Option<String>,
    /// Force single-part types to multi-part ones.
    #[arg(long, conflicts_with = "single")]
    multi: bool,
    /// Force multi-part types to single-part ones.
    #[arg(long)]
    single: bool,
    /// Convert curve geometries to linear ones (terrio's geometries are all linear).
    #[arg(long, conflicts_with = "curve")]
    linear: bool,
    /// Convert linear geometries to curves (not supported: terrio has no curve geometries).
    #[arg(long)]
    curve: bool,
    /// Force the coordinate dimensions: XY, XYZ, XYM or XYZM (added Z and M values are 0).
    #[arg(long, value_name = "DIM")]
    dim: Option<String>,
    /// Only change the layer's declared geometry type.
    #[arg(long = "layer-only", conflicts_with = "feature_only")]
    layer_only: bool,
    /// Only change the features' geometries.
    #[arg(long = "feature-only")]
    feature_only: bool,
    /// Skip the features whose geometry cannot be converted to the target type.
    #[arg(long)]
    skip: bool,
}

pub fn set_geom_type(a: SetGeomTypeArgs) -> Result<()> {
    let options = SetGeomTypeOptions {
        geometry_type: a.geometry_type.as_deref().map(GeometryTypeSpec::parse).transpose()?,
        multi: a.multi,
        single: a.single,
        linear: a.linear,
        curve: a.curve,
        dim: a.dim.as_deref().map(Dimension::parse).transpose()?,
        layer_only: a.layer_only,
        feature_only: a.feature_only,
        skip: a.skip,
    };
    run_op(&a.input, &a.output, GeometryOp::SetGeomType(options))
}

#[derive(clap::Args)]
pub struct CombineArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Combine the features with the same values of these fields (repeat or comma-separate).
    /// By default, each layer becomes one feature.
    #[arg(long = "group-by", value_delimiter = ',', value_name = "FIELD")]
    group_by: Vec<String>,
    /// Add multi-part geometries whole, as members of a geometry collection.
    #[arg(long = "keep-nested")]
    keep_nested: bool,
    /// Also copy other fields: no, sometimes-identical (identical within some group; null
    /// elsewhere) or always-identical (identical within every group).
    #[arg(long = "add-extra-fields", default_value = "no", value_name = "MODE")]
    add_extra_fields: String,
}

pub fn combine(a: CombineArgs) -> Result<()> {
    let options = CombineOptions {
        group_by: a.group_by.iter().map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect(),
        keep_nested: a.keep_nested,
        add_extra_fields: ExtraFields::parse(&a.add_extra_fields)?,
    };
    let layers = a.input.open()?.iter().map(|l| vector_geom::combine(l, &options)).collect::<Result<Vec<_>>>()?;
    a.output.write(layers)
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Method {
    Hilbert,
    Strtree,
}

#[derive(clap::Args)]
pub struct SortArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// hilbert: by the Hilbert code of the bounding box centres; strtree: in the leaf order of
    /// a sort-tile-recursive tree. Null and empty geometries go last.
    #[arg(long, value_enum, default_value_t = Method::Hilbert)]
    method: Method,
}

pub fn sort(a: SortArgs) -> Result<()> {
    let method = match a.method {
        Method::Hilbert => SortMethod::Hilbert,
        Method::Strtree => SortMethod::StrTree,
    };
    let layers = a.input.open()?.iter().map(|l| vector_geom::sort(l, method)).collect();
    a.output.write(layers)
}
