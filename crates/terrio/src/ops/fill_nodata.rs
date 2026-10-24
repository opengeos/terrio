//! Fill raster holes with GDAL's four-quadrant interpolation and optional 3×3 smoothing.
//!
//! Adapted from GDAL 3.12.2's `alg/rasterfill.cpp` (MIT; see NOTICE). Searches and
//! smoothing use Float32 samples as GDAL does. Original valid samples are copied exactly.

use crate::{Error, RasterDataset, Result};
use wbraster::raster::RasterData;

/// How values surrounding a hole are combined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Interpolation {
    /// Inverse-distance weighting of the closest sample in each quadrant.
    #[default]
    InverseDistance,
    /// The nearest quadrant sample, with GDAL's tie-breaking order.
    Nearest,
}

impl Interpolation {
    /// Parses `inv_dist` or `nearest`, case-insensitively.
    pub fn parse(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "inv_dist" => Ok(Self::InverseDistance),
            "nearest" => Ok(Self::Nearest),
            _ => Err(Error::InvalidArgument(format!("unknown fill interpolation '{name}' (inv_dist, nearest)"))),
        }
    }
}

/// Options for [`fill_nodata`].
#[derive(Debug, Clone)]
pub struct FillNodataOptions<'a> {
    /// The band to fill, from 1. The output contains only this band.
    pub band: usize,
    /// Validity mask, row by row: zero marks holes; nonzero marks donors.
    /// Overrides nodata. Without it, the selected band's nodata defines holes.
    /// A caller with a lazy raster can pass its [`crate::LazyRaster::read_mask`] result.
    pub mask: Option<&'a [u8]>,
    /// Maximum search distance in pixels (100 by default); 0 searches the whole image.
    pub max_distance: f64,
    /// Interpolation method.
    pub interpolation: Interpolation,
    /// Number of 3×3 mean-filter passes on filled pixels. First/last rows are
    /// unchanged, matching GDAL. Original valid pixels are never smoothed.
    pub smoothing_iterations: usize,
}

impl Default for FillNodataOptions<'_> {
    fn default() -> Self {
        Self { band: 1, mask: None, max_distance: 100.0, interpolation: Interpolation::default(), smoothing_iterations: 0 }
    }
}

/// Converts band 1 of a validity-mask raster to bytes, as GDAL reads a mask.
/// Rejects a different size. The georeferencing is deliberately ignored.
pub fn raster_mask(mask: &RasterDataset, width: usize, height: usize) -> Result<Vec<u8>> {
    if mask.complex || mask.raster.bands == 0 || (mask.raster.cols, mask.raster.rows) != (width, height) {
        return Err(Error::InvalidArgument("the mask must be a real raster of the input's size".into()));
    }
    Ok(super::proximity::read_ints(mask, 1, wbraster::DataType::U8).into_iter().map(|v| v as u8).collect())
}

/// Fills holes in one band using GDAL's four-quadrant search. Returns a single-band
/// raster with the source's type, nodata and georeferencing. Pixels without donors
/// within the search fringe are unchanged. As in GDAL, nearest interpolation
/// writes zero when a quadrant candidate lies just beyond the distance limit
/// (but less than one pixel beyond it). Complex bands are rejected.
///
/// ```no_run
/// use terrio::ops::fill_nodata::{fill_nodata, FillNodataOptions};
/// let src = terrio::open_raster("dem.tif")?;
/// let filled = fill_nodata(&src, &FillNodataOptions::default())?;
/// terrio::write_raster_dataset(&filled, "filled.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn fill_nodata(ds: &RasterDataset, o: &FillNodataOptions<'_>) -> Result<RasterDataset> {
    let r = &ds.raster;
    let (w, h) = (r.cols, r.rows);
    if ds.complex {
        return Err(Error::Unsupported("fill nodata requires real-valued bands".into()));
    }
    if o.band == 0 || o.band > r.bands || w == 0 || h == 0 {
        return Err(Error::InvalidArgument("fill nodata needs a nonempty raster and an existing band (from 1)".into()));
    }
    if !o.max_distance.is_finite() || o.max_distance < 0.0 {
        return Err(Error::InvalidArgument("maximum search distance must be finite and nonnegative".into()));
    }
    if o.mask.is_some_and(|m| m.len() != w * h) {
        return Err(Error::InvalidArgument("the mask's size differs from the raster's".into()));
    }
    let n = w * h;
    let start = (o.band - 1) * n;
    let raw = super::band_f64(r, o.band - 1);
    let mut valid: Vec<bool> = match o.mask {
        Some(m) => m.iter().map(|v| *v != 0).collect(),
        None => match ds.nodata() {
            Some(nd) if super::is_integer(r.data_type) => {
                let (lo, hi) = super::type_range(r.data_type);
                if !nd.is_finite() || nd.fract() != 0.0 || nd < lo || nd > hi {
                    vec![true; n]
                } else {
                    // Compare 64-bit samples in their own type: f64 would merge
                    // adjacent integers above 2^53 and incorrectly fill donors.
                    fn mask<T: crate::pixel::Pixel>(v: &[T], nd: f64) -> Vec<bool> {
                        let nd = T::from_f64(nd);
                        v.iter().map(|x| *x != nd).collect()
                    }
                    crate::pixel::with_data!(&r.data, v => mask(&v[start..start + n], nd))
                }
            }
            Some(nd) => super::sieve::nodata_mask(&raw, nd, r.data_type),
            None => vec![true; w * h],
        },
    };
    // Copy only the selected band, rather than cloning every input band and
    // immediately discarding the others (important for large multispectral inputs).
    let raster = wbraster::Raster {
        cols: w,
        rows: h,
        bands: 1,
        x_min: r.x_min,
        y_min: r.y_min,
        cell_size_x: r.cell_size_x,
        cell_size_y: r.cell_size_y,
        nodata: r.nodata,
        data_type: r.data_type,
        crs: r.crs.clone(),
        metadata: r.metadata.clone(),
        data: crate::pixel::map_data!(&r.data, v => v[start..start + n].to_vec()),
    };
    let mut out = RasterDataset::with_driver(raster, ds.driver, ds.has_nodata);
    out.affine = ds.affine;
    out.geolocation = ds.geolocation.clone();
    if valid.iter().all(|v| *v) || valid.iter().all(|v| !*v) {
        return Ok(out);
    }
    let original = valid.clone();
    let mut values: Vec<f32> = raw.iter().map(|v| *v as f32).collect();
    let max = if o.max_distance == 0.0 { w.max(h) as f64 + 1.0 } else { o.max_distance };
    // A donor beyond the image diagonal can never improve a result. Bounding the
    // scan also avoids converting an unbounded user distance to an integer.
    let limit = max.floor().min(w.max(h) as f64 + 1.0) as usize;
    // Nearest valid row at or above each pixel. Values always come from original
    // donors: interpolated pixels must not propagate into the second scan.
    let mut top = vec![usize::MAX; n];
    let mut last = vec![usize::MAX; w];
    for y in 0..h {
        for x in 0..w {
            if original[y * w + x] {
                last[x] = y;
            }
            if last[x] != usize::MAX && (y - last[x]) as f64 <= max {
                top[y * w + x] = last[x];
            }
        }
    }
    last.fill(usize::MAX);
    let mut filled = vec![false; n];
    for y in (0..h).rev() {
        for x in 0..w {
            let p = y * w + x;
            if original[p] {
                continue;
            }
            let mut distances = [max + 1.0; 4];
            let mut samples = [0f32; 4];
            let mut search = limit;
            let mut step = 0;
            while step <= search {
                let left = x.saturating_sub(step);
                let right = x.saturating_add(step).min(w - 1);
                for (q, cx, cy) in
                    [(0, left, top[y * w + left]), (1, left, last[left]), (2, right, top[y * w + right]), (3, right, last[right])]
                {
                    if cy == usize::MAX || (step == 0 && q >= 2) {
                        continue;
                    }
                    let dx = cx as f64 - x as f64;
                    let dy = cy as f64 - y as f64;
                    let sq = dx * dx + dy * dy;
                    if sq < distances[q] * distances[q] {
                        distances[q] = sq.sqrt();
                        samples[q] = raw[cy * w + cx] as f32;
                    }
                }
                if step != 0 && step & 3 == 0 {
                    search = distances.iter().copied().fold(0.0, f64::max).floor().min(w.max(h) as f64 + 1.0) as usize;
                }
                step += 1;
            }
            let mut accepted = true;
            let result = match o.interpolation {
                Interpolation::Nearest => {
                    let mut best = max + 1.0;
                    let mut value = None;
                    for q in 0..4 {
                        if distances[q] < best {
                            best = distances[q];
                            value = Some(samples[q]);
                        }
                    }
                    // GDAL marks a candidate in the one-pixel search fringe as
                    // filled, writes zero, but excludes it from smoothing donors.
                    value.map(|v| {
                        if best <= max {
                            v
                        } else {
                            accepted = false;
                            0.0
                        }
                    })
                }
                Interpolation::InverseDistance => {
                    let (mut weights, mut sum) = (0.0, 0.0);
                    for q in 0..4 {
                        if distances[q] <= max {
                            let weight = 1.0 / distances[q];
                            weights += weight;
                            sum += samples[q] as f64 * weight;
                        }
                    }
                    (weights > 0.0).then(|| (sum / weights) as f32)
                }
            };
            if let Some(value) = result {
                // GDAL stores the interpolation into the destination type before smoothing.
                values[p] = super::convert_value(value as f64, r.data_type) as f32;
                valid[p] = accepted;
                filled[p] = true;
            }
        }
        // Only rows strictly below y belong to the lower quadrants.
        for x in 0..w {
            if original[y * w + x] {
                last[x] = y;
            }
        }
    }
    // An implicit nodata mask is recomputed from the interpolated band by GDAL.
    // An explicit mask instead keeps the donor flags set by the search (notably
    // for zero values in nearest interpolation's search fringe).
    if o.mask.is_none() {
        valid = match ds.nodata() {
            Some(nd) => super::sieve::nodata_mask(&values.iter().map(|v| *v as f64).collect::<Vec<_>>(), nd, r.data_type),
            None => vec![true; n],
        };
    }
    for _ in 0..o.smoothing_iterations {
        let mut next = values.clone();
        for y in 1..h.saturating_sub(1) {
            for x in 0..w {
                let p = y * w + x;
                if !filled[p] {
                    continue;
                }
                let (mut sum, mut count) = (0f32, 0f32);
                for cy in y - 1..=y + 1 {
                    for cx in x.saturating_sub(1)..=x.saturating_add(1).min(w - 1) {
                        if valid[cy * w + cx] {
                            sum += values[cy * w + cx];
                            count += 1.0;
                        }
                    }
                }
                if count > 0.0 {
                    next[p] = sum / count;
                }
            }
        }
        values = next;
    }
    for y in 0..h {
        for x in 0..w {
            let p = y * w + x;
            if filled[p] {
                out.raster.set(0, y as isize, x as isize, super::convert_value(values[p] as f64, r.data_type))?;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Grid, build_raster, convert_value};
    use wbraster::DataType;

    fn source(values: Vec<f64>, w: usize, h: usize, nodata: Option<f64>) -> RasterDataset {
        build_raster(
            Grid { width: w, height: h, geo_transform: [0.0, 1.0, 0.0, h as f64, 0.0, -1.0] },
            vec![values],
            DataType::F32,
            nodata,
            &Default::default(),
            convert_value,
        )
        .unwrap()
    }

    #[test]
    fn gdal_quadrants_and_ties() {
        let ds = source(vec![0., 1., 2., 3., -9999., 5., 6., 7., 8.], 3, 3, Some(-9999.));
        let options = FillNodataOptions { max_distance: 1.0, ..Default::default() };
        let filled = fill_nodata(&ds, &options).unwrap();
        assert_eq!(super::super::band_f64(&filled.raster, 0), vec![0., 1., 2., 3., (13.0f32 / 3.0) as f64, 5., 6., 7., 8.]);
        let nearest = fill_nodata(&ds, &FillNodataOptions { interpolation: Interpolation::Nearest, ..options }).unwrap();
        assert_eq!(nearest.raster.get(0, 1, 1), 1.0);
        assert_eq!(ds.raster.get(0, 1, 1), -9999.0);
    }

    #[test]
    fn masks_distance_nan_and_edges() {
        let ds = source(vec![9., 0., 0., 0., 0., 0.], 6, 1, None);
        let mask = [1, 0, 0, 0, 0, 0];
        let filled = fill_nodata(&ds, &FillNodataOptions { mask: Some(&mask), max_distance: 2.0, ..Default::default() }).unwrap();
        assert_eq!(super::super::band_f64(&filled.raster, 0), vec![9., 9., 9., 0., 0., 0.]);
        let filled = fill_nodata(&ds, &FillNodataOptions { mask: Some(&mask), max_distance: 0.0, ..Default::default() }).unwrap();
        assert_eq!(super::super::band_f64(&filled.raster, 0), vec![9.; 6]);
        let ds = source(vec![f64::NAN, 7., f64::NAN], 1, 3, Some(f64::NAN));
        let filled = fill_nodata(&ds, &Default::default()).unwrap();
        assert_eq!(super::super::band_f64(&filled.raster, 0), vec![7.; 3]);
        let empty = fill_nodata(&ds, &FillNodataOptions { mask: Some(&[0, 0, 0]), smoothing_iterations: 2, ..Default::default() }).unwrap();
        assert!(empty.raster.get(0, 0, 0).is_nan());
        assert_eq!(empty.raster.get(0, 1, 0), 7.0);
    }

    #[test]
    fn preserves_typed_donors_and_rotated_grid() {
        let mut ds = source(vec![1., 0., 3.], 3, 1, Some(0.));
        ds.raster.data = RasterData::U64(vec![9_007_199_254_740_993, 0, 9_007_199_254_740_995]);
        ds.raster.data_type = DataType::U64;
        ds.affine = Some([3., 2., 1., 4., 1., -2.]);
        let filled = fill_nodata(&ds, &FillNodataOptions { smoothing_iterations: 3, ..Default::default() }).unwrap();
        assert_eq!(filled.affine, ds.affine);
        let RasterData::U64(v) = &filled.raster.data else {
            panic!("dtype changed");
        };
        assert_eq!(v[0], 9_007_199_254_740_993);
        assert_eq!(v[2], 9_007_199_254_740_995);
        ds.raster.nodata = 9_007_199_254_740_992.0;
        let unchanged = fill_nodata(&ds, &FillNodataOptions::default()).unwrap();
        let RasterData::U64(v) = &unchanged.raster.data else {
            panic!("dtype changed");
        };
        assert_eq!(v, &[9_007_199_254_740_993, 0, 9_007_199_254_740_995]);
    }

    #[test]
    fn rejects_invalid_options() {
        let ds = source(vec![1.], 1, 1, None);
        for distance in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(fill_nodata(&ds, &FillNodataOptions { max_distance: distance, ..Default::default() }).is_err());
        }
        assert!(fill_nodata(&ds, &FillNodataOptions { band: 0, ..Default::default() }).is_err());
        assert!(fill_nodata(&ds, &FillNodataOptions { mask: Some(&[]), ..Default::default() }).is_err());
        assert!(Interpolation::parse("cubic").is_err());
    }
}
