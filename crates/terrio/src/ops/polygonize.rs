//! Turning a raster band into polygons, the equivalent of `gdal_polygonize`.
//!
//! Each connected region of pixels with the same value becomes one polygon feature, with holes
//! where other regions lie inside it. Regions connect through pixel sides (4-connectedness) or
//! also through corners (8-connectedness). Pixels that are nodata, NaN, or zero in the mask are
//! left out. Polygon vertices lie on pixel corners; exterior rings run counter-clockwise and holes
//! clockwise, and only the corners where a boundary turns are kept.

use crate::{Error, RasterDataset, Result};
use wbraster::DataType;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

/// Options for [`polygonize`], named after `gdal_polygonize`'s.
#[derive(Debug, Clone)]
pub struct PolygonizeOptions {
    /// The band to polygonize (0-based).
    pub band: usize,
    /// The name of the field receiving the pixel values. `DN` by default.
    pub field_name: String,
    /// The output layer name.
    pub layer_name: String,
    /// Connect pixels through their corners too (8-connectedness).
    pub connectedness8: bool,
    /// `-nomask`: do not leave out nodata pixels.
    pub ignore_nodata: bool,
    /// `-mask`: leave out the pixels where the first band of this raster, on the same grid, is 0.
    pub mask: Option<RasterDataset>,
}

impl Default for PolygonizeOptions {
    fn default() -> Self {
        Self { band: 0, field_name: "DN".into(), layer_name: "polygonized".into(), connectedness8: false, ignore_nodata: false, mask: None }
    }
}

/// Directions of boundary edges between pixel corners, in a grid whose rows grow downwards.
const EAST: u8 = 1;
const NORTH: u8 = 2;
const WEST: u8 = 4;
const SOUTH: u8 = 8;

/// Polygonizes a raster band into a layer of polygons with the pixel value in a field.
pub fn polygonize(ds: &RasterDataset, options: &PolygonizeOptions) -> Result<Layer> {
    let r = &ds.raster;
    if options.band >= r.bands {
        return Err(Error::InvalidArgument(format!("band {} does not exist (the raster has {})", options.band + 1, r.bands)));
    }
    let (cols, rows) = (r.cols, r.rows);
    let values = r.band_to_vec_f64(options.band);
    let nodata = if options.ignore_nodata { None } else { ds.nodata() };
    let mask = match &options.mask {
        Some(m) if (m.raster.cols, m.raster.rows) != (cols, rows) => {
            return Err(Error::InvalidArgument("the mask raster must have the size of the input".into()));
        }
        Some(m) => Some(m.raster.band_to_vec_f64(0)),
        None => None,
    };
    let valid = |i: usize| {
        let v = values[i];
        !v.is_nan() && nodata.is_none_or(|nd| v != nd) && mask.as_ref().is_none_or(|m| m[i] != 0.0)
    };

    // Label connected regions with a union-find over equal, valid neighbours.
    const NONE: u32 = u32::MAX;
    let mut labels = vec![NONE; cols * rows];
    let mut parent: Vec<u32> = Vec::new();
    fn find(parent: &mut [u32], mut a: u32) -> u32 {
        while parent[a as usize] != a {
            parent[a as usize] = parent[parent[a as usize] as usize];
            a = parent[a as usize];
        }
        a
    }
    for row in 0..rows {
        for col in 0..cols {
            let i = row * cols + col;
            if !valid(i) {
                continue;
            }
            let mut neighbours = [usize::MAX; 4];
            if col > 0 {
                neighbours[0] = i - 1;
            }
            if row > 0 {
                neighbours[1] = i - cols;
                if options.connectedness8 && col > 0 {
                    neighbours[2] = i - cols - 1;
                }
                if options.connectedness8 && col + 1 < cols {
                    neighbours[3] = i - cols + 1;
                }
            }
            let mut label = NONE;
            for j in neighbours.into_iter().filter(|&j| j != usize::MAX) {
                if labels[j] == NONE || values[j] != values[i] {
                    continue;
                }
                let lj = find(&mut parent, labels[j]);
                if label == NONE {
                    label = lj;
                } else if lj != label {
                    let (a, b) = (label.min(lj), label.max(lj));
                    parent[b as usize] = a;
                    label = a;
                }
            }
            if label == NONE {
                label = parent.len() as u32;
                parent.push(label);
            }
            labels[i] = label;
        }
    }
    // Final labels, numbered by first pixel in scan order.
    let mut number = vec![NONE; parent.len()];
    let mut count = 0u32;
    for l in labels.iter_mut().filter(|l| **l != NONE) {
        let root = find(&mut parent, *l) as usize;
        if number[root] == NONE {
            number[root] = count;
            count += 1;
        }
        *l = number[root];
    }
    drop(parent);

    // Boundary edges, oriented with their region on the left (in map coordinates, y up).
    let vcols = cols + 1;
    let mut edges = vec![0u8; vcols * (rows + 1)];
    let label_at = |row: isize, col: isize| -> u32 {
        if row < 0 || col < 0 || row >= rows as isize || col >= cols as isize { NONE } else { labels[row as usize * cols + col as usize] }
    };
    for row in 0..rows {
        for col in 0..cols {
            let l = labels[row * cols + col];
            if l == NONE {
                continue;
            }
            let (ri, ci) = (row as isize, col as isize);
            if label_at(ri - 1, ci) != l {
                edges[row * vcols + col + 1] |= WEST;
            }
            if label_at(ri + 1, ci) != l {
                edges[(row + 1) * vcols + col] |= EAST;
            }
            if label_at(ri, ci - 1) != l {
                edges[row * vcols + col] |= SOUTH;
            }
            if label_at(ri, ci + 1) != l {
                edges[(row + 1) * vcols + col + 1] |= NORTH;
            }
        }
    }
    // The pixel an edge leaving vertex (col, row) in a direction bounds.
    let bounded = |col: usize, row: usize, dir: u8| -> u32 {
        let (c, r) = (col as isize, row as isize);
        match dir {
            SOUTH => label_at(r, c),
            EAST => label_at(r - 1, c),
            NORTH => label_at(r - 1, c - 1),
            _ => label_at(r, c - 1),
        }
    };
    let step = |col: usize, row: usize, dir: u8| match dir {
        EAST => (col + 1, row),
        WEST => (col - 1, row),
        NORTH => (col, row - 1),
        _ => (col, row + 1),
    };
    // At a vertex where a region touches itself diagonally, turning left keeps the two pixels
    // apart (4-connectedness); turning right joins them (8-connectedness).
    let left = |d: u8| match d {
        SOUTH => EAST,
        EAST => NORTH,
        NORTH => WEST,
        _ => SOUTH,
    };
    let right = |d: u8| match d {
        SOUTH => WEST,
        WEST => NORTH,
        NORTH => EAST,
        _ => SOUTH,
    };

    let gt = ds.geo_transform();
    let georef = |col: usize, row: usize| {
        let (c, r) = (col as f64, row as f64);
        Coord::xy(gt[0] + c * gt[1] + r * gt[2], gt[3] + c * gt[4] + r * gt[5])
    };
    let mut rings: Vec<Vec<Vec<Coord>>> = vec![Vec::new(); count as usize];
    for v0 in 0..edges.len() {
        while edges[v0] != 0 {
            let (col0, row0) = (v0 % vcols, v0 / vcols);
            let dir0 = edges[v0] & edges[v0].wrapping_neg();
            let label = bounded(col0, row0, dir0);
            edges[v0] &= !dir0;
            let mut ring = vec![(col0, row0)];
            let (mut col, mut row, mut dir) = (col0, row0, dir0);
            loop {
                (col, row) = step(col, row, dir);
                let v = row * vcols + col;
                let at_start = (col, row) == (col0, row0);
                // The edges leaving this vertex along the region; at the start, the first edge too.
                let mut out = 0u8;
                for d in [EAST, NORTH, WEST, SOUTH] {
                    if (edges[v] & d != 0 && bounded(col, row, d) == label) || (at_start && d == dir0) {
                        out |= d;
                    }
                }
                let next = match out.count_ones() {
                    0 => break,
                    1 => out,
                    _ if options.connectedness8 => right(dir),
                    _ => left(dir),
                };
                if at_start && next == dir0 {
                    break;
                }
                if next != dir {
                    ring.push((col, row));
                }
                edges[v] &= !next;
                dir = next;
            }
            // With 4-connectedness, a ring through the same corner twice is an exterior and a hole
            // (or two holes) touching there: split it, as GDAL does, so that rings are simple.
            let parts = if options.connectedness8 { vec![ring] } else { split_at_repeats(ring) };
            for mut part in parts {
                // The start corner may be in the middle of a straight run.
                if part.len() > 2 {
                    let (a, b, c) = (part[part.len() - 1], part[0], part[1]);
                    if (a.0 == b.0 && b.0 == c.0) || (a.1 == b.1 && b.1 == c.1) {
                        part.remove(0);
                    }
                }
                let mut coords: Vec<Coord> = part.iter().map(|&(c, r)| georef(c, r)).collect();
                if let Some(first) = coords.first().cloned() {
                    coords.push(first);
                }
                rings[label as usize].push(coords);
            }
        }
    }

    let integer = !matches!(ds.raster.data_type, DataType::F32 | DataType::F64);
    let mut layer = Layer::new(options.layer_name.clone()).with_geom_type(GeometryType::Polygon);
    ds.crs().apply_to_layer(&mut layer);
    layer.schema.add_field(FieldDef::new(options.field_name.clone(), if integer { FieldType::Integer } else { FieldType::Float }));
    let mut value_of = vec![f64::NAN; count as usize];
    for (i, l) in labels.iter().enumerate() {
        if *l != NONE && value_of[*l as usize].is_nan() {
            value_of[*l as usize] = values[i];
        }
    }
    for (label, region) in rings.into_iter().enumerate() {
        let mut exterior: Option<Vec<Coord>> = None;
        let mut holes = Vec::new();
        for mut ring in region {
            let area = signed_area(&ring);
            if area > 0.0 && exterior.as_ref().is_none_or(|e| signed_area(e) < area) {
                if let Some(mut e) = exterior.replace(ring) {
                    e.reverse();
                    holes.push(e);
                }
            } else {
                if area > 0.0 {
                    ring.reverse();
                }
                holes.push(ring);
            }
        }
        let Some(mut exterior) = exterior else { continue };
        // South-up grids flip the orientation: exteriors run counter-clockwise, holes clockwise.
        if signed_area(&exterior) < 0.0 {
            exterior.reverse();
        }
        for h in &mut holes {
            if signed_area(h) > 0.0 {
                h.reverse();
            }
        }
        let v = value_of[label];
        let value = if integer { FieldValue::Integer(v as i64) } else { FieldValue::Float(v) };
        let geometry = Geometry::Polygon { exterior: Ring(exterior), interiors: holes.into_iter().map(Ring).collect() };
        layer.features.push(Feature { fid: label as u64, geometry: Some(geometry), attributes: vec![value] });
    }
    Ok(layer)
}

/// Splits a closed ring (without its closing vertex) that passes through a vertex more than once
/// into rings that do not.
fn split_at_repeats(ring: Vec<(usize, usize)>) -> Vec<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::with_capacity(ring.len());
    let mut position = std::collections::HashMap::new();
    for v in ring {
        if let Some(&p) = position.get(&v) {
            let sub: Vec<_> = stack.drain(p..).collect();
            for u in &sub[1..] {
                position.remove(u);
            }
            out.push(sub);
        }
        position.insert(v, stack.len());
        stack.push(v);
    }
    out.push(stack);
    out
}

/// Twice the signed area of a closed ring: positive when counter-clockwise.
fn signed_area(r: &[Coord]) -> f64 {
    r.windows(2).map(|w| w[0].x * w[1].y - w[1].x * w[0].y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::{Raster, RasterConfig};

    fn raster(cols: usize, rows: usize, data: &[f64], nodata: Option<f64>) -> RasterDataset {
        let cfg = RasterConfig {
            cols,
            rows,
            x_min: 100.0,
            y_min: 200.0,
            cell_size: 10.0,
            nodata: nodata.unwrap_or(-9999.0),
            data_type: DataType::U8,
            ..Default::default()
        };
        RasterDataset::new(Raster::from_data(cfg, data.to_vec()).unwrap(), nodata.is_some())
    }

    fn polygons(l: &Layer) -> Vec<(i64, f64, usize)> {
        l.features
            .iter()
            .map(|f| {
                let Some(Geometry::Polygon { exterior, interiors }) = &f.geometry else { panic!() };
                let area = (signed_area(&exterior.0) + interiors.iter().map(|h| signed_area(&h.0)).sum::<f64>()) / 2.0;
                (f.attributes[0].as_i64().unwrap(), area, interiors.len())
            })
            .collect()
    }

    #[test]
    fn regions_with_holes_and_nodata() {
        #[rustfmt::skip]
        let data = [
            1.0, 1.0, 1.0, 1.0,
            1.0, 2.0, 2.0, 1.0,
            1.0, 2.0, 0.0, 1.0,
            1.0, 1.0, 1.0, 1.0,
        ];
        let ds = raster(4, 4, &data, Some(0.0));
        let l = polygonize(&ds, &Default::default()).unwrap();
        assert_eq!(polygons(&l), vec![(1, 1200.0, 1), (2, 300.0, 0)]);
        let Some(Geometry::Polygon { exterior, interiors }) = &l.features[0].geometry else { panic!() };
        // Only corners are kept, on pixel edges: the 4 corners of the square, closed.
        assert_eq!(exterior.0.len(), 5);
        assert!(exterior.0.iter().all(|c| [100.0, 140.0].contains(&c.x) && [200.0, 240.0].contains(&c.y)));
        assert!(signed_area(&exterior.0) > 0.0 && signed_area(&interiors[0].0) < 0.0);
        // The hole is the L of 2s plus the nodata pixel.
        assert_eq!(interiors[0].0.len(), 5);

        let o = PolygonizeOptions { ignore_nodata: true, ..Default::default() };
        assert_eq!(polygonize(&ds, &o).unwrap().len(), 3);
    }

    #[test]
    fn connectedness() {
        #[rustfmt::skip]
        let data = [
            5.0, 0.0, 5.0,
            0.0, 5.0, 0.0,
            5.0, 0.0, 5.0,
        ];
        let ds = raster(3, 3, &data, None);
        let four = polygonize(&ds, &Default::default()).unwrap();
        assert_eq!(four.len(), 9);
        let eight = polygonize(&ds, &PolygonizeOptions { connectedness8: true, ..Default::default() }).unwrap();
        let p = polygons(&eight);
        assert_eq!(p.len(), 2);
        assert_eq!(p.iter().map(|x| x.1).sum::<f64>(), 900.0);
        // The diagonal of 5s joins; so do the 0s, around the centre pixel, which is their hole.
        assert_eq!(p, vec![(5, 500.0, 0), (0, 400.0, 1)]);
    }
}
