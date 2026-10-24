//! Single-observer viewsheds using GDAL's Wang (2000) edge recurrence.
//!
//! Requires a north-up projected DEM whose horizontal and vertical units agree. Numeric
//! input nodata is treated as elevation, as in GDAL: this can
//! produce misleading occlusion. Nonfinite elevations in the processed window are rejected.
//! Dataset masks are not interpreted. Fill or crop missing elevations for meaningful results.
//!
//! The recurrence is adapted from GDAL's `alg/viewshed/viewshed_executor.cpp` (MIT, see NOTICE).
use super::{Grid, build_raster};
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;

/// Information written by a viewshed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ViewshedMode {
    /// Byte visibility, 255 visible, 0 invisible.
    #[default]
    Normal,
    /// Minimum visible elevation above the vertical datum (Float64).
    Dem,
    /// Minimum visible height above the DEM surface (Float64).
    Ground,
}
impl ViewshedMode {
    /// Parses NORMAL, DEM or GROUND, case-insensitively.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_uppercase().as_str() {
            "NORMAL" => Ok(Self::Normal),
            "DEM" => Ok(Self::Dem),
            "GROUND" => Ok(Self::Ground),
            _ => Err(Error::InvalidArgument("viewshed mode must be NORMAL, DEM or GROUND (cumulative visibility is not supported)".into())),
        }
    }
}

/// Options for [`viewshed`]. Heights use DEM units and distances use projected CRS units.
#[derive(Debug, Clone)]
pub struct ViewshedOptions {
    /// Source band, from 1.
    pub band: usize,
    /// Observer easting in the DEM CRS. The containing pixel supplies its elevation.
    pub observer_x: f64,
    /// Observer northing in the DEM CRS.
    pub observer_y: f64,
    /// Observer height above the DEM (default 2); see outside-observer behavior below.
    pub observer_height: f64,
    /// Target height above the DEM, used only in NORMAL mode (default 0).
    pub target_height: f64,
    /// Maximum distance; zero means unlimited. Also crops the output rectangle.
    pub max_distance: f64,
    /// Curvature coefficient, equal to 1 minus atmospheric refraction coefficient.
    /// Zero disables curvature, 1 uses no refraction. None uses GDAL's Earth/non-Earth default.
    pub curvature_coefficient: Option<f64>,
    /// Output information.
    pub mode: ViewshedMode,
    /// Value outside the maximum distance (default 0).
    pub out_of_range: u8,
}
impl Default for ViewshedOptions {
    fn default() -> Self {
        Self {
            band: 1,
            observer_x: 0.0,
            observer_y: 0.0,
            observer_height: 2.0,
            target_height: 0.0,
            max_distance: 0.0,
            curvature_coefficient: None,
            mode: ViewshedMode::Normal,
            out_of_range: 0,
        }
    }
}

/// Computes visibility or minimum visible height in O(width × height) time.
///
/// The observer is snapped to its containing pixel, as in GDAL. Ground/DEM modes ignore
/// target height. The output has no nodata value: nonfinite input is an error, and out-of-range
/// pixels have `out_of_range`. Cumulative visibility is unsupported.
/// Outside observers follow GDAL: the nearest raster edge starts unobstructed. If observer X
/// is outside, observer height is absolute; if only Y is outside, elevation is sampled on the
/// nearest row at observer X. Distances use the snapped observer pixel. Curvature uses the CRS
/// ellipsoid's semi-major axis in metres, matching GDAL even for non-metre projected units;
/// use coefficient zero or convert the DEM to metres for physical consistency in that case.
pub fn viewshed(ds: &RasterDataset, o: &ViewshedOptions) -> Result<RasterDataset> {
    let invalid = |s: &str| Error::InvalidArgument(s.into());
    if ds.complex || o.band == 0 || o.band > ds.raster.bands {
        return Err(invalid("viewshed requires a real source band"));
    }
    let crs = ds.crs();
    if crs.is_unknown() || crs.is_geographic() || crs.is_rotated_pole() {
        return Err(invalid("viewshed requires a projected DEM (horizontal and vertical units must agree)"));
    }
    let gt = ds.geo_transform();
    super::north_up(&Grid::of(&ds.raster))?;
    if gt.iter().any(|v| !v.is_finite()) || gt[1] <= 0.0 || gt[5] >= 0.0 || gt[2] != 0.0 || gt[4] != 0.0 {
        return Err(invalid("viewshed requires a finite north-up geotransform"));
    }
    if [o.observer_x, o.observer_y, o.observer_height, o.target_height, o.max_distance].iter().any(|v| !v.is_finite())
        || o.max_distance < 0.0
        || o.curvature_coefficient.is_some_and(|v| !v.is_finite())
    {
        return Err(invalid("viewshed options must be finite; maximum distance must be nonnegative"));
    }
    let (w, h) = (ds.raster.cols, ds.raster.rows);
    let (px, py) = ((o.observer_x - gt[0]) / gt[1], (o.observer_y - gt[3]) / gt[5]);
    if !px.is_finite() || !py.is_finite() || px.abs() > i32::MAX as f64 || py.abs() > i32::MAX as f64 {
        return Err(invalid("viewshed observer pixel coordinates are out of range"));
    }
    // GDAL truncates toward zero, including coordinates just outside the top/left edge.
    let (ox, oy) = (px as isize, py as isize);
    let (mut x0, mut y0, mut x1, mut y1) = (0, 0, w, h);
    if o.max_distance > 0.0 {
        let (rx, ry) = (o.max_distance / gt[1], o.max_distance / -gt[5]);
        x0 = ((ox as f64 - rx + 1e-8).floor().max(0.0) as usize).min(w);
        y0 = ((oy as f64 - ry + 1e-8).floor().max(0.0) as usize).min(h);
        x1 = ((ox as f64 + rx - 1e-8).ceil() + 1.0).min(w as f64) as usize;
        y1 = ((oy as f64 + ry - 1e-8).ceil() + 1.0).min(h as f64) as usize;
    }
    if x1 <= x0 || y1 <= y0 {
        return Err(invalid("viewshed maximum distance does not intersect the DEM"));
    }
    let (nw, nh) = (x1 - x0, y1 - y0);
    let src = super::BandView::of(&ds.raster, o.band - 1);
    let mut z = Vec::with_capacity(nw * nh);
    for y in y0..y1 {
        for x in x0..x1 {
            let v = src.get(y * w + x);
            if !v.is_finite() {
                return Err(invalid("viewshed input contains nonfinite elevations in the processed window; fill or crop them first"));
            }
            z.push(v);
        }
    }
    let (ox, oy) = (ox - x0 as isize, oy - y0 as isize);
    let (start_x, start_y) = (ox.clamp(0, nw as isize - 1) as usize, oy.clamp(0, nh as isize - 1) as usize);
    let inside_x = ox >= 0 && ox < nw as isize;
    let inside_y = oy >= 0 && oy < nh as isize;
    // GDAL samples the nearest row when Y is outside but X is inside; when X is
    // outside, the observer height is absolute and intervening space is unobstructed.
    let observer = o.observer_height + if inside_x { z[start_y * nw + start_x] } else { 0.0 };
    let a = crs.to_wkt().as_deref().and_then(crate::crs::semi_major_axis).unwrap_or(6_378_137.0);
    let coeff = o.curvature_coefficient.unwrap_or(if (a / 6_378_137.0 - 1.0).abs() > 0.05 { 1.0 } else { 0.85714 });
    let mut out = vec![0.0; nw * nh];
    for y in 0..nh {
        for x in 0..nw {
            let p = y * nw + x;
            out[p] = if o.mode == ViewshedMode::Dem { z[p] } else { 0.0 };
            let r2 = ((x as f64 - ox as f64) * gt[1]).powi(2) + ((y as f64 - oy as f64) * gt[5]).powi(2);
            if o.max_distance == 0.0 || r2 <= o.max_distance * o.max_distance {
                z[p] -= (coeff / (2.0 * a)) * r2 + observer;
            }
        }
    }
    // Sweep the observer row first, then each half of the raster, always away from the observer.
    let rows = std::iter::once(start_y).chain((0..start_y).rev()).chain(start_y + 1..nh);
    for y in rows {
        let j = (y as isize).abs_diff(oy);
        let prev_y = if y as isize > oy { y.saturating_sub(1) } else { (y + 1).min(nh - 1) };
        let cols = std::iter::once(start_x).chain((0..start_x).rev()).chain(start_x + 1..nw);
        for x in cols {
            let i = (x as isize).abs_diff(ox);
            let p = y * nw + x;
            let r2 = ((x as f64 - ox as f64) * gt[1]).powi(2) + ((y as f64 - oy as f64) * gt[5]).powi(2);
            if o.max_distance > 0.0 && r2 > o.max_distance * o.max_distance {
                out[p] = o.out_of_range as f64;
                continue;
            }
            let current = z[p];
            let boundary = (y == start_y && !inside_y) || (x == start_x && !inside_x);
            let horizon = if boundary || i.max(j) <= 1 {
                current
            } else if j == 0 {
                z[y * nw + if x as isize > ox { x - 1 } else { x + 1 }] * i as f64 / (i - 1) as f64
            } else if i == 0 {
                z[prev_y * nw + x] * j as f64 / (j - 1) as f64
            } else {
                let prev_x = if x as isize > ox { x - 1 } else { x + 1 };
                let diagonal = z[prev_y * nw + prev_x];
                if i == j {
                    diagonal * i as f64 / (i - 1) as f64
                } else if i > j {
                    (diagonal * j as f64 + z[y * nw + prev_x] * (i - j) as f64) / (i - 1) as f64
                } else {
                    (diagonal * i as f64 + z[prev_y * nw + x] * (j - i) as f64) / (j - 1) as f64
                }
            };
            if o.mode == ViewshedMode::Normal {
                out[p] = if !(boundary || (y as isize == oy && i <= 1)) && current + o.target_height < horizon { 0.0 } else { 255.0 };
            } else {
                out[p] += (horizon - current).max(0.0);
            }
            z[p] = current.max(horizon);
        }
    }
    drop(z);
    let mut output_gt = gt;
    output_gt[0] += x0 as f64 * gt[1];
    output_gt[3] += y0 as f64 * gt[5];
    build_raster(
        Grid { width: nw, height: nh, geo_transform: output_gt },
        vec![out],
        if o.mode == ViewshedMode::Normal { DataType::U8 } else { DataType::F64 },
        None,
        &crs,
        |v, _| v,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dem(v: Vec<f64>, w: usize, h: usize, nd: Option<f64>) -> RasterDataset {
        build_raster(
            Grid { width: w, height: h, geo_transform: [0.0, 10.0, 0.0, h as f64 * 10.0, 0.0, -10.0] },
            vec![v],
            DataType::F64,
            nd,
            &crate::crs::SpatialRef::from_epsg(32611).unwrap(),
            |v, _| v,
        )
        .unwrap()
    }
    #[test]
    fn ridge_heights_targets_and_radius() {
        let src = dem(vec![0.0, 0.0, 10.0, 0.0, 0.0], 5, 1, None);
        let mut o = ViewshedOptions { observer_x: 5.0, observer_y: 5.0, curvature_coefficient: Some(0.0), ..Default::default() };
        let v = viewshed(&src, &o).unwrap();
        assert_eq!(super::super::band_f64(&v.raster, 0), [255.0, 255.0, 255.0, 0.0, 0.0]);
        o.mode = ViewshedMode::Ground;
        let ground = viewshed(&src, &o).unwrap();
        assert_eq!(super::super::band_f64(&ground.raster, 0), [0.0, 0.0, 0.0, 14.0, 18.0]);
        o.mode = ViewshedMode::Normal;
        o.target_height = 14.0;
        let v = viewshed(&src, &o).unwrap();
        assert_eq!(v.raster.get(0, 0, 3), 255.0);
        assert_eq!(v.raster.get(0, 0, 4), 0.0);
        o.max_distance = 20.0;
        let v = viewshed(&src, &o).unwrap();
        assert_eq!((v.raster.cols, v.raster.rows), (3, 1));
    }
    #[test]
    fn nodata_and_validation() {
        let mut src = dem(vec![0.0, -9999.0, 0.0], 3, 1, Some(-9999.0));
        let o = ViewshedOptions { observer_x: 5.0, observer_y: 5.0, curvature_coefficient: Some(0.0), ..Default::default() };
        assert!(viewshed(&src, &o).is_ok(), "numeric nodata follows GDAL");
        src.raster.set(0, 0, 1, f64::NAN).unwrap();
        assert!(viewshed(&src, &o).is_err());
        let local = ViewshedOptions { max_distance: 1.0, ..o.clone() };
        // The radius crop includes the immediate neighbor, like GDAL.
        assert!(viewshed(&src, &local).is_err());
        assert!(viewshed(&src, &ViewshedOptions { observer_x: f64::INFINITY, ..o }).is_err());
        assert!(ViewshedMode::parse("ACCUM").is_err());
    }
    #[test]
    fn outside_observers_and_radius_mask() {
        let src = dem(vec![0.0; 25], 5, 5, None);
        let o = ViewshedOptions {
            observer_x: -15.0,
            observer_y: 25.0,
            max_distance: 25.0,
            curvature_coefficient: Some(0.0),
            out_of_range: 7,
            ..Default::default()
        };
        let out = viewshed(&src, &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (3, 5));
        assert_eq!(
            super::super::band_f64(&out.raster, 0),
            [255.0, 7.0, 7.0, 255.0, 255.0, 7.0, 255.0, 255.0, 7.0, 255.0, 255.0, 7.0, 255.0, 7.0, 7.0]
        );
        let north = viewshed(&src, &ViewshedOptions { observer_x: 25.0, observer_y: 65.0, mode: ViewshedMode::Ground, ..o }).unwrap();
        assert_eq!(super::super::band_f64(&north.raster, 0), [0.0, 0.0, 0.0, 0.0, 0.0, 7.0, 0.0, 0.0, 0.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0]);
    }
    #[test]
    fn curvature_and_symmetry() {
        let src = dem(vec![0.0; 81], 9, 9, None);
        let o = ViewshedOptions {
            observer_x: 45.0,
            observer_y: 45.0,
            observer_height: 0.0,
            curvature_coefficient: Some(1.0),
            ..Default::default()
        };
        let v = viewshed(&src, &o).unwrap();
        assert_eq!(v.raster.get(0, 4, 8), 0.0);
        for y in 0..9 {
            for x in 0..9 {
                assert_eq!(v.raster.get(0, y, x), v.raster.get(0, 8 - y, 8 - x));
            }
        }
        let flat = viewshed(&src, &ViewshedOptions { curvature_coefficient: Some(0.0), ..o }).unwrap();
        assert!(super::super::band_f64(&flat.raster, 0).iter().all(|v| *v == 255.0));
    }
}
