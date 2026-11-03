//! Exact polygon/cell intersection areas in the raster's affine pixel space.
use crate::Geometry;
use geo::{Area, BooleanOps};

#[path = "zonal_weighted_median.rs"]
mod quantiles;
pub(super) use quantiles::median;

pub(super) fn coverage(geometry: &Geometry, col: usize, row: usize, cols: usize, rows: usize) -> Vec<f64> {
    let polygons = match super::super::topology::to_geo(geometry) {
        geo::Geometry::Polygon(p) => geo::MultiPolygon(vec![p]),
        geo::Geometry::MultiPolygon(p) => p,
        _ => unreachable!("zonal input was checked"),
    };
    let rect = |x0, y0, x1, y1| geo::Rect::new(geo::Coord { x: x0, y: y0 }, geo::Coord { x: x1, y: y1 }).to_polygon();
    let mut values = vec![0.0; cols * rows];
    for y in 0..rows {
        // Clip once per row so pixel overlays do not repeatedly traverse a large zone.
        let bottom = -((row + y + 1) as f64);
        let strip = polygons.intersection(&geo::MultiPolygon(vec![rect(col as f64, bottom, (col + cols) as f64, bottom + 1.0)]));
        for x in 0..cols {
            let left = (col + x) as f64;
            let cell = rect(left, bottom, left + 1.0, bottom + 1.0);
            values[y * cols + x] = strip.intersection(&geo::MultiPolygon(vec![cell])).unsigned_area().clamp(0.0, 1.0);
        }
    }
    values
}
