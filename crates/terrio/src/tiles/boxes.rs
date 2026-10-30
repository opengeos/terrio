//! Envelope candidate pairs for the existing clipping-failure predictor.
//!
//! This adapter uses the existing `rstar` dependency so retiring the legacy
//! repair also removes its shared grid-index helper. It contains no repair code.

use rstar::{
    RTree, RTreeObject,
    primitives::{GeomWithData, Rectangle},
};

pub(super) fn intersecting_pairs(boxes: &[(f64, f64, f64, f64)]) -> Vec<(usize, usize)> {
    let entries: Vec<_> = boxes
        .iter()
        .enumerate()
        .filter(|(_, b)| [b.0, b.1, b.2, b.3].iter().all(|v| v.is_finite()))
        .map(|(index, &(x0, y0, x1, y1))| GeomWithData::new(Rectangle::from_corners([x0, y0], [x1, y1]), index))
        .collect();
    let tree = RTree::bulk_load(entries);
    let mut pairs = Vec::new();
    for entry in tree.iter() {
        for other in tree.locate_in_envelope_intersecting(entry.envelope()) {
            if entry.data < other.data {
                pairs.push((entry.data, other.data));
            }
        }
    }
    pairs.sort_unstable();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_touching_and_degenerate_boxes_once() {
        let boxes = [(0., 0., 2., 2.), (2., 1., 3., 2.), (1., 1., 1., 1.), (9., 9., 10., 10.)];
        assert_eq!(intersecting_pairs(&boxes), [(0, 1), (0, 2)]);
        assert!(intersecting_pairs(&[]).is_empty());
        assert!(intersecting_pairs(&[(f64::NAN, 0., 1., 1.)]).is_empty());
    }
}
