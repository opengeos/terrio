//! Independent integer-tile polygon repair reproducing the polygonal content of
//! GEOS linework MakeValid (see docs/make-valid-implementation.md for provenance and validation).
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod arrangement;
mod exact;
mod topology;

use std::fmt;

/// Integer tile coordinate.
pub type IPt = (i32, i32);
/// Fractional tile coordinate.
pub type P = (f64, f64);
/// Exterior ring followed by zero or more holes, each ring closed.
pub type Polygon<T> = Vec<Vec<T>>;

/// An internal invariant violation. It never means "no area survives".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairError {
    message: &'static str,
}

impl RepairError {
    pub(crate) fn new(message: &'static str) -> Self {
        RepairError { message }
    }
}

impl fmt::Display for RepairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tile polygon repair invariant violated: {}", self.message)
    }
}

impl std::error::Error for RepairError {}

/// Fallible repair. `Ok(vec![])` means no polygonal area survives.
///
/// All polygons are one feature and are repaired jointly. Output shells are
/// clockwise and holes counter-clockwise (y-up signed area), rings are closed,
/// and every coordinate is the binary64 value nearest the exact node.
pub(crate) fn try_make_valid(polys: &[Polygon<IPt>]) -> Result<Vec<Polygon<P>>, RepairError> {
    let segments = arrangement::collect_segments(polys);
    if segments.is_empty() {
        return Ok(Vec::new());
    }
    let graph = topology::Graph::new(arrangement::build(segments)?)?;
    let faces = topology::atomic_faces(&graph)?;
    let in_r = topology::linework_fill(&graph, &faces)?;
    topology::assemble(&graph, &faces, &in_r)
}

#[cfg(test)]
mod tests;
