//! Leaf packing from JTS 1.20.0 `index/strtree/STRtree.java`.
//! Copyright (c) 2016 Vivid Solutions. Distributed under EDL-1.0 (see NOTICE).
//!
//! This is the first packed level, the ordering used by GDAL's spatial sort.
//! It does not construct an index or reorder leaves by parent-node bounds.

type Envelope = (f64, f64, f64, f64);

/// Returns packed leaf item IDs. Equal centres retain input order; GEOS's
/// unstable ordering of ties is deliberately not reproduced.
pub(crate) fn leaf_order(boxes: &[Envelope]) -> Vec<usize> {
    const CAPACITY: usize = 10;
    let mut items: Vec<usize> = (0..boxes.len()).collect();
    if items.len() < 2 {
        return items;
    }
    let centre = |id: usize, axis: usize| {
        let b = boxes[id];
        if axis == 0 { (b.0 + b.2) / 2.0 } else { (b.1 + b.3) / 2.0 }
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
