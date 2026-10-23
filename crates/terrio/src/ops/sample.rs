//! Sample one or more real-valued raster bands at vector points, reprojecting their coordinates
//! for each raster. Sampling is nearest-neighbour (the pixel containing the point).

use crate::crs::{SpatialRef, Transformer};
use crate::{Error, FieldDef, FieldType, FieldValue, Geometry, Layer, RasterDataset, Result};

/// One raster band and the name of its output field in [`sample`].
pub struct SampleInput<'a> {
    /// Output field name. Collisions with existing or earlier sample fields get `_2`, `_3`, etc.
    pub field_name: String,
    /// Raster to sample. It is read into memory by the caller.
    pub raster: &'a RasterDataset,
    /// Band, from 1.
    pub band: usize,
}

/// Adds a Float field per input raster band, retaining the points' geometry (including Z/M),
/// attributes, IDs and CRS. Null geometries, nodata, non-finite pixels and out-of-bounds samples
/// yield null fields. A point that cannot be transformed into a raster's CRS also yields a null
/// sample for that raster. Failures to construct a CRS transformation remain errors, as do
/// non-point geometries and non-finite point XY coordinates.
/// Per-point failures record one warning per output field with their count (see
/// [`crate::warnings::take`]); the CLI prints it to standard error.
///
/// Rasters may have different affine grids and CRSs; points are transformed for each raster,
/// leaving their output geometries unchanged. Missing CRSs are allowed only when both point
/// and raster CRS are missing, in which case coordinates are used directly. Complex rasters
/// and geolocation arrays are unsupported. Pixel values are represented as f64.
///
/// ```no_run
/// use terrio::ops::sample::{sample, SampleInput};
/// let points = terrio::open_vector("sites.gpkg", &Default::default())?;
/// let dem = terrio::open_raster("dem.tif")?;
/// let inputs = [SampleInput { field_name: "elevation".into(), raster: &dem, band: 1 }];
/// let sampled = sample(&points.layers[0], &inputs)?;
/// terrio::write_vector(&[sampled], "sampled.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn sample(points: &Layer, inputs: &[SampleInput<'_>]) -> Result<Layer> {
    if inputs.is_empty() {
        return Err(Error::InvalidArgument("sampling needs at least one raster band".into()));
    }
    for f in &points.features {
        match &f.geometry {
            Some(Geometry::Point(c)) if c.x.is_finite() && c.y.is_finite() => {}
            None => {}
            Some(Geometry::Point(_)) => return Err(Error::InvalidArgument(format!("feature {} has non-finite XY coordinates", f.fid))),
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "feature {} is not a point; split multipart points with singlepart first",
                    f.fid
                )));
            }
        }
    }
    let src = SpatialRef::from_layer(points);
    let mut out = points.clone();
    for input in inputs {
        let ds = input.raster;
        check_raster(ds)?;
        check_band(ds, input.band)?;
        if input.field_name.trim().is_empty() {
            return Err(Error::InvalidArgument("sample field names cannot be empty".into()));
        }
        let dst = ds.crs();
        let ct = match (src.is_unknown(), dst.is_unknown()) {
            (true, true) => None,
            (true, false) | (false, true) => {
                return Err(Error::Crs("points and raster must both declare a CRS, or both have no CRS".into()));
            }
            _ if src.same_as(&dst) => None,
            _ => Some(Transformer::new(&src, &dst)?),
        };
        let gt = ds.geo_transform();
        let det = gt[1] * gt[5] - gt[2] * gt[4];
        let name = super::geometry::unique_field(&out, &input.field_name);
        let index = out.schema.len();
        out.schema.add_field(FieldDef::new(name.clone(), FieldType::Float));
        let mut projection_failures = 0usize;
        for f in &mut out.features {
            let value = match &f.geometry {
                Some(Geometry::Point(c)) => {
                    let coords = match &ct {
                        Some(t) => t.transform(c.x, c.y),
                        None => Ok((c.x, c.y)),
                    };
                    let (x, y) = match coords {
                        Ok(xy) => xy,
                        Err(_) => {
                            projection_failures += 1;
                            f.set_by_index(index, FieldValue::Null);
                            continue;
                        }
                    };
                    let (dx, dy) = (x - gt[0], y - gt[3]);
                    let col = ((gt[5] * dx - gt[2] * dy) / det).floor();
                    let row = ((gt[1] * dy - gt[4] * dx) / det).floor();
                    if col >= 0.0 && row >= 0.0 && col < ds.raster.cols as f64 && row < ds.raster.rows as f64 {
                        let v = ds
                            .raster
                            .data
                            .get_f64((input.band - 1) * ds.raster.rows * ds.raster.cols + row as usize * ds.raster.cols + col as usize);
                        if v.is_finite() && crate::histogram::is_valid(v, ds.nodata(), ds.raster.data_type) {
                            FieldValue::Float(v)
                        } else {
                            FieldValue::Null
                        }
                    } else {
                        FieldValue::Null
                    }
                }
                _ => FieldValue::Null,
            };
            f.attributes.resize(index, FieldValue::Null);
            f.attributes.push(value);
        }
        if projection_failures > 0 {
            let point_label = if projection_failures == 1 { "point" } else { "points" };
            crate::warnings::warn(format!(
                "sample field '{name}' in layer '{}': could not transform {projection_failures} {point_label} into the raster CRS; samples set to null",
                points.name
            ));
        }
    }
    Ok(out)
}

pub(crate) fn check_band(ds: &RasterDataset, band: usize) -> Result<()> {
    if band == 0 || band > ds.band_count() {
        return Err(Error::InvalidArgument(format!(
            "band {band} does not exist (bands are numbered from 1; the raster has {})",
            ds.band_count()
        )));
    }
    Ok(())
}

pub(crate) fn check_raster(ds: &RasterDataset) -> Result<()> {
    if ds.complex || ds.geolocation.is_some() {
        return Err(Error::Unsupported("this operation needs real-valued raster bands with affine georeferencing".into()));
    }
    let gt = ds.geo_transform();
    let det = gt[1] * gt[5] - gt[2] * gt[4];
    if gt.iter().any(|v| !v.is_finite()) || det == 0.0 || !det.is_finite() {
        return Err(Error::InvalidArgument("the raster geotransform must be finite and invertible".into()));
    }
    if ds.raster.cols == 0 || ds.raster.rows == 0 || ds.band_count() == 0 {
        return Err(Error::InvalidArgument("the raster must have pixels and at least one band".into()));
    }
    Ok(())
}
