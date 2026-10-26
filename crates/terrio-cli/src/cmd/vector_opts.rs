//! The `ogr2ogr`-style options of `terrio convert` for vector inputs.

use terrio::crs::SpatialRef;
use terrio::ops::vector_filter::{GeometryConversion, VectorTranslateOptions, WriteMode};

#[derive(clap::Args, Default)]
pub struct VectorArgs {
    /// Coordinate operation options: ALLOW_BALLPARK=YES|NO, ONLY_BEST=YES|NO, PROMOTE_TO_3D=YES|NO.
    #[arg(long = "ct-opt", visible_aliases = ["ct_opt", "to-option"], value_name = "KEY=VALUE", help_heading = "Vector options")]
    transform_options: Vec<String>,
    /// Keep the features matching this OGR SQL expression, such as "pop > 1e6 AND name LIKE 'A%'".
    #[arg(long = "where", value_name = "EXPR", help_heading = "Vector options")]
    where_clause: Option<String>,
    /// Keep the features intersecting this rectangle.
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true, help_heading = "Vector options")]
    spat: Option<Vec<f64>>,
    /// The CRS of the --spat rectangle. By default, that of the layer.
    #[arg(long = "spat-srs", value_name = "CRS", help_heading = "Vector options")]
    spat_srs: Option<String>,
    /// Keep only these fields, in this order (comma-separated).
    #[arg(long, value_name = "FIELDS", help_heading = "Vector options")]
    select: Option<String>,
    /// Keep at most this many features per layer.
    #[arg(long, value_name = "N", help_heading = "Vector options")]
    limit: Option<usize>,
    /// Read the result of "SELECT fields FROM layer [WHERE ...] [ORDER BY field [DESC]] [LIMIT n]".
    #[arg(long, value_name = "STATEMENT", help_heading = "Vector options")]
    sql: Option<String>,
    /// The output layer name.
    #[arg(long, value_name = "NAME", help_heading = "Vector options")]
    nln: Option<String>,
    /// Convert geometries: PROMOTE_TO_MULTI, POINT, LINESTRING, POLYGON, MULTIPOINT, MULTILINESTRING,
    /// MULTIPOLYGON, GEOMETRYCOLLECTION, GEOMETRY or CONVERT_TO_LINEAR.
    #[arg(long, value_name = "TYPE", help_heading = "Vector options")]
    nlt: Option<String>,
    /// Write each part of multi-part geometries as its own feature.
    #[arg(long, help_heading = "Vector options")]
    explodecollections: bool,
    /// Simplify geometries with this tolerance, preserving topology, before reprojection
    /// (ogr2ogr -simplify). A simplification that fails keeps the geometry.
    #[arg(long, value_name = "TOLERANCE", conflicts_with = "segmentize", allow_negative_numbers = true, help_heading = "Vector options")]
    simplify: Option<f64>,
    /// Add vertices so that no segment is longer than this, before reprojection (ogr2ogr
    /// -segmentize).
    #[arg(long, value_name = "MAX_LENGTH", allow_negative_numbers = true, help_heading = "Vector options")]
    segmentize: Option<f64>,
    /// The CRS of the input, overriding what it declares.
    #[arg(long = "s-srs", value_name = "CRS", help_heading = "Vector options")]
    s_srs: Option<String>,
    /// Assign this CRS to the output, without reprojecting.
    #[arg(long = "a-srs", value_name = "CRS", help_heading = "Vector options")]
    a_srs: Option<String>,
    /// Add the features to the layers of an existing output (a GeoPackage, or the tiles of a
    /// PMTiles or MBTiles archive); for a raster, add it to an existing archive of image tiles;
    /// for a point cloud, add its points to an existing EPT dataset (an ept.json output).
    #[arg(long, conflicts_with = "overwrite", help_heading = "Vector options")]
    append: bool,
    /// Replace the layers of the same name in an existing output (other GeoPackage layers stay).
    #[arg(long, help_heading = "Vector options")]
    overwrite: bool,
}

impl VectorArgs {
    pub fn has_transform_options(&self) -> bool {
        !self.transform_options.is_empty()
    }

    pub fn coordinate_options(&self) -> terrio::Result<terrio::crs::TransformOptions> {
        coordinate_options(&self.transform_options)
    }

    /// Whether any vector-only option is given (coordinate controls also apply to rasters).
    pub fn is_set(&self) -> bool {
        self.where_clause.is_some()
            || self.spat.is_some()
            || self.select.is_some()
            || self.limit.is_some()
            || self.sql.is_some()
            || self.nln.is_some()
            || self.nlt.is_some()
            || self.explodecollections
            || self.simplify.is_some()
            || self.segmentize.is_some()
            || self.s_srs.is_some()
            || self.a_srs.is_some()
            || self.append
            || self.overwrite
    }

    /// The `--spat` rectangle, for the drivers of `input` that can skip features outside it
    /// while reading (GeoPackage, GeoParquet), when it is in the layer's CRS.
    pub fn spatial_hint(&self, input: &str) -> Option<(f64, f64, f64, f64)> {
        self.to_options(None).ok()?.read_filter(input)
    }

    /// The library options, with `t_srs` as the reprojection target.
    pub fn to_options(&self, t_srs: Option<SpatialRef>) -> terrio::Result<VectorTranslateOptions> {
        let srs = |s: &Option<String>| s.as_deref().map(SpatialRef::parse).transpose();
        Ok(VectorTranslateOptions {
            where_clause: self.where_clause.clone(),
            sql: self.sql.clone(),
            spatial_filter: self.spat.as_deref().map(|v| (v[0], v[1], v[2], v[3])),
            spatial_filter_srs: srs(&self.spat_srs)?,
            select: self.select.as_deref().map(|s| s.split(',').map(|f| f.trim().to_string()).collect()),
            limit: self.limit,
            layer_name: self.nln.clone(),
            geometry_type: self.nlt.as_deref().map(GeometryConversion::parse).transpose()?,
            explode_collections: self.explodecollections,
            geometry_op: self.geometry_op(),
            strict_geometry_op: false,
            src_srs: srs(&self.s_srs)?,
            dst_srs: t_srs,
            assign_srs: srs(&self.a_srs)?,
            transform_options: self.coordinate_options()?,
        })
    }

    /// Whether `--append` is the only option given (it also applies to rasters added to tile
    /// archives).
    pub fn append_only(&self) -> bool {
        self.append
            && self.transform_options.is_empty()
            && self.where_clause.is_none()
            && self.spat.is_none()
            && self.select.is_none()
            && self.limit.is_none()
            && self.sql.is_none()
            && self.nln.is_none()
            && self.nlt.is_none()
            && !self.explodecollections
            && self.simplify.is_none()
            && self.segmentize.is_none()
            && self.s_srs.is_none()
            && self.a_srs.is_none()
    }

    /// `--simplify` or `--segmentize`, which ogr2ogr ignores unless positive.
    fn geometry_op(&self) -> Option<terrio::ops::vector_geom::GeometryOp> {
        use terrio::ops::vector_geom::GeometryOp;
        GeometryOp::from_ogr2ogr(self.simplify, self.segmentize)
    }

    /// How to treat an existing output.
    pub fn write_mode(&self) -> WriteMode {
        if self.append {
            WriteMode::Append
        } else if self.overwrite {
            WriteMode::Overwrite
        } else {
            WriteMode::Create
        }
    }
}

/// Dedicated coordinate-operation flags reject typos; warp's mixed --to list remains permissive.
pub(super) fn coordinate_options(items: &[String]) -> terrio::Result<terrio::crs::TransformOptions> {
    terrio::crs::TransformOptions::from_strict_options(&super::convert::key_values(items)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedicated_coordinate_options_reject_unknown_keys() {
        assert!(coordinate_options(&["ALLOW_BALPARK=NO".into()]).is_err());
        let args = VectorArgs { transform_options: vec!["ONLY_BEST=YES".into()], ..Default::default() };
        assert!(!args.is_set(), "coordinate controls also apply to raster inputs");
        assert!(args.coordinate_options().unwrap().only_best);
    }
}
