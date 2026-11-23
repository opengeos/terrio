// SPDX-FileCopyrightText: Copyright (c) 2016 Vivid Solutions.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   JTS 1.20.0 (commit 6e95fe82feb986a7aa657f4ffa406d8c290af509):
//     modules/core/src/main/java/org/locationtech/jts/simplify/DouglasPeuckerSimplifier.java
//     modules/core/src/main/java/org/locationtech/jts/simplify/DouglasPeuckerLineSimplifier.java
//     modules/core/src/main/java/org/locationtech/jts/geom/util/GeometryTransformer.java
//     modules/core/src/main/java/org/locationtech/jts/geom/CoordinateArrays.java
//     modules/core/src/main/java/org/locationtech/jts/geom/CoordinateList.java
//     modules/core/src/main/java/org/locationtech/jts/geom/LineSegment.java
//     modules/core/src/main/java/org/locationtech/jts/algorithm/Distance.java

//! Douglas-Peucker simplification of lines and polygons, derived from JTS 1.20.0's
//! `DouglasPeuckerSimplifier` and `DouglasPeuckerLineSimplifier`, and run directly on Well-Known
//! Binary. Copyright (c) 2016 Vivid Solutions. EDL-1.0; see NOTICE and docs/jts-provenance.md.
//! GEOS (shapely's `simplify(..., preserve_topology=False)`) is only a behavioral oracle in the
//! tests.
//!
//! [`simplify_wkb`] and [`simplify_wkb_many`] (in parallel over features) simplify WKB without
//! decoding it into geometry objects:
//!
//! - Every linear component is simplified on its own (`DouglasPeuckerLineSimplifier`): a section
//!   is replaced by its chord when no vertex is farther than the tolerance from the chord segment
//!   (a distance equal to the tolerance flattens it), otherwise it is split at its farthest vertex
//!   (the first one on ties). Line endpoints stay, closed lines included. The distances are
//!   JTS's `Distance.pointToSegment`, as the topology-preserving simplifier
//!   ([`crate::ops::simplify`]) computes them: the square root of the sum of squares, where JTS
//!   1.20's `Coordinate.distance` uses `Math.hypot` (the two can differ in the last bit).
//! - A polygon ring may also lose its start point (`simplifyRingEndpoint`): when the simplified
//!   ring has at least four points and its start lies within the tolerance of the segment joining
//!   its neighbours, the ring is closed on its second point instead. The four points include the
//!   closing one, so a thin triangle can lose its start and collapse, in JTS and GEOS alike.
//! - A ring left with fewer than four points has collapsed. With [`RingCollapse::Keep`] (the
//!   default) it keeps its input vertices, so that no polygon or hole disappears, and the
//!   collapsed rings are counted ([`SimplifyStats::rings_kept`]). [`RingCollapse::Drop`] does as
//!   JTS: a collapsed or empty hole is removed, a polygon whose shell collapses becomes empty
//!   (what JTS's repair step makes of the holes it is left with), and empty parts leave their
//!   multi geometry or collection. A multi geometry left with one part keeps its type, where
//!   JTS's `buildGeometry` returns the part alone.
//! - There is no repair step: this is `DouglasPeuckerSimplifier` with `setEnsureValid(false)`.
//!   Douglas-Peucker can make a ring self-intersect or a hole cross its shell when the tolerance is
//!   large against the feature; use JTS's topology-preserving simplifier
//!   ([`crate::ops::simplify::simplify_preserve_topology`]) where that matters.
//!
//! Every output vertex is an input vertex, copied with its Z and M. Each (sub)geometry keeps its
//! byte order and type code (ISO or extended WKB, with an SRID). Points and empty geometries pass
//! through. A feature with a non-finite X or Y passes through unchanged and is counted
//! ([`SimplifyStats::non_finite`]).
//!
//! The sections are simplified from an explicit work stack instead of JTS's recursion, so that a
//! ring of millions of vertices cannot exhaust the call stack; each section is decided from its
//! own vertices, so the order does not change the result.
//!
//! ```
//! use terrio::ops::simplify_dp::{SimplifyOptions, simplify_wkb};
//!
//! // LINESTRING (0 0, 5 0.05, 10 0) in little-endian WKB.
//! let mut wkb = vec![1, 2, 0, 0, 0, 3, 0, 0, 0];
//! for v in [0.0f64, 0.0, 5.0, 0.05, 10.0, 0.0] {
//!     wkb.extend_from_slice(&v.to_le_bytes());
//! }
//! let (out, stats) = simplify_wkb(&wkb, &SimplifyOptions::new(0.1))?;
//! assert_eq!((stats.vertices_in, stats.vertices_out), (3, 2));
//! assert_eq!(out.len(), 9 + 2 * 16);
//! # Ok::<(), terrio::Error>(())
//! ```

use super::simplify::dist;
use crate::{Error, Geometry, Result};
use std::str::FromStr;

type P = (f64, f64);

/// What happens to a polygon ring that Douglas-Peucker leaves with fewer than four points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RingCollapse {
    /// The ring keeps its input vertices, and nothing is removed: no polygon, hole or part
    /// disappears.
    #[default]
    Keep,
    /// As JTS: the hole is removed; a polygon whose shell collapses becomes empty; empty parts
    /// leave their multi geometry or collection.
    Drop,
}

impl FromStr for RingCollapse {
    type Err = Error;

    /// Parses `keep` or `drop`, ignoring case.
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "keep" => Ok(Self::Keep),
            "drop" => Ok(Self::Drop),
            _ => Err(Error::InvalidArgument(format!("unknown ring collapse policy '{s}' (keep, drop)"))),
        }
    }
}

/// Simplification options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimplifyOptions {
    /// The distance tolerance, in CRS units (non-negative and finite).
    pub tolerance: f64,
    /// Rings left with fewer than four points.
    pub collapse: RingCollapse,
}

impl SimplifyOptions {
    /// Douglas-Peucker with `tolerance`, keeping collapsed rings.
    pub fn new(tolerance: f64) -> Self {
        Self { tolerance, collapse: RingCollapse::default() }
    }

    /// Checks the tolerance.
    pub fn validate(&self) -> Result<()> {
        if !(self.tolerance >= 0.0 && self.tolerance.is_finite()) {
            return Err(Error::InvalidArgument("the simplification tolerance must be a non-negative number".into()));
        }
        Ok(())
    }
}

/// What a simplification did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimplifyStats {
    /// Features (null ones included).
    pub features: u64,
    /// Null features, passed through.
    pub nulls: u64,
    /// Vertices read (points and the closing points of rings included).
    pub vertices_in: u64,
    /// Vertices written.
    pub vertices_out: u64,
    /// Rings that collapsed and kept their input vertices ([`RingCollapse::Keep`]).
    pub rings_kept: u64,
    /// Rings removed ([`RingCollapse::Drop`]): collapsed and empty holes, and every ring of an
    /// emptied polygon.
    pub rings_dropped: u64,
    /// Polygons emptied by a collapsed shell ([`RingCollapse::Drop`]).
    pub polygons_emptied: u64,
    /// Features passed through unchanged because of a non-finite X or Y.
    pub non_finite: u64,
}

impl std::ops::AddAssign for SimplifyStats {
    fn add_assign(&mut self, o: Self) {
        self.features += o.features;
        self.nulls += o.nulls;
        self.vertices_in += o.vertices_in;
        self.vertices_out += o.vertices_out;
        self.rings_kept += o.rings_kept;
        self.rings_dropped += o.rings_dropped;
        self.polygons_emptied += o.polygons_emptied;
        self.non_finite += o.non_finite;
    }
}

/// Buffers reused from feature to feature.
#[derive(Default)]
struct Scratch {
    pts: Vec<P>,
    use_pt: Vec<bool>,
    keep: Vec<usize>,
    sections: Vec<(usize, usize)>,
}

/// JTS's `Distance.pointToSegment` from points to the segment `a`-`b`, with the segment's terms
/// computed once. The operations are those of [`crate::ops::simplify`]'s, in the same order, so
/// the distances are the same.
struct SegmentDistance {
    a: P,
    b: P,
    dx: f64,
    dy: f64,
    len2: f64,
    len: f64,
}

impl SegmentDistance {
    fn new(a: P, b: P) -> Self {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        Self { a, b, dx, dy, len2, len: len2.sqrt() }
    }

    fn to(&self, p: P) -> f64 {
        let (a, b) = (self.a, self.b);
        if a == b {
            return dist(p, a);
        }
        let r = ((p.0 - a.0) * self.dx + (p.1 - a.1) * self.dy) / self.len2;
        if r <= 0.0 {
            return dist(p, a);
        }
        if r >= 1.0 {
            return dist(p, b);
        }
        (((a.1 - p.1) * self.dx - (a.0 - p.0) * self.dy) / self.len2).abs() * self.len
    }
}

/// `DouglasPeuckerLineSimplifier.simplify` without the ring endpoint step: the indices of the
/// vertices kept, in order, into `s.keep`.
fn douglas_peucker(s: &mut Scratch, tolerance: f64) {
    let n = s.pts.len();
    s.use_pt.clear();
    s.use_pt.resize(n, true);
    s.sections.clear();
    if n > 0 {
        s.sections.push((0, n - 1));
    }
    // `simplifySection`, from a stack.
    while let Some((i, j)) = s.sections.pop() {
        if i + 1 >= j {
            continue;
        }
        let seg = SegmentDistance::new(s.pts[i], s.pts[j]);
        let (mut max_distance, mut max_index) = (-1.0, i);
        for (k, &p) in s.pts.iter().enumerate().take(j).skip(i + 1) {
            let distance = seg.to(p);
            if distance > max_distance {
                max_distance = distance;
                max_index = k;
            }
        }
        if max_distance <= tolerance {
            s.use_pt[i + 1..j].fill(false);
        } else {
            s.sections.push((max_index, j));
            s.sections.push((i, max_index));
        }
    }
    s.keep.clear();
    s.keep.extend((0..n).filter(|&k| s.use_pt[k]));
}

/// `simplifyRingEndpoint`, for a ring (`CoordinateArrays.isRing`: at least four points, closed):
/// drops the start of the kept points when it lies within the tolerance of its neighbours'
/// segment, and closes the ring on the next point (`CoordinateList.closeRing`, which adds no
/// point equal to the last).
fn simplify_ring_endpoint(s: &mut Scratch, tolerance: f64) {
    let (pts, keep) = (&s.pts, &mut s.keep);
    let n = pts.len();
    if n < 4 || pts[0] != pts[n - 1] {
        return;
    }
    // "Avoid collapsing triangles": JTS tests `size() < 4`, which counts the closing point, so
    // a triangle can still lose its start and collapse (as in GEOS 3.14 too).
    let m = keep.len();
    if m < 4 {
        return;
    }
    let distance = SegmentDistance::new(pts[keep[1]], pts[keep[m - 2]]).to(pts[keep[0]]);
    if distance <= tolerance {
        keep.pop();
        keep.remove(0);
        let (first, last) = (keep[0], keep[keep.len() - 1]);
        if pts[first] != pts[last] {
            keep.push(first);
        }
    }
}

fn wkb_error(pos: usize, msg: &str) -> Error {
    Error::InvalidData(format!("WKB at byte {pos}: {msg}"))
}

/// A (sub)geometry header: byte order, base type (1 to 7) and coordinate dimensions, and the
/// span of its bytes (order, type and SRID), copied to the output as they are.
struct Header {
    le: bool,
    kind: u32,
    dims: usize,
    start: usize,
    end: usize,
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| wkb_error(self.pos, "truncated"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self, le: bool) -> Result<u32> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    /// A count of items of at least `size` bytes each, checked against the bytes left.
    fn count(&mut self, le: bool, size: usize) -> Result<usize> {
        let at = self.pos;
        let n = self.u32(le)? as usize;
        if n.checked_mul(size).is_none_or(|bytes| bytes > self.b.len() - self.pos) {
            return Err(wkb_error(at, "count larger than the data"));
        }
        Ok(n)
    }

    fn header(&mut self) -> Result<Header> {
        let start = self.pos;
        let le = match self.take(1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(wkb_error(start, "invalid byte order")),
        };
        let t = self.u32(le)?;
        let (mut z, mut m) = (t & 0x8000_0000 != 0, t & 0x4000_0000 != 0);
        if t & 0x2000_0000 != 0 {
            self.take(4)?;
        }
        let base = t & 0x0FFF_FFFF;
        match base / 1000 {
            0 => {}
            1 => z = true,
            2 => m = true,
            3 => (z, m) = (true, true),
            _ => return Err(wkb_error(start, &format!("unsupported geometry type {t}"))),
        }
        let kind = base % 1000;
        if !(1..=7).contains(&kind) {
            return Err(wkb_error(start, &format!("unsupported geometry type {t}")));
        }
        Ok(Header { le, kind, dims: 2 + usize::from(z) + usize::from(m), start, end: self.pos })
    }
}

fn put_u32(out: &mut Vec<u8>, v: u32, le: bool) {
    out.extend_from_slice(&if le { v.to_le_bytes() } else { v.to_be_bytes() });
}

fn patch_u32(out: &mut [u8], at: usize, v: u32, le: bool) {
    out[at..at + 4].copy_from_slice(&if le { v.to_le_bytes() } else { v.to_be_bytes() });
}

fn f64_at(b: &[u8], le: bool) -> f64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    if le { f64::from_le_bytes(a) } else { f64::from_be_bytes(a) }
}

/// What became of a (sub)geometry.
#[derive(PartialEq)]
enum Outcome {
    /// Written, with coordinates.
    Kept,
    /// Written, empty (an empty input, or a polygon emptied by its shell).
    Empty,
}

/// Douglas-Peucker on one WKB feature (`DPTransformer`).
struct Run<'s> {
    tolerance: f64,
    collapse: RingCollapse,
    s: &'s mut Scratch,
    stats: SimplifyStats,
    non_finite: bool,
}

impl Run<'_> {
    /// Reads `n` coordinates into the scratch points.
    fn points(&mut self, coords: &[u8], n: usize, stride: usize, le: bool) {
        self.s.pts.clear();
        for c in coords.chunks_exact(stride).take(n) {
            let (x, y) = (f64_at(c, le), f64_at(&c[8..], le));
            if !(x.is_finite() && y.is_finite()) {
                self.non_finite = true;
            }
            self.s.pts.push((x, y));
        }
        self.stats.vertices_in += n as u64;
    }

    fn write_kept(&mut self, out: &mut Vec<u8>, coords: &[u8], stride: usize, le: bool) -> Result<()> {
        let n = u32::try_from(self.s.keep.len()).map_err(|_| Error::InvalidData("too many vertices".into()))?;
        put_u32(out, n, le);
        for &k in &self.s.keep {
            out.extend_from_slice(&coords[k * stride..(k + 1) * stride]);
        }
        self.stats.vertices_out += self.s.keep.len() as u64;
        Ok(())
    }

    /// Simplifies a polygon ring into `keep` (`transformLinearRing`); false when it is to be
    /// removed.
    fn ring(&mut self, n: usize) -> bool {
        if n == 0 {
            self.s.keep.clear();
            return self.collapse == RingCollapse::Keep;
        }
        let tolerance = self.tolerance;
        douglas_peucker(self.s, tolerance);
        simplify_ring_endpoint(self.s, tolerance);
        if self.s.keep.len() >= 4 {
            return true;
        }
        match self.collapse {
            RingCollapse::Keep => {
                self.s.keep.clear();
                self.s.keep.extend(0..n);
                if n >= 4 {
                    self.stats.rings_kept += 1;
                }
                true
            }
            RingCollapse::Drop => false,
        }
    }

    /// Copies a geometry to `out`, simplified.
    fn geometry(&mut self, r: &mut Reader, out: &mut Vec<u8>) -> Result<Outcome> {
        let h = r.header()?;
        out.extend_from_slice(&r.b[h.start..h.end]);
        let stride = 8 * h.dims;
        match h.kind {
            1 => {
                let c = r.take(stride)?;
                out.extend_from_slice(c);
                self.stats.vertices_in += 1;
                self.stats.vertices_out += 1;
                Ok(Outcome::Kept)
            }
            2 => {
                let n = r.count(h.le, stride)?;
                let coords = r.take(n * stride)?;
                self.points(coords, n, stride, h.le);
                // Lines keep their endpoints (`isPreserveEndpoint`), closed ones included.
                douglas_peucker(self.s, self.tolerance);
                self.write_kept(out, coords, stride, h.le)?;
                Ok(if n == 0 { Outcome::Empty } else { Outcome::Kept })
            }
            3 => {
                let rings = r.count(h.le, 4)?;
                let count_at = out.len();
                put_u32(out, 0, h.le);
                let mut written = 0u32;
                for i in 0..rings {
                    let n = r.count(h.le, stride)?;
                    let coords = r.take(n * stride)?;
                    self.points(coords, n, stride, h.le);
                    if self.ring(n) {
                        self.write_kept(out, coords, stride, h.le)?;
                        written += 1;
                    } else if i > 0 {
                        self.stats.rings_dropped += 1;
                    } else {
                        // The shell is empty or collapsed: the polygon is empty. Read the holes
                        // past.
                        for _ in 1..rings {
                            let n = r.count(h.le, stride)?;
                            let coords = r.take(n * stride)?;
                            self.points(coords, n, stride, h.le);
                        }
                        out.truncate(count_at);
                        put_u32(out, 0, h.le);
                        self.stats.rings_dropped += rings as u64;
                        self.stats.polygons_emptied += u64::from(n > 0);
                        return Ok(Outcome::Empty);
                    }
                }
                patch_u32(out, count_at, written, h.le);
                Ok(if rings == 0 { Outcome::Empty } else { Outcome::Kept })
            }
            _ => {
                // Multi geometries and collections: each part is a WKB geometry. As JTS, empty
                // parts are removed when collapses are dropped.
                let parts = r.count(h.le, 9)?;
                let count_at = out.len();
                put_u32(out, 0, h.le);
                let mut written = 0u32;
                for _ in 0..parts {
                    let at = out.len();
                    if self.geometry(r, out)? == Outcome::Empty && self.collapse == RingCollapse::Drop {
                        out.truncate(at);
                    } else {
                        written += 1;
                    }
                }
                patch_u32(out, count_at, written, h.le);
                Ok(if written == 0 { Outcome::Empty } else { Outcome::Kept })
            }
        }
    }
}

fn simplify_one(wkb: &[u8], options: &SimplifyOptions, s: &mut Scratch) -> Result<(Vec<u8>, SimplifyStats)> {
    let mut run = Run { tolerance: options.tolerance, collapse: options.collapse, s, stats: SimplifyStats::default(), non_finite: false };
    let mut r = Reader { b: wkb, pos: 0 };
    let mut out = Vec::with_capacity(wkb.len());
    let outcome = run.geometry(&mut r, &mut out)?;
    if r.pos != wkb.len() {
        return Err(wkb_error(r.pos, "trailing bytes after the geometry"));
    }
    let n = run.stats.vertices_in;
    if run.non_finite {
        return Ok((wkb.to_vec(), SimplifyStats { features: 1, vertices_in: n, vertices_out: n, non_finite: 1, ..Default::default() }));
    }
    // An empty input is returned as it is (`getResultGeometry`).
    if outcome == Outcome::Empty && n == 0 {
        return Ok((wkb.to_vec(), SimplifyStats { features: 1, ..Default::default() }));
    }
    let mut stats = run.stats;
    stats.features = 1;
    Ok((out, stats))
}

/// Simplifies one WKB geometry (see the module documentation).
///
/// # Errors
/// An invalid tolerance, or WKB that is malformed, has trailing bytes or holds a type other
/// than the seven simple feature types.
pub fn simplify_wkb(wkb: &[u8], options: &SimplifyOptions) -> Result<(Vec<u8>, SimplifyStats)> {
    options.validate()?;
    simplify_one(wkb, options, &mut Scratch::default())
}

/// Simplifies WKB geometries in parallel, keeping their order; `None` stays `None`.
///
/// ```
/// use terrio::ops::simplify_dp::{SimplifyOptions, simplify_wkb_many};
///
/// let point = [1u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
/// let (out, stats) = simplify_wkb_many(&[Some(&point[..]), None], &SimplifyOptions::new(1.0))?;
/// assert_eq!(out, vec![Some(point.to_vec()), None]);
/// assert_eq!((stats.features, stats.nulls), (2, 1));
/// # Ok::<(), terrio::Error>(())
/// ```
///
/// # Errors
/// As [`simplify_wkb`], naming the first failing feature's index.
pub fn simplify_wkb_many<B: AsRef<[u8]> + Sync>(
    geometries: &[Option<B>],
    options: &SimplifyOptions,
) -> Result<(Vec<Option<Vec<u8>>>, SimplifyStats)> {
    options.validate()?;
    let results = crate::par::map_init(geometries, Scratch::default, |s, g| match g {
        None => Ok((None, SimplifyStats { features: 1, nulls: 1, ..Default::default() })),
        Some(b) => simplify_one(b.as_ref(), options, s).map(|(w, st)| (Some(w), st)),
    });
    let mut stats = SimplifyStats::default();
    let mut out = Vec::with_capacity(results.len());
    for (i, r) in results.into_iter().enumerate() {
        let (w, st) = r.map_err(|e| Error::InvalidData(format!("feature {i}: {e}")))?;
        stats += st;
        out.push(w);
    }
    Ok((out, stats))
}

/// Douglas-Peucker simplification of a geometry (see the module documentation), through WKB.
/// Z is kept; M is dropped, as terrio geometries hold no M.
pub fn simplify_douglas_peucker(g: &Geometry, tolerance: f64, collapse: RingCollapse) -> Result<Geometry> {
    let options = SimplifyOptions { tolerance, collapse };
    let (wkb, _) = simplify_wkb(&g.to_wkb(), &options)?;
    Geometry::from_wkb(&wkb).map_err(|e| Error::InvalidData(format!("WKB: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le_line(kind: u32, pts: &[(f64, f64)]) -> Vec<u8> {
        let mut b = vec![1];
        b.extend_from_slice(&kind.to_le_bytes());
        b.extend_from_slice(&(pts.len() as u32).to_le_bytes());
        for p in pts {
            b.extend_from_slice(&p.0.to_le_bytes());
            b.extend_from_slice(&p.1.to_le_bytes());
        }
        b
    }

    fn polygon(rings: &[&[(f64, f64)]], le: bool) -> Vec<u8> {
        let u = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let f = |v: f64| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut b = vec![u8::from(le)];
        b.extend_from_slice(&u(3));
        b.extend_from_slice(&u(rings.len() as u32));
        for r in rings {
            b.extend_from_slice(&u(r.len() as u32));
            for p in *r {
                b.extend_from_slice(&f(p.0));
                b.extend_from_slice(&f(p.1));
            }
        }
        b
    }

    fn coords(g: &Geometry) -> Vec<(f64, f64)> {
        g.all_coords().iter().map(|c| (c.x, c.y)).collect()
    }

    fn run(wkb: &[u8], tolerance: f64, collapse: RingCollapse) -> (Vec<u8>, SimplifyStats) {
        simplify_wkb(wkb, &SimplifyOptions { tolerance, collapse }).unwrap()
    }

    /// A direct transcription of `DouglasPeuckerLineSimplifier`, recursion and all, with the
    /// distance computed per point: the reference for the stack and the hoisted segment terms.
    fn reference(pts: &[P], tolerance: f64, ring: bool) -> Vec<P> {
        fn section(pts: &[P], i: usize, j: usize, tol: f64, use_pt: &mut [bool]) {
            if i + 1 == j {
                return;
            }
            let (mut max, mut idx) = (-1.0, i);
            for k in i + 1..j {
                let d = super::super::simplify::distance_to_segment(pts[k], pts[i], pts[j]);
                if d > max {
                    max = d;
                    idx = k;
                }
            }
            if max <= tol {
                use_pt[i + 1..j].fill(false);
            } else {
                section(pts, i, idx, tol, use_pt);
                section(pts, idx, j, tol, use_pt);
            }
        }
        let mut use_pt = vec![true; pts.len()];
        if !pts.is_empty() {
            section(pts, 0, pts.len() - 1, tolerance, &mut use_pt);
        }
        let mut list: Vec<P> = pts.iter().zip(&use_pt).filter(|(_, u)| **u).map(|(p, _)| *p).collect();
        if ring && pts.len() >= 4 && pts[0] == pts[pts.len() - 1] && list.len() >= 4 {
            let d = super::super::simplify::distance_to_segment(list[0], list[1], list[list.len() - 2]);
            if d <= tolerance {
                list.remove(0);
                list.pop();
                if list.last() != list.first() {
                    list.push(list[0]);
                }
            }
        }
        list
    }

    fn noisy_ring(n: usize, seed: u64) -> Vec<P> {
        let mut x = seed;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % 10_000) as f64 / 10_000.0
        };
        let mut pts: Vec<P> = (0..n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                let r = 100.0 + 3.0 * next();
                (r * a.cos(), r * a.sin())
            })
            .collect();
        pts.push(pts[0]);
        pts
    }

    #[test]
    fn sections_from_a_stack_match_the_recursive_line_simplifier() {
        for seed in 1..40u64 {
            let ring = noisy_ring(50 + seed as usize * 7, seed);
            for tol in [0.0, 0.5, 1.0, 2.5, 10.0, 150.0] {
                let mut s = Scratch { pts: ring.clone(), ..Default::default() };
                douglas_peucker(&mut s, tol);
                simplify_ring_endpoint(&mut s, tol);
                let got: Vec<P> = s.keep.iter().map(|&k| ring[k]).collect();
                assert_eq!(got, reference(&ring, tol, true), "seed {seed}, tolerance {tol}");
                let mut s = Scratch { pts: ring.clone(), ..Default::default() };
                douglas_peucker(&mut s, tol);
                let got: Vec<P> = s.keep.iter().map(|&k| ring[k]).collect();
                assert_eq!(got, reference(&ring, tol, false), "line, seed {seed}, tolerance {tol}");
            }
        }
    }

    #[test]
    fn hoisted_segment_terms_give_the_same_distances() {
        let ring = noisy_ring(500, 7);
        for w in ring.windows(3) {
            let seg = SegmentDistance::new(w[0], w[2]);
            for &p in &ring {
                assert_eq!(seg.to(p).to_bits(), super::super::simplify::distance_to_segment(p, w[0], w[2]).to_bits());
            }
        }
        let seg = SegmentDistance::new((1.0, 1.0), (1.0, 1.0));
        assert_eq!(seg.to((4.0, 5.0)), 5.0);
    }

    #[test]
    fn a_line_keeps_its_endpoints_and_the_tolerance_is_inclusive() {
        let wkb = le_line(2, &[(0.0, 0.0), (5.0, 1.0), (10.0, 0.0)]);
        let g = Geometry::from_wkb(&run(&wkb, 1.0, RingCollapse::Keep).0).unwrap();
        assert_eq!(coords(&g), [(0.0, 0.0), (10.0, 0.0)]);
        let g = Geometry::from_wkb(&run(&wkb, 0.999, RingCollapse::Keep).0).unwrap();
        assert_eq!(coords(&g).len(), 3);
        // A closed line keeps its start, unlike a ring.
        let closed = le_line(2, &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.01), (0.0, 0.0)]);
        let g = Geometry::from_wkb(&run(&closed, 0.1, RingCollapse::Keep).0).unwrap();
        assert_eq!(coords(&g).first(), Some(&(0.0, 0.0)));
    }

    #[test]
    fn a_ring_loses_its_start_point_within_the_tolerance() {
        // The start (0, 5) lies on the segment from (0, 10) to (0, 0)'s neighbours.
        let ring = [(0.0, 5.0), (0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 5.0)];
        let (out, stats) = run(&polygon(&[&ring], true), 0.1, RingCollapse::Keep);
        let g = Geometry::from_wkb(&out).unwrap();
        let c = coords(&g);
        assert!(!c.contains(&(0.0, 5.0)), "{c:?}");
        assert_eq!((stats.vertices_in, stats.vertices_out), (6, 5));
    }

    #[test]
    fn a_thin_triangle_collapses_as_in_jts() {
        // The start lies 0.3 from the segment joining its neighbours, within the tolerance: it is
        // dropped, leaving three points, and the ring has collapsed.
        let ring = [(5.0, 0.3), (0.0, 0.0), (10.0, 0.0), (5.0, 0.3)];
        let mut s = Scratch { pts: ring.to_vec(), ..Default::default() };
        douglas_peucker(&mut s, 0.5);
        assert_eq!(s.keep, [0, 1, 2, 3]);
        simplify_ring_endpoint(&mut s, 0.5);
        assert_eq!(s.keep, [1, 2, 1]);
    }

    #[test]
    fn collapsed_rings_are_kept_or_dropped() {
        let shell: &[P] = &[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0), (0.0, 0.0)];
        let sliver: &[P] = &[(10.0, 10.0), (20.0, 10.1), (30.0, 10.0), (20.0, 9.9), (10.0, 10.0)];
        let wkb = polygon(&[shell, sliver], false);
        let (kept, s) = run(&wkb, 1.0, RingCollapse::Keep);
        assert_eq!(s.rings_kept, 1);
        assert_eq!(Geometry::from_wkb(&kept).unwrap(), Geometry::from_wkb(&wkb).unwrap());
        let (dropped, s) = run(&wkb, 1.0, RingCollapse::Drop);
        assert_eq!((s.rings_dropped, s.polygons_emptied), (1, 0));
        match Geometry::from_wkb(&dropped).unwrap() {
            Geometry::Polygon { interiors, .. } => assert!(interiors.is_empty()),
            g => panic!("{g:?}"),
        }
        // A collapsed shell empties its polygon, which leaves its multipolygon.
        let mut multi = vec![1u8, 6, 0, 0, 0, 2, 0, 0, 0];
        multi.extend(polygon(&[sliver], true));
        multi.extend(polygon(&[shell], true));
        let (out, s) = run(&multi, 1.0, RingCollapse::Drop);
        assert_eq!(s.polygons_emptied, 1);
        match Geometry::from_wkb(&out).unwrap() {
            Geometry::MultiPolygon(ps) => assert_eq!(ps.len(), 1),
            g => panic!("{g:?}"),
        }
        let (out, _) = run(&polygon(&[sliver], true), 1.0, RingCollapse::Drop);
        assert_eq!(out, [1, 3, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn byte_order_type_codes_srid_z_and_m_are_kept() {
        // A big-endian EWKB LineString ZM with an SRID.
        let mut b = vec![0u8];
        b.extend_from_slice(&(2u32 | 0x8000_0000 | 0x4000_0000 | 0x2000_0000).to_be_bytes());
        b.extend_from_slice(&4326u32.to_be_bytes());
        b.extend_from_slice(&3u32.to_be_bytes());
        for p in [[0.0f64, 0.0, 1.0, 2.0], [5.0, 0.01, 3.0, 4.0], [10.0, 0.0, 5.0, 6.0]] {
            p.iter().for_each(|v| b.extend_from_slice(&v.to_be_bytes()));
        }
        let (out, _) = run(&b, 1.0, RingCollapse::Keep);
        assert_eq!(&out[..13], &b[..9].iter().chain(&2u32.to_be_bytes()).copied().collect::<Vec<_>>()[..]);
        assert_eq!(&out[13..45], &b[13..45]);
        assert_eq!(&out[45..], &b[77..]);
        // ISO LineString Z.
        let mut iso = vec![1u8];
        iso.extend_from_slice(&1002u32.to_le_bytes());
        iso.extend_from_slice(&3u32.to_le_bytes());
        for p in [[0.0f64, 0.0, 7.0], [5.0, 0.01, 8.0], [10.0, 0.0, 9.0]] {
            p.iter().for_each(|v| iso.extend_from_slice(&v.to_le_bytes()));
        }
        let (out, s) = run(&iso, 1.0, RingCollapse::Keep);
        assert_eq!((s.vertices_in, s.vertices_out), (3, 2));
        assert_eq!(&out[9..33], &iso[9..33]);
        assert_eq!(&out[33..], &iso[57..]);
    }

    #[test]
    fn points_empties_and_non_finite_features_pass_through() {
        let point = le_line(1, &[]).into_iter().take(5).chain([0u8; 16]).collect::<Vec<_>>();
        assert_eq!(run(&point, 1.0, RingCollapse::Drop).0, point);
        let empty = le_line(2, &[]);
        assert_eq!(run(&empty, 1.0, RingCollapse::Drop).0, empty);
        let nan = le_line(2, &[(0.0, 0.0), (5.0, f64::NAN), (10.0, 0.0)]);
        let (out, s) = run(&nan, 100.0, RingCollapse::Keep);
        assert_eq!((out, s.non_finite), (nan, 1));
    }

    #[test]
    fn malformed_wkb_is_refused() {
        let opts = SimplifyOptions::new(1.0);
        let line = le_line(2, &[(0.0, 0.0), (1.0, 1.0)]);
        assert!(simplify_wkb(&line[..line.len() - 1], &opts).is_err());
        let mut trailing = line.clone();
        trailing.push(0);
        assert!(simplify_wkb(&trailing, &opts).is_err());
        let mut huge = line.clone();
        huge[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(simplify_wkb(&huge, &opts).is_err());
        let mut curve = line.clone();
        curve[1..5].copy_from_slice(&8u32.to_le_bytes());
        assert!(simplify_wkb(&curve, &opts).is_err());
        assert!(simplify_wkb(&line, &SimplifyOptions::new(-1.0)).is_err());
        assert!(simplify_wkb(&line, &SimplifyOptions::new(f64::NAN)).is_err());
        let err = simplify_wkb_many(&[Some(line.clone()), Some(trailing)], &opts).unwrap_err();
        assert!(err.to_string().contains("feature 1"), "{err}");
    }

    #[test]
    fn options_parse() {
        assert_eq!("DROP".parse::<RingCollapse>().unwrap(), RingCollapse::Drop);
        assert!("maybe".parse::<RingCollapse>().is_err());
    }

    #[test]
    fn a_ring_of_a_million_vertices_does_not_recurse() {
        let ring = noisy_ring(1_000_000, 3);
        let mut s = Scratch { pts: ring, ..Default::default() };
        douglas_peucker(&mut s, 0.5);
        assert!(s.keep.len() > 4 && s.keep.len() < 1_000_001);
    }
}
