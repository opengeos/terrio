//! Exact polygon/cell intersection areas in the raster's affine pixel space.
use crate::Geometry;
use geo::BoundingRect;
use rstar::{AABB, RTree, RTreeObject};

#[path = "zonal_weighted_median.rs"]
mod quantiles;
pub(super) use quantiles::quantile;

#[derive(Clone)]
struct Edge(geo::Line<f64>);
impl RTreeObject for Edge {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        let (a, b) = (self.0.start, self.0.end);
        AABB::from_corners([a.x.min(b.x), a.y.min(b.y)], [a.x.max(b.x), a.y.max(b.y)])
    }
}

pub(super) fn coverage(geometry: &Geometry, col: usize, row: usize, cols: usize, rows: usize) -> Vec<f64> {
    let polygons = match super::super::topology::to_geo(geometry) {
        geo::Geometry::Polygon(p) => geo::MultiPolygon(vec![p]),
        geo::Geometry::MultiPolygon(p) => p,
        _ => unreachable!("zonal input was checked"),
    };
    let Some(bounds) = polygons.bounding_rect() else { return vec![0.; cols * rows] };
    let edges = RTree::bulk_load(
        polygons
            .0
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .flat_map(|ring| ring.lines().map(Edge))
            .collect(),
    );
    let rings: Vec<Vec<Vec<[f64; 2]>>> = polygons
        .0
        .iter()
        .map(|p| std::iter::once(p.exterior()).chain(p.interiors()).map(|r| r.0.iter().map(|c| [c.x, c.y]).collect()).collect())
        .collect();
    let mut values = vec![0.0; cols * rows];
    let mut boundary = vec![false; cols];
    let mut crossings = Vec::new();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for y in 0..rows {
        let bottom = -((row + y + 1) as f64);
        let top = bottom + 1.;
        let middle = bottom + 0.5;
        boundary.fill(false);
        crossings.clear();
        // Include edges outside the raster's X bounds: their crossings determine whether
        // an entire row is interior when a polygon surrounds the requested window.
        let query = AABB::from_corners([bounds.min().x, bottom], [bounds.max().x, top]);
        for edge in edges.locate_in_envelope_intersecting(query) {
            let (a, b) = (edge.0.start, edge.0.end);
            if (a.y > middle) != (b.y > middle) {
                crossings.push(a.x + (middle - a.y) / (b.y - a.y) * (b.x - a.x));
            }
            let (x0, x1) = if a.y == b.y {
                (a.x, b.x)
            } else {
                let t0 = ((bottom - a.y) / (b.y - a.y)).clamp(0., 1.);
                let t1 = ((top - a.y) / (b.y - a.y)).clamp(0., 1.);
                (a.x + t0 * (b.x - a.x), a.x + t1 * (b.x - a.x))
            };
            // One-cell padding makes this a conservative boundary candidate mask.
            // Only candidates use overlay; scanline parity classifies all other cells.
            let start = (x0.min(x1).floor() - col as f64 - 1.).clamp(0., cols as f64) as usize;
            let end = (x0.max(x1).ceil() - col as f64 + 1.).clamp(0., cols as f64) as usize;
            boundary[start..end].fill(true);
        }
        crossings.sort_by(f64::total_cmp);
        // Every ring clipped to this row; boundary cells clip these again to their columns.
        // ponytail: each ring is clipped whole per row, O(vertices × rows); look up the row's
        // edges in the R-tree instead if polygons with very many vertices become slow.
        let strip: Vec<Vec<Vec<[f64; 2]>>> = if boundary.iter().any(|b| *b) {
            rings
                .iter()
                .map(|polygon| {
                    polygon
                        .iter()
                        .map(|ring| {
                            clip(ring, &mut a, 1, bottom, true);
                            let mut out = Vec::new();
                            clip(&a, &mut out, 1, top, false);
                            out
                        })
                        .collect()
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut crossing = 0;
        for x in 0..cols {
            let left = (col + x) as f64;
            while crossing < crossings.len() && crossings[crossing] <= left + 0.5 {
                crossing += 1;
            }
            values[y * cols + x] = if boundary[x] {
                let mut area = 0.;
                for polygon in &strip {
                    for (k, ring) in polygon.iter().enumerate() {
                        clip(ring, &mut a, 0, left, true);
                        clip(&a, &mut b, 0, left + 1., false);
                        let part = shoelace(&b, left, bottom).abs();
                        area += if k == 0 { part } else { -part };
                    }
                }
                area.clamp(0., 1.)
            } else if crossing % 2 == 1 {
                1.
            } else {
                0.
            };
        }
    }
    values
}

/// Sutherland–Hodgman: `ring` clipped to the half-plane `p[axis] >= v` (`keep_above`) or
/// `p[axis] <= v`. The output may run along the clip line, but its signed area is exactly the
/// part of the ring inside the half-plane, holes and concave rings included.
fn clip(ring: &[[f64; 2]], out: &mut Vec<[f64; 2]>, axis: usize, v: f64, keep_above: bool) {
    out.clear();
    let inside = |p: &[f64; 2]| if keep_above { p[axis] >= v } else { p[axis] <= v };
    let Some(mut prev) = ring.last().copied() else { return };
    for &cur in ring {
        if inside(&prev) != inside(&cur) {
            let t = (v - prev[axis]) / (cur[axis] - prev[axis]);
            let mut p = [prev[0] + t * (cur[0] - prev[0]), prev[1] + t * (cur[1] - prev[1])];
            p[axis] = v;
            out.push(p);
        }
        if inside(&cur) {
            out.push(cur);
        }
        prev = cur;
    }
}

/// Signed area of a ring, relative to (`x0`, `y0`) to keep cell-sized areas precise.
fn shoelace(ring: &[[f64; 2]], x0: f64, y0: f64) -> f64 {
    let Some(&last) = ring.last() else { return 0. };
    let (mut px, mut py) = (last[0] - x0, last[1] - y0);
    let mut twice = 0.;
    for p in ring {
        let (x, y) = (p[0] - x0, p[1] - y0);
        twice += px * y - x * py;
        (px, py) = (x, y);
    }
    twice / 2.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::Coord;
    use geo::{Area, BooleanOps};
    /// An exterior ring and its holes, as (x, y) points.
    type Shape = (&'static [(f64, f64)], &'static [&'static [(f64, f64)]]);
    #[test]
    fn scanline_interior_and_boundary_candidates_match_cell_overlays() {
        let ring = |points: &[(f64, f64)]| points.iter().map(|&(x, y)| Coord::xy(x, y)).collect::<Vec<_>>();
        let shapes: [Shape; 6] = [
            (&[(-10., -10.), (20., -10.), (20., 20.), (-10., 20.), (-10., -10.)], &[]),
            (&[(0.2, -0.1), (9.7, -9.2), (0.3, -9.4), (0.2, -0.1)], &[]),
            (&[(3.2, -2.1), (3.8, -2.1), (3.8, -2.9), (3.2, -2.9), (3.2, -2.1)], &[]),
            (&[(-1., -3.), (11., -3.), (11., -7.), (-1., -7.), (-1., -3.)], &[]),
            // Concave: a U whose arms and notch share rows and cells.
            (&[(0.3, -0.4), (9.6, -0.2), (9.4, -9.7), (6.2, -9.5), (6.6, -3.3), (3.1, -3.6), (3.4, -9.1), (0.5, -9.8), (0.3, -0.4)], &[]),
            // Clockwise exterior with a hole inside one cell, and one crossing several cells.
            (
                &[(0.5, -0.5), (0.5, -9.5), (9.5, -9.5), (9.5, -0.5), (0.5, -0.5)],
                &[
                    &[(2.2, -2.2), (2.8, -2.2), (2.8, -2.8), (2.2, -2.8), (2.2, -2.2)],
                    &[(4.3, -4.1), (7.7, -5.2), (5.1, -7.9), (4.3, -4.1)],
                ],
            ),
        ];
        for (exterior, holes) in shapes {
            let g = Geometry::polygon(ring(exterior), holes.iter().map(|h| ring(h)).collect());
            let fast = coverage(&g, 0, 0, 10, 10);
            let geo::Geometry::Polygon(p) = super::super::super::topology::to_geo(&g) else { unreachable!() };
            for y in 0..10 {
                for x in 0..10 {
                    let cell =
                        geo::Rect::new(geo::Coord { x: x as f64, y: -(y as f64) - 1. }, geo::Coord { x: x as f64 + 1., y: -(y as f64) })
                            .to_polygon();
                    let expected = p.intersection(&cell).unsigned_area();
                    assert!((fast[y * 10 + x] - expected).abs() < 1e-6, "cell {x}/{y}: {} != {expected}", fast[y * 10 + x]);
                }
            }
        }
    }
}
