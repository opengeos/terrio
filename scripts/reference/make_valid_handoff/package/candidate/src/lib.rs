//! Contract-only scaffold. The independent repair has not been implemented.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// Integer tile coordinate.
pub type IPt = (i32, i32);
/// Fractional tile coordinate.
pub type P = (f64, f64);
/// Exterior ring followed by zero or more holes, each ring closed.
pub type Polygon<T> = Vec<Vec<T>>;
/// Required repair signature.
pub type Repair = fn(&[Polygon<IPt>]) -> Vec<Polygon<P>>;

/// Wire an independently implemented `make_valid` here by returning `Some(make_valid)`.
/// `None` explicitly reports an unimplemented candidate, never a successful empty repair.
pub fn implementation() -> Option<Repair> {
    None
}
