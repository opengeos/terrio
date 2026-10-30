// SPDX-FileCopyrightText: Copyright (c) 2002, i3 - information integration and imaging Fort Collin, CO
// SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2021, CLS
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     alg/gdaltransformer.cpp

//! Filters that take values from rasters and vector layers at the points: PDAL's `filters.dem`
//! (the points near a raster's surface), `filters.colorization` (dimensions from raster bands)
//! and `filters.overlay` (a dimension from the attribute of the polygons the points are in).

use super::DimRange;
use super::select::{Poly, polys_of, set_value};
use super::xyz;
use crate::crs::{SpatialRef, Transformer};
use crate::pointcloud::{Dimension, PointCloud, Values};
use crate::{Error, RasterDataset, Result};
use wbvector::{FieldValue, Layer};

/// The pixel of a raster a point is in, as GDAL and PDAL find it: the floor of the point's
/// position through the inverse geotransform (`GDALInvGeoTransform`), `None` outside.
struct Pixels {
    inv: [f64; 6],
    cols: usize,
    rows: usize,
}

impl Pixels {
    fn new(ds: &RasterDataset) -> Result<Self> {
        let gt = ds.geo_transform();
        let inv = if gt[2] == 0.0 && gt[4] == 0.0 && gt[1] != 0.0 && gt[5] != 0.0 {
            [-gt[0] / gt[1], 1.0 / gt[1], 0.0, -gt[3] / gt[5], 0.0, 1.0 / gt[5]]
        } else {
            let det = gt[1] * gt[5] - gt[2] * gt[4];
            let magnitude = gt[1].abs().max(gt[2].abs()).max(gt[4].abs().max(gt[5].abs()));
            if det.abs() <= 1e-10 * magnitude * magnitude {
                return Err(Error::InvalidArgument("the raster's geotransform cannot be inverted".into()));
            }
            let inv_det = 1.0 / det;
            [
                (gt[2] * gt[3] - gt[0] * gt[5]) * inv_det,
                gt[5] * inv_det,
                -gt[2] * inv_det,
                (-gt[1] * gt[3] + gt[0] * gt[4]) * inv_det,
                -gt[4] * inv_det,
                gt[1] * inv_det,
            ]
        };
        Ok(Self { inv, cols: ds.raster.cols, rows: ds.raster.rows })
    }

    /// The offset of the pixel `(x, y)` is in, within a band.
    fn at(&self, x: f64, y: f64) -> Option<usize> {
        let i = &self.inv;
        let col = (i[0] + i[1] * x + i[2] * y).floor();
        let row = (i[3] + i[4] * x + i[5] * y).floor();
        // PDAL casts to 32-bit integers before comparing.
        let (col, row) = (col as i32, row as i32);
        (col >= 0 && (col as usize) < self.cols && row >= 0 && (row as usize) < self.rows).then(|| row as usize * self.cols + col as usize)
    }
}

/// The value of a band (from 0) at a pixel offset: the stored value, nodata included, as GDAL
/// reads it.
fn band_value(ds: &RasterDataset, band: usize, offset: usize) -> f64 {
    let r = &ds.raster;
    r.data.get_f64(band * r.rows * r.cols + offset)
}

/// Keeps the points whose values of a dimension are within a range of a raster band's value at
/// them, as PDAL's `filters.dem`: from the value minus the range's lower bound to the value plus
/// its upper bound, both included (`Z[20:20]` keeps the points within 20 of the surface, the
/// range's brackets ignored as PDAL does). Points outside the raster are dropped; the raster is
/// taken to be in the points' CRS. `band` counts from 1.
pub fn dem(cloud: &PointCloud, raster: &RasterDataset, limits: &DimRange, band: usize) -> Result<PointCloud> {
    if band == 0 || band > raster.raster.bands {
        return Err(Error::InvalidArgument(format!("no band {band} (the raster has {})", raster.raster.bands)));
    }
    let (xs, ys, _) = xyz(cloud)?;
    let d = super::find_dim(cloud, &limits.name)?;
    let px = Pixels::new(raster)?;
    let keep: Vec<bool> = (0..cloud.len())
        .map(|i| {
            px.at(xs[i], ys[i]).is_some_and(|o| {
                let v = band_value(raster, band - 1, o);
                let z = d.get(i);
                z >= v - limits.lower && z <= v + limits.upper
            })
        })
        .collect();
    Ok(cloud.select(&keep))
}

/// A dimension filled from a raster band, as PDAL's `filters.colorization` writes them:
/// `Name:band:scale`.
#[derive(Debug, Clone, PartialEq)]
pub struct BandDimension {
    /// The dimension, as PDAL or terrio names it.
    pub name: String,
    /// The band, from 1.
    pub band: usize,
    /// The factor the band's values are multiplied by.
    pub scale: f64,
}

impl BandDimension {
    /// Parses PDAL's `dimensions`: `Name[:band[:scale]]` items separated by commas, such as
    /// `Red:1:1.0, Green:2, Blue`; a band left out is the one after the previous item's (the
    /// first is 1), a scale left out is 1. Names are PDAL's or terrio's, in any case.
    pub fn parse_list(s: &str) -> Result<Vec<Self>> {
        let mut out: Vec<Self> = Vec::new();
        for item in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let default_band = out.last().map_or(1, |b| b.band + 1);
            let err = |what: &str| Error::InvalidArgument(format!("invalid dimension '{item}': {what}"));
            let mut parts = item.splitn(3, ':');
            let name = parts.next().unwrap_or_default().trim();
            if !name.starts_with(|c: char| c.is_ascii_alphabetic()) || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(err("no dimension name"));
            }
            let band = match parts.next().map(str::trim) {
                None | Some("") => default_band,
                Some(b) => b.parse::<usize>().map_err(|_| err("the band is not a number"))?,
            };
            if band == 0 {
                return Err(err("bands start at 1"));
            }
            let scale = match parts.next().map(str::trim) {
                None | Some("") => 1.0,
                Some(f) => f.parse::<f64>().map_err(|_| err("the scale is not a number"))?,
            };
            out.push(Self { name: name.to_string(), band, scale });
        }
        Ok(out)
    }

    /// PDAL's default: `Red`, `Green` and `Blue` from bands 1, 2 and 3.
    pub fn rgb() -> Vec<Self> {
        ["Red", "Green", "Blue"].iter().enumerate().map(|(k, n)| Self { name: n.to_string(), band: k + 1, scale: 1.0 }).collect()
    }
}

/// Sets a value as PDAL's `setField` converts doubles: rounded (half away from zero) for
/// integers, and left as it was when it does not fit.
fn set_lenient(d: &mut Dimension, i: usize, v: f64) {
    let _ = set_value(d, i, v);
}

/// Fills dimensions from raster bands, as PDAL's `filters.colorization`: each point in the
/// raster takes the values of the pixel it is in, times each dimension's scale; points outside
/// keep theirs (0 for dimensions added). The dimensions are doubles, as PDAL 2.9 makes them (it
/// asks for the bands' types before opening the raster, and so registers every dimension as
/// doubles, which its layout prefers to the type a dimension had). The raster is taken to be in
/// the points' CRS.
pub fn colorization(cloud: &PointCloud, raster: &RasterDataset, dimensions: &[BandDimension]) -> Result<PointCloud> {
    let bands = raster.raster.bands;
    if let Some(b) = dimensions.iter().find(|b| b.band > bands) {
        return Err(Error::InvalidArgument(format!("no band {} for {} (the raster has {bands})", b.band, b.name)));
    }
    let (xs, ys, _) = xyz(cloud)?;
    let px = Pixels::new(raster)?;
    let mut out = cloud.clone();
    let n = cloud.len();
    let pixels: Vec<Option<usize>> = (0..n).map(|i| px.at(xs[i], ys[i])).collect();
    for b in dimensions {
        let name = super::output_name(&out, &b.name);
        let mut values = super::features::f64_or_zeros(&out, &name);
        for (v, p) in values.iter_mut().zip(&pixels) {
            if let Some(o) = p {
                *v = band_value(raster, b.band - 1, *o) * b.scale;
            }
        }
        super::features::set_column(&mut out, &name, Values::F64(values));
    }
    Ok(out)
}

/// What PDAL's `filters.overlay` reads of a vector layer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayOptions {
    /// The dimension set, which the points must have, as PDAL or terrio names it.
    pub dimension: String,
    /// The attribute whose values the points take (case-insensitive). By default, the second
    /// attribute, as PDAL takes it.
    pub column: Option<String>,
}

/// An attribute as an integer, as GDAL's `OGR_F_GetFieldAsInteger` reads it: integers (clamped
/// to 32 bits), reals truncated, the leading integer of text, 0 otherwise.
fn as_integer(v: Option<&FieldValue>) -> i32 {
    match v {
        Some(FieldValue::Integer(i)) => (*i).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        Some(FieldValue::Float(f)) => *f as i32,
        Some(FieldValue::Boolean(b)) => i32::from(*b),
        Some(FieldValue::Text(s)) => {
            let t = s.trim_start();
            let end =
                t.char_indices().find(|&(k, c)| !(c.is_ascii_digit() || (k == 0 && (c == '-' || c == '+')))).map_or(t.len(), |(k, _)| k);
            t[..end].parse::<i64>().map_or(0, |v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
        }
        _ => 0,
    }
}

/// Sets a dimension from the polygons of a vector layer, as PDAL's `filters.overlay`: each point
/// in (or on the boundary of) a polygon takes the attribute's value, as an integer, of the first
/// feature whose polygon has it; others keep theirs. The polygons are reprojected to the
/// points' CRS when both are known.
pub fn overlay(cloud: &PointCloud, layer: &Layer, o: &OverlayOptions) -> Result<PointCloud> {
    let name = super::find_dim(cloud, &o.dimension)?.name.clone();
    if layer.features.is_empty() {
        return Err(Error::InvalidArgument(format!("the layer '{}' has no features", layer.name)));
    }
    let fields = layer.schema.fields();
    let field = match &o.column {
        Some(c) => Some(
            fields
                .iter()
                .position(|f| f.name.eq_ignore_ascii_case(c))
                .ok_or_else(|| Error::InvalidArgument(format!("no column named '{c}' was found")))?,
        ),
        None => (fields.len() > 1).then_some(1),
    };
    let src = SpatialRef::from_layer(layer);
    let t = (!src.is_unknown() && !cloud.crs.is_unknown() && !src.same_as(&cloud.crs))
        .then(|| Transformer::new(&src, &cloud.crs))
        .transpose()?;
    // Two descriptions of the same CRS (an EPSG code and its WKT) give a transformation that
    // moves nothing but for rounding: leave the polygons alone then, as GDAL does.
    let t = match (t, crate::vector::layer_extent(layer)) {
        (Some(t), Some((x0, y0, x1, y1))) => {
            let moved = [(x0, y0), (x1, y1), (x0, y1), (x1, y0)].iter().any(|&(x, y)| {
                t.transform(x, y)
                    .map_or(true, |(u, v)| (u - x).abs() > 1e-10 * x.abs().max(1.0) || (v - y).abs() > 1e-10 * y.abs().max(1.0))
            });
            moved.then_some(t)
        }
        (t, _) => t,
    };
    let mut polys: Vec<(Vec<Poly>, i32)> = Vec::new();
    for f in &layer.features {
        let Some(mut ps) = f.geometry.as_ref().and_then(polys_of) else { continue };
        if let Some(t) = &t {
            for p in ps.iter_mut() {
                for ring in std::iter::once(&mut p.exterior).chain(p.holes.iter_mut()) {
                    for c in ring.iter_mut() {
                        let (x, y) = t.transform(c[0], c[1])?;
                        *c = [x, y];
                    }
                }
            }
        }
        polys.push((ps, as_integer(field.and_then(|k| f.attributes.get(k)))));
    }
    let (xs, ys, _) = xyz(cloud)?;
    let mut out = cloud.clone();
    let d = out.dim_mut(&name).expect("the dimension was found");
    for i in 0..xs.len() {
        if let Some((_, v)) = polys.iter().find(|(ps, _)| ps.iter().any(|p| p.contains(xs[i], ys[i]))) {
            set_lenient(d, i, *v as f64);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_band_dimensions_as_pdal() {
        let b = BandDimension::parse_list("Red:3:256, Green, Blue::2, Intensity:1").unwrap();
        let got: Vec<(&str, usize, f64)> = b.iter().map(|b| (b.name.as_str(), b.band, b.scale)).collect();
        assert_eq!(got, [("Red", 3, 256.0), ("Green", 4, 1.0), ("Blue", 5, 2.0), ("Intensity", 1, 1.0)]);
        assert!(BandDimension::parse_list("Red:0").is_err());
        assert!(BandDimension::parse_list("1:2").is_err());
    }

    #[test]
    fn reads_integers_as_ogr() {
        assert_eq!(as_integer(Some(&FieldValue::Float(-3.9))), -3);
        assert_eq!(as_integer(Some(&FieldValue::Text(" 42abc".into()))), 42);
        assert_eq!(as_integer(Some(&FieldValue::Text("x".into()))), 0);
        assert_eq!(as_integer(None), 0);
    }
}
