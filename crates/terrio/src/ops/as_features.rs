//! One vector feature per pixel: an equivalent of `gdal raster as-features`.
//!
//! Features follow the pixels row by row. Their fields are, in this order, `CENTER_X` and
//! `CENTER_Y` (the pixel centre, with `include_xy`), `ROW` and `COL` (with `include_row_col`) and
//! `BAND_<n>` for every selected band, as `Float` fields. Geometries are the pixel centres or the
//! pixel squares (rings from the top-left corner down the left edge, as GDAL draws them), in the
//! raster's CRS, or none.

use crate::{Error, RasterDataset, Result};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

/// The geometry of each feature.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PixelGeometry {
    /// No geometry: an attribute table (`none`, the default as in GDAL).
    #[default]
    None,
    /// The pixel centre.
    Point,
    /// The pixel square.
    Polygon,
}

impl PixelGeometry {
    /// Parses GDAL's `--geometry-type` value: `none`, `point` or `polygon`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "point" => Ok(Self::Point),
            "polygon" => Ok(Self::Polygon),
            other => Err(Error::InvalidArgument(format!("unknown geometry type '{other}' (none, point or polygon)"))),
        }
    }
}

/// Options for [`as_features`], named after `gdal raster as-features`'.
#[derive(Debug, Clone)]
pub struct AsFeaturesOptions {
    /// The bands to report, from 1 (`-b`); all by default.
    pub bands: Vec<usize>,
    /// The geometry of the features (`--geometry-type`).
    pub geometry: PixelGeometry,
    /// Leave out the pixels whose first reported band has its nodata value (`--skip-nodata`).
    pub skip_nodata: bool,
    /// Add `CENTER_X` and `CENTER_Y` fields (`--include-xy`).
    pub include_xy: bool,
    /// Add `ROW` and `COL` fields (`--include-row-col`).
    pub include_row_col: bool,
    /// The layer name (`--output-layer`); `pixels` by default.
    pub layer_name: String,
}

impl Default for AsFeaturesOptions {
    fn default() -> Self {
        Self {
            bands: Vec::new(),
            geometry: PixelGeometry::None,
            skip_nodata: false,
            include_xy: false,
            include_row_col: false,
            layer_name: "pixels".into(),
        }
    }
}

/// One feature per pixel of a raster, as `gdal raster as-features` makes them.
pub fn as_features(ds: &RasterDataset, o: &AsFeaturesOptions) -> Result<Layer> {
    if ds.complex {
        return Err(Error::Unsupported("features from complex bands".into()));
    }
    let r = &ds.raster;
    let bands: Vec<usize> = if o.bands.is_empty() { (1..=r.bands).collect() } else { o.bands.clone() };
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > r.bands) {
        return Err(Error::InvalidArgument(format!("the raster has no band {b}")));
    }
    let mut layer = Layer::new(o.layer_name.clone());
    match o.geometry {
        PixelGeometry::None => {}
        PixelGeometry::Point => layer = layer.with_geom_type(GeometryType::Point),
        PixelGeometry::Polygon => layer = layer.with_geom_type(GeometryType::Polygon),
    }
    if o.geometry != PixelGeometry::None {
        ds.crs().apply_to_layer(&mut layer);
    }
    if o.include_xy {
        layer.schema.add_field(FieldDef::new("CENTER_X", FieldType::Float));
        layer.schema.add_field(FieldDef::new("CENTER_Y", FieldType::Float));
    }
    if o.include_row_col {
        layer.schema.add_field(FieldDef::new("ROW", FieldType::Integer));
        layer.schema.add_field(FieldDef::new("COL", FieldType::Integer));
    }
    for b in &bands {
        layer.schema.add_field(FieldDef::new(format!("BAND_{b}"), FieldType::Float));
    }
    let views: Vec<_> = bands.iter().map(|b| super::BandView::of(r, b - 1)).collect();
    let nodata = if o.skip_nodata { ds.band_nodata(bands[0]) } else { None };
    let gt = ds.geo_transform();
    let at = |col: f64, row: f64| {
        let (x, y) = super::apply_gt(&gt, col, row);
        Coord::xy(x, y)
    };
    let (w, h) = (r.cols, r.rows);
    for row in 0..h {
        for col in 0..w {
            let i = row * w + col;
            if let Some(nd) = nodata {
                let v = views[0].get(i);
                if v == nd || (nd.is_nan() && v.is_nan()) {
                    continue;
                }
            }
            let mut attributes = Vec::with_capacity(views.len() + 4);
            let (c, l) = (col as f64, row as f64);
            if o.include_xy {
                let (x, y) = super::apply_gt(&gt, c + 0.5, l + 0.5);
                attributes.push(FieldValue::Float(x));
                attributes.push(FieldValue::Float(y));
            }
            if o.include_row_col {
                attributes.push(FieldValue::Integer(row as i64));
                attributes.push(FieldValue::Integer(col as i64));
            }
            attributes.extend(views.iter().map(|v| FieldValue::Float(v.get(i))));
            let geometry = match o.geometry {
                PixelGeometry::None => None,
                PixelGeometry::Point => Some(Geometry::Point(at(c + 0.5, l + 0.5))),
                PixelGeometry::Polygon => Some(Geometry::Polygon {
                    exterior: Ring(vec![at(c, l), at(c, l + 1.0), at(c + 1.0, l + 1.0), at(c + 1.0, l), at(c, l)]),
                    interiors: Vec::new(),
                }),
            };
            layer.features.push(Feature { fid: layer.features.len() as u64 + 1, geometry, attributes });
        }
    }
    Ok(layer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Grid, build_raster, convert_value};
    use wbraster::DataType;

    #[test]
    fn one_feature_per_pixel() {
        let grid = Grid { width: 2, height: 2, geo_transform: [100.0, 10.0, 0.0, 50.0, 0.0, -10.0] };
        let ds = build_raster(
            grid,
            vec![vec![1.0, 0.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]],
            DataType::U8,
            Some(0.0),
            &Default::default(),
            convert_value,
        )
        .unwrap();
        let o = AsFeaturesOptions { geometry: PixelGeometry::Polygon, include_xy: true, include_row_col: true, ..Default::default() };
        let l = as_features(&ds, &o).unwrap();
        assert_eq!(l.name, "pixels");
        assert_eq!(l.features.len(), 4);
        let names: Vec<_> = l.schema.fields().iter().map(|f| f.name.clone()).collect();
        assert_eq!(names, ["CENTER_X", "CENTER_Y", "ROW", "COL", "BAND_1", "BAND_2"]);
        let f = &l.features[3];
        assert_eq!(f.attributes[..4], [FieldValue::Float(115.0), FieldValue::Float(35.0), FieldValue::Integer(1), FieldValue::Integer(1)]);
        let Some(Geometry::Polygon { exterior, .. }) = &f.geometry else { panic!() };
        assert_eq!(exterior.0[0], Coord::xy(110.0, 40.0));
        assert_eq!(exterior.0[1], Coord::xy(110.0, 30.0));
        assert_eq!(exterior.0[2], Coord::xy(120.0, 30.0));
        let skipped = as_features(&ds, &AsFeaturesOptions { skip_nodata: true, bands: vec![2, 1], ..Default::default() }).unwrap();
        // The first reported band (2) has no zeros: nothing is left out.
        assert_eq!(skipped.features.len(), 4);
        let skipped = as_features(&ds, &AsFeaturesOptions { skip_nodata: true, ..Default::default() }).unwrap();
        assert_eq!(skipped.features.len(), 3);
        assert!(skipped.features[0].geometry.is_none());
        assert!(as_features(&ds, &AsFeaturesOptions { bands: vec![3], ..Default::default() }).is_err());
    }
}
