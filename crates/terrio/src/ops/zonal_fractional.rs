//! Exact polygon/cell intersection areas in the raster's affine pixel space.
use crate::Geometry;
use geo::{Area, BooleanOps, BoundingRect};
use rstar::{AABB, RTree, RTreeObject};

#[path = "zonal_weighted_median.rs"]
mod quantiles;
pub(super) use quantiles::median;

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
    let rect = |x0, y0, x1, y1| geo::Rect::new(geo::Coord { x: x0, y: y0 }, geo::Coord { x: x1, y: y1 }).to_polygon();
    let mut values = vec![0.0; cols * rows];
    let mut boundary = vec![false; cols];
    let mut crossings = Vec::new();
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
        let strip = boundary
            .iter()
            .any(|b| *b)
            .then(|| polygons.intersection(&geo::MultiPolygon(vec![rect(col as f64, bottom, (col + cols) as f64, top)])));
        let mut crossing = 0;
        for x in 0..cols {
            let left = (col + x) as f64;
            while crossing < crossings.len() && crossings[crossing] <= left + 0.5 {
                crossing += 1;
            }
            values[y * cols + x] = if boundary[x] {
                strip
                    .as_ref()
                    .unwrap()
                    .intersection(&geo::MultiPolygon(vec![rect(left, bottom, left + 1., top)]))
                    .unsigned_area()
                    .clamp(0., 1.)
            } else if crossing % 2 == 1 {
                1.
            } else {
                0.
            };
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::Coord;
    #[test]
    fn scanline_interior_and_boundary_candidates_match_cell_overlays() {
        let shapes = [
            vec![(-10., -10.), (20., -10.), (20., 20.), (-10., 20.), (-10., -10.)],
            vec![(0.2, -0.1), (9.7, -9.2), (0.3, -9.4), (0.2, -0.1)],
            vec![(3.2, -2.1), (3.8, -2.1), (3.8, -2.9), (3.2, -2.9), (3.2, -2.1)],
            vec![(-1., -3.), (11., -3.), (11., -7.), (-1., -7.), (-1., -3.)],
        ];
        for points in shapes {
            let g = Geometry::polygon(points.into_iter().map(|(x, y)| Coord::xy(x, y)).collect(), vec![]);
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
