//! Exact valid-data raster footprints, with boundaries on pixel edges.

use super::polygonize::{PolygonizeOptions, polygonize};
use crate::{
    DataType, Error, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Raster, RasterConfig, RasterDataset, Result,
};

/// Options for [`footprint`]. Validity uses declared nodata and finite values; an optional
/// explicit mask can further restrict it. Implicit alpha bands and external mask files are not
/// interpreted as masks.
#[derive(Debug, Clone, Default)]
pub struct FootprintOptions {
    /// Bands from 1; empty means all bands.
    pub bands: Vec<usize>,
    /// Require every selected band to be valid, rather than at least one.
    pub intersection: bool,
    /// Optional mask on exactly the input grid and CRS. Only finite, nonzero, non-nodata pixels
    /// of its first band are included.
    pub mask: Option<RasterDataset>,
    /// Emit one polygon feature per connected region instead of one multipolygon.
    pub singlepart: bool,
    /// Text fields copied to every output feature, in the supplied order.
    /// Names must be nonempty and unique (case-insensitively).
    pub attributes: Vec<(String, String)>,
}

/// A single multipolygon containing every valid pixel, including holes and disconnected areas.
/// Pixels connect through their sides (4-connectedness); regions touching only at a corner
/// remain separate polygon members, as in GDAL's default footprint.
/// With `singlepart`, each polygon member becomes a feature with the same attributes.
/// No simplification or overview is applied. An entirely invalid raster gives an empty layer.
/// Rotated/south-up affine grids and the source CRS are preserved. Complex rasters and rasters
/// georeferenced by geolocation arrays are rejected.
///
/// ```no_run
/// let dem = terrio::open_raster("dem.tif")?;
/// let area = terrio::ops::footprint::footprint(&dem, &Default::default())?;
/// terrio::write_vector(&[area], "footprint.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn footprint(ds: &RasterDataset, options: &FootprintOptions) -> Result<Layer> {
    let mut names = std::collections::HashSet::new();
    for (name, _) in &options.attributes {
        if name.trim().is_empty() || !names.insert(name.to_lowercase()) {
            return Err(Error::InvalidArgument(format!("footprint attribute names must be nonempty and unique: '{name}'")));
        }
    }
    super::sample::check_raster(ds)?;
    let r = &ds.raster;
    let bands = if options.bands.is_empty() { (1..=r.bands).collect() } else { options.bands.clone() };
    for &band in &bands {
        super::sample::check_band(ds, band)?;
    }
    if let Some(m) = &options.mask {
        super::sample::check_raster(m)?;
        super::sample::check_band(m, 1)?;
        let same_crs = (m.crs().is_unknown() && ds.crs().is_unknown()) || m.crs().same_as(&ds.crs());
        if m.geo_transform() != ds.geo_transform() || (m.raster.cols, m.raster.rows) != (r.cols, r.rows) || !same_crs {
            return Err(Error::InvalidArgument("the mask must have the input raster's grid and CRS".into()));
        }
    }
    let n = r.rows * r.cols;
    let mut valid = vec![u8::from(options.intersection); n];
    for band in bands {
        for (i, value) in valid.iter_mut().enumerate() {
            let v = r.data.get_f64((band - 1) * n + i);
            let ok = v.is_finite() && crate::histogram::is_valid(v, ds.nodata(), r.data_type);
            if options.intersection {
                *value &= u8::from(ok);
            } else {
                *value |= u8::from(ok);
            }
        }
    }
    if let Some(m) = &options.mask {
        for (i, value) in valid.iter_mut().enumerate() {
            let v = m.raster.data.get_f64(i);
            *value &= u8::from(v.is_finite() && v != 0.0 && crate::histogram::is_valid(v, m.nodata(), m.raster.data_type));
        }
    }
    let config = RasterConfig { cols: r.cols, rows: r.rows, bands: 1, data_type: DataType::U8, nodata: 0.0, ..Default::default() };
    let mut mask = RasterDataset::new(Raster::from_data_native(config, crate::raster::RasterData::U8(valid))?, true);
    mask.set_geo_transform(ds.geo_transform());
    mask.raster.crs = r.crs.clone();
    let regions = polygonize(&mask, &PolygonizeOptions::default())?;
    let parts: Vec<_> = regions
        .features
        .into_iter()
        .filter_map(|f| match f.geometry {
            Some(Geometry::Polygon { exterior, interiors }) => Some((exterior, interiors)),
            _ => None,
        })
        .collect();
    let mut out = Layer::new("footprint");
    out.geom_type = Some(GeometryType::MultiPolygon);
    ds.crs().apply_to_layer(&mut out);
    for (name, _) in &options.attributes {
        out.schema.add_field(FieldDef::new(name, FieldType::Text));
    }
    if !parts.is_empty() {
        let attributes = options.attributes.iter().map(|(_, value)| FieldValue::Text(value.clone())).collect();
        out.features.push(Feature { fid: 1, geometry: Some(Geometry::MultiPolygon(parts)), attributes });
    }
    Ok(if options.singlepart { super::geometry::singleparts(&out) } else { out })
}
