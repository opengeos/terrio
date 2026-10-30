//! Leaf packing from JTS 1.20.0 `index/strtree/STRtree.java`.
//! Copyright (c) 2016 Vivid Solutions. Distributed under EDL-1.0 (see NOTICE).
//!
//! This is the first packed level, the ordering used by GDAL's spatial sort.
//! It does not construct an index or reorder leaves by parent-node bounds.

type Envelope = (f64, f64, f64, f64);

/// Returns packed leaf item IDs. Equal centres retain input order; GEOS's
/// unstable ordering of ties is deliberately not reproduced.
/// Callers supply finite boxes. Numeric comparison keeps signed zeros tied.
pub(crate) fn leaf_order(boxes: &[Envelope]) -> Vec<usize> {
    const CAPACITY: usize = 10;
    let mut items: Vec<usize> = (0..boxes.len()).collect();
    if items.len() < 2 {
        return items;
    }
    let centre = |id: usize, axis: usize| {
        let b = boxes[id];
        if axis == 0 { b.0.midpoint(b.2) } else { b.1.midpoint(b.3) }
    };
    items.sort_by(|&a, &b| centre(a, 0).partial_cmp(&centre(b, 0)).unwrap_or(std::cmp::Ordering::Equal));
    let leaf_count = items.len().div_ceil(CAPACITY);
    let slices = (leaf_count as f64).sqrt().ceil() as usize;
    let slice_size = items.len().div_ceil(slices);
    for slice in items.chunks_mut(slice_size) {
        slice.sort_by(|&a, &b| centre(a, 1).partial_cmp(&centre(b, 1)).unwrap_or(std::cmp::Ordering::Equal));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_zero_centres_remain_tied() {
        let boxes = [(0., 0., 0., 0.), (0., -0., 0., -0.)];
        assert_eq!(leaf_order(&boxes), [0, 1]);
    }

    #[test]
    fn signed_zero_x_centres_remain_tied_across_slices() {
        let boxes = (0..24)
            .map(|i| {
                let x = if i < 12 { 0. } else { -0. };
                (x, 0., x, 0.)
            })
            .collect::<Vec<_>>();
        assert_eq!(leaf_order(&boxes), (0..24).collect::<Vec<_>>());
    }

    #[test]
    fn packing_preserves_distinct_centres_near_coordinate_limits() {
        // More than one slice is essential: its X partition must be correct
        // before Y sorting, even when the unhalved centre sum overflows.
        let mut boxes = (0..24)
            .rev()
            .map(|i| {
                let x = 1e308 + i as f64 * 1e306;
                (x, i as f64, x, i as f64)
            })
            .collect::<Vec<_>>();
        boxes.push((-1e308, 0.0, 1e308, 0.0));
        let normalized = boxes.iter().map(|b| (b.0 * 0.5, b.1, b.2 * 0.5, b.3)).collect::<Vec<_>>();
        assert_eq!(leaf_order(&boxes), leaf_order(&normalized));
    }
}
