//! Removal of small raster polygons: an equivalent of `gdal_sieve.py` (GDAL's
//! `GDALSieveFilter()`).
//!
//! Connected areas of pixels with the same value (4- or 8-connected) smaller than a threshold
//! take the value of their largest neighbouring area, following chains of small areas to a large
//! enough one, as GDAL does: neighbours are compared in the same scan order, so that ties
//! between equally large neighbours are broken the same way. Masked pixels (the band's nodata
//! by default) are left unchanged and neither merge nor take part in merges.

use super::{Grid, build_raster};
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;

/// Which pixels take part in the sieve.
#[derive(Debug, Clone, Default)]
pub enum SieveMask<'a> {
    /// The band's own mask: pixels equal to its nodata value are excluded (the default).
    #[default]
    Default,
    /// All pixels take part (`-nomask`).
    None,
    /// Pixels where band 1 of this raster is 0 are excluded (`-mask`).
    Raster(&'a RasterDataset),
}

/// Options for [`sieve`], named after `gdal_sieve.py`'s.
#[derive(Debug, Clone)]
pub struct SieveOptions<'a> {
    /// The band to sieve, from 1.
    pub band: usize,
    /// Areas smaller than this many pixels are merged (`-st`).
    pub threshold: usize,
    /// Connect diagonal pixels too (`-8`).
    pub eight_connected: bool,
    /// The pixels taking part.
    pub mask: SieveMask<'a>,
}

impl Default for SieveOptions<'_> {
    fn default() -> Self {
        Self { band: 1, threshold: 2, eight_connected: false, mask: SieveMask::Default }
    }
}

fn find(parent: &mut [u32], mut i: u32) -> u32 {
    while parent[i as usize] != i {
        parent[i as usize] = parent[parent[i as usize] as usize];
        i = parent[i as usize];
    }
    i
}

/// The connected areas of a band: an area id per pixel (`u32::MAX` for masked pixels) and the
/// number of areas.
fn label(values: &[i64], valid: &[bool], w: usize, h: usize, eight: bool) -> (Vec<u32>, usize) {
    const NONE: u32 = u32::MAX;
    let mut ids = vec![NONE; w * h];
    let mut parent: Vec<u32> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let p = y * w + x;
            if !valid[p] {
                continue;
            }
            let v = values[p];
            let mut neighbours = [NONE; 4];
            let mut k = 0;
            let mut look = |q: usize, nb: &mut [u32; 4]| {
                if valid[q] && values[q] == v {
                    nb[k] = ids[q];
                    k += 1;
                }
            };
            if x > 0 {
                look(p - 1, &mut neighbours);
            }
            if y > 0 {
                look(p - w, &mut neighbours);
                if eight && x > 0 {
                    look(p - w - 1, &mut neighbours);
                }
                if eight && x + 1 < w {
                    look(p - w + 1, &mut neighbours);
                }
            }
            let mut id = NONE;
            for &n in neighbours.iter().filter(|n| **n != NONE) {
                let r = find(&mut parent, n);
                if id == NONE {
                    id = r;
                } else if r != id {
                    let (a, b) = (id.min(r), id.max(r));
                    parent[b as usize] = a;
                    id = a;
                }
            }
            if id == NONE {
                id = parent.len() as u32;
                parent.push(id);
            }
            ids[p] = id;
        }
    }
    // Number the areas compactly.
    let mut compact = vec![NONE; parent.len()];
    let mut n = 0usize;
    for i in 0..parent.len() as u32 {
        let r = find(&mut parent, i);
        if compact[r as usize] == NONE {
            compact[r as usize] = n as u32;
            n += 1;
        }
        compact[i as usize] = compact[r as usize];
    }
    for id in ids.iter_mut().filter(|i| **i != NONE) {
        *id = compact[*id as usize];
    }
    (ids, n)
}

/// Merges areas smaller than a threshold into their largest neighbours, as `gdal_sieve.py`
/// does. The output has the source's type, georeferencing and nodata value.
pub fn sieve(ds: &RasterDataset, o: &SieveOptions) -> Result<RasterDataset> {
    let r = &ds.raster;
    if o.band == 0 || o.band > r.bands {
        return Err(Error::InvalidArgument(format!("the raster has no band {}", o.band)));
    }
    let (w, h) = (r.cols, r.rows);
    let raw = super::band_f64(r, o.band - 1);
    let values: Vec<i64> = super::proximity::read_ints(ds, o.band, DataType::I64).into_iter().map(|v| v as i64).collect();
    let valid: Vec<bool> = match &o.mask {
        SieveMask::None => vec![true; w * h],
        SieveMask::Raster(m) => {
            if (m.raster.cols, m.raster.rows) != (w, h) {
                return Err(Error::InvalidArgument("the mask's size differs from the raster's".into()));
            }
            super::proximity::read_ints(m, 1, DataType::U8).into_iter().map(|v| v != 0.0).collect()
        }
        SieveMask::Default => match ds.nodata() {
            None => vec![true; w * h],
            Some(nd) => nodata_mask(&raw, nd, r.data_type),
        },
    };
    let (ids, n) = label(&values, &valid, w, h, o.eight_connected);
    let mut sizes = vec![0usize; n];
    let mut value_of = vec![0i64; n];
    for (p, id) in ids.iter().enumerate() {
        if *id != u32::MAX {
            sizes[*id as usize] += 1;
            value_of[*id as usize] = values[p];
        }
    }
    let sizes: Vec<i64> = sizes.into_iter().map(|s| s.min(i32::MAX as usize) as i64).collect();
    // The largest neighbour of each area, compared in GDAL's scan order.
    let mut big = vec![-1i64; n];
    let mut compare = |a: u32, b: u32| {
        if a == u32::MAX || b == u32::MAX || a == b {
            return;
        }
        let (a, b) = (a as usize, b as usize);
        if big[a] == -1 || sizes[big[a] as usize] < sizes[b] {
            big[a] = b as i64;
        }
        if big[b] == -1 || sizes[big[b] as usize] < sizes[a] {
            big[b] = a as i64;
        }
    };
    for y in 0..h {
        for x in 0..w {
            let p = y * w + x;
            if y > 0 {
                compare(ids[p], ids[p - w]);
                if x > 0 && o.eight_connected {
                    compare(ids[p], ids[p - w - 1]);
                }
                if x + 1 < w && o.eight_connected {
                    compare(ids[p], ids[p - w + 1]);
                }
            }
            if x > 0 {
                compare(ids[p], ids[p - 1]);
            }
        }
    }
    let threshold = o.threshold.min(i64::MAX as usize) as i64;
    for i in 0..n {
        if sizes[i] >= threshold {
            big[i] = -1;
            continue;
        }
        if big[i] == -1 {
            continue;
        }
        let mut visited = std::collections::HashSet::from([i as i64]);
        let mut fin = i as i64;
        let mut found = false;
        loop {
            fin = big[fin as usize];
            if fin < 0 {
                break;
            }
            if sizes[fin as usize] >= threshold {
                found = true;
                break;
            }
            if !visited.insert(fin) {
                break;
            }
        }
        if !found {
            big[i] = -1;
            continue;
        }
        let mut cur = i as i64;
        while big[cur as usize] != fin {
            let next = big[cur as usize];
            big[cur as usize] = fin;
            cur = next;
        }
    }
    // Pixels of merged areas take their new area's value; the others keep theirs.
    let out: Vec<f64> = (0..w * h)
        .map(|p| {
            let id = ids[p];
            if id != u32::MAX && big[id as usize] != -1 { value_of[big[id as usize] as usize] as f64 } else { values[p] as f64 }
        })
        .collect();
    build_raster(Grid::of(r), vec![out], r.data_type, ds.nodata(), &ds.crs(), super::convert_value)
}

/// GDAL's nodata mask: pixels equal to the nodata value (NaN matching NaN; floats compared with
/// GDAL's tolerance).
pub(crate) fn nodata_mask(values: &[f64], nd: f64, dt: DataType) -> Vec<bool> {
    values.iter().map(|v| super::gdal_nodata_valid(*v, nd, dt)).collect()
}

/// Typed mask without allocating a widened f64 copy of the band.
pub(crate) fn nodata_mask_view(view: super::BandView<'_>, nd: f64, dt: DataType) -> Vec<bool> {
    crate::par::map_range(view.len(), |i| super::gdal_nodata_valid(view.get(i), nd, dt))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(values: Vec<f64>, w: usize, h: usize, nodata: Option<f64>) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [0.0, 1.0, 0.0, h as f64, 0.0, -1.0] };
        build_raster(grid, vec![values], DataType::U8, nodata, &crate::crs::SpatialRef::default(), super::super::convert_value).unwrap()
    }

    #[test]
    fn removes_small_areas() {
        #[rustfmt::skip]
        let v = vec![
            1.0, 1.0, 1.0, 2.0, 2.0,
            1.0, 3.0, 1.0, 2.0, 2.0,
            1.0, 1.0, 1.0, 2.0, 4.0,
        ];
        let src = ds(v, 5, 3, None);
        let out = sieve(&src, &SieveOptions { threshold: 2, ..Default::default() }).unwrap();
        assert_eq!(out.raster.get(0, 1, 1), 1.0);
        assert_eq!(out.raster.get(0, 2, 4), 2.0);
        assert_eq!(out.raster.get(0, 0, 3), 2.0);
        // The nodata value is masked out: the single 4 stays.
        let src = ds(vec![1.0, 1.0, 4.0, 1.0, 1.0, 1.0], 3, 2, Some(4.0));
        let out = sieve(&src, &SieveOptions::default()).unwrap();
        assert_eq!(out.raster.get(0, 0, 2), 4.0);
        assert_eq!(out.nodata(), Some(4.0));
        let out = sieve(&src, &SieveOptions { mask: SieveMask::None, ..Default::default() }).unwrap();
        assert_eq!(out.raster.get(0, 0, 2), 1.0);
    }

    #[test]
    fn connectivity() {
        #[rustfmt::skip]
        let v = vec![
            5.0, 0.0, 0.0,
            0.0, 5.0, 0.0,
            0.0, 0.0, 0.0,
        ];
        let four = sieve(&ds(v.clone(), 3, 3, None), &SieveOptions { threshold: 2, ..Default::default() }).unwrap();
        assert_eq!(four.raster.get(0, 1, 1), 0.0);
        let eight = sieve(&ds(v, 3, 3, None), &SieveOptions { threshold: 2, eight_connected: true, ..Default::default() }).unwrap();
        assert_eq!(eight.raster.get(0, 1, 1), 5.0);
    }
}
