//! Constrained polygon triangulation from JTS 1.20.0 under EDL-1.0.
//! Sources: PolygonHoleJoiner, PolygonNoder, PolygonEarClipper,
//! TriDelaunayImprover, Tri, TriangulationBuilder, PolygonNodeTopology,
//! Orientation, Angle and TrianglePredicate. Copyright (c) 2016 Vivid
//! Solutions; Copyright (c) 2016, 2021 Martin Davis. See NOTICE.
//!
//! Arena indices retain JTS's triangle construction and flip order.
//! R-trees index vertices and prune join-visibility boundary candidates.
//! Active joined vertices retain JTS lexicographic candidate order.
//! Every topology walk is bounded.
use super::simplify::{conflicts, orient};
use crate::{Error, Result};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use std::collections::{BTreeSet, HashMap};
use wbvector::Coord;

type P = (f64, f64);
fn xy(c: &Coord) -> P {
    (c.x, c.y)
}
fn failure() -> Error {
    Error::InvalidArgument("cannot triangulate polygon boundary".into())
}
fn compare(a: P, b: P) -> std::cmp::Ordering {
    a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
}
fn envelope(a: P, b: P) -> AABB<[f64; 2]> {
    AABB::from_corners([a.0.min(b.0), a.1.min(b.1)], [a.0.max(b.0), a.1.max(b.1)])
}

/// JTS Orientation.isCCW, using the upper cap of the ring.
fn ccw(r: &[usize], pts: &[P]) -> bool {
    let n = r.len();
    if n < 3 {
        return false;
    }
    let mut high = pts[r[0]];
    let mut low = high;
    let mut hi = None;
    let mut previous = high;
    for i in 1..=n {
        let p = pts[r[i % n]];
        if p.1 > previous.1 && p.1 >= high.1 {
            high = p;
            low = previous;
            hi = Some(i % n);
        }
        previous = p;
    }
    let Some(hi) = hi else {
        return false;
    };
    let mut down = hi;
    for _ in 0..n {
        down = (down + 1) % n;
        if pts[r[down]].1 != high.1 {
            break;
        }
    }
    let down_low = pts[r[down]];
    let down_high = pts[r[(down + n - 1) % n]];
    if high == down_high {
        low != high && down_low != high && low != down_low && orient(low, high, down_low) > 0
    } else {
        down_high.0 - high.0 < 0.0
    }
}
fn normalize(mut r: Vec<usize>, pts: &[P], clockwise: bool) -> Vec<usize> {
    if r.first() == r.last() {
        r.pop();
    }
    if ccw(&r, pts) == clockwise && r.len() > 1 {
        r[1..].reverse();
    }
    r
}
fn quadrant(o: P, p: P) -> u8 {
    match (p.0 >= o.0, p.1 >= o.1) {
        (true, true) => 0,
        (false, true) => 1,
        (false, false) => 2,
        (true, false) => 3,
    }
}
fn greater(o: P, p: P, q: P) -> bool {
    let a = quadrant(o, p);
    let b = quadrant(o, q);
    a > b || (a == b && orient(o, q, p) > 0)
}
fn interior(o: P, a: P, b: P, p: P) -> bool {
    let swapped = greater(o, a, b);
    let (lo, hi) = if swapped { (b, a) } else { (a, b) };
    let between = greater(o, p, lo) && !greater(o, p, hi);
    between != swapped
}
fn nodes(r: &[usize], pts: &[P], index: &RTree<GeomWithData<[f64; 2], usize>>) -> Vec<usize> {
    let mut result = Vec::new();
    for i in 0..r.len() {
        let (a, b) = (r[i], r[(i + 1) % r.len()]);
        if a == b {
            continue;
        }
        result.push(a);
        let (p, q) = (pts[a], pts[b]);
        let mut inside = index
            .locate_in_envelope_intersecting(envelope(p, q))
            .filter_map(|v| {
                let id = v.data;
                (id != a && id != b && orient(p, q, pts[id]) == 0).then_some(id)
            })
            .collect::<Vec<_>>();
        inside.sort_by(|&a, &b| {
            let order = compare(pts[a], pts[b]);
            if compare(p, q).is_gt() { order.reverse() } else { order }
        });
        result.extend(inside);
    }
    result
}
fn joined(shell: Vec<usize>, mut holes: Vec<Vec<usize>>, pts: &[P]) -> Result<Vec<usize>> {
    if holes.is_empty() {
        return Ok(shell);
    }
    let bounds = |r: &Vec<usize>| {
        r.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |e, &i| {
            let p = pts[i];
            (e.0.min(p.0), e.1.min(p.1), e.2.max(p.0), e.3.max(p.1))
        })
    };
    holes.sort_by(|a, b| {
        let a = bounds(a);
        let b = bounds(b);
        a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.total_cmp(&b.2)).then(a.3.total_cmp(&b.3))
    });
    let boundaries = std::iter::once(&shell)
        .chain(&holes)
        .flat_map(|r| (0..r.len()).map(|i| (pts[r[i]], pts[r[(i + 1) % r.len()]])))
        .collect::<Vec<_>>();
    let boundary_index = RTree::bulk_load(
        boundaries
            .iter()
            .enumerate()
            .map(|(id, &(a, b))| {
                let e = envelope(a, b);
                GeomWithData::new(Rectangle::from_corners(e.lower(), e.upper()), id)
            })
            .collect(),
    );
    // Sort only this polygon's vertices once, not the triangulator's global
    // coordinate arena. The active ranks grow with the joined ring, so future
    // holes cannot become candidates before they have been spliced in.
    let mut ordered = std::iter::once(&shell).chain(&holes).flatten().copied().collect::<Vec<_>>();
    ordered.sort_by(|&a, &b| compare(pts[a], pts[b]));
    ordered.dedup();
    let rank = ordered.iter().enumerate().map(|(rank, &id)| (id, rank)).collect::<HashMap<_, _>>();
    let mut joined_points = shell.iter().map(|id| rank[id]).collect::<BTreeSet<_>>();
    let mut ring = shell;
    for hole in holes {
        if hole.len() < 3 {
            return Err(failure());
        }
        let touching = hole.iter().position(|id| joined_points.contains(&rank[id]));
        let (h, join_point, line_point) = if let Some(h) = touching {
            (h, hole[h], hole[(h + hole.len() - 1) % hole.len()])
        } else {
            let h = (0..hole.len()).min_by(|&a, &b| compare(pts[hole[a]], pts[hole[b]])).ok_or_else(failure)?;
            let p = pts[hole[h]];
            let end = ordered.partition_point(|&id| pts[id].0 <= p.0);
            let join = joined_points
                .range(..end)
                .rev()
                .map(|&r| ordered[r])
                .find(|&id| {
                    !boundary_index.locate_in_envelope_intersecting(envelope(p, pts[id])).any(|entry| {
                        let (a, b) = boundaries[entry.data];
                        conflicts(p, pts[id], a, b)
                    })
                })
                .ok_or_else(failure)?;
            (h, join, hole[h])
        };
        let n = ring.len();
        let join = (0..n)
            .find(|&i| ring[i] == join_point && interior(pts[ring[i]], pts[ring[(i + n - 1) % n]], pts[ring[(i + 1) % n]], pts[line_point]))
            .ok_or_else(failure)?;
        let mut section = Vec::new();
        if touching.is_none() {
            section.push(hole[h]);
        }
        for offset in 1..=hole.len() {
            section.push(hole[(h + offset) % hole.len()]);
        }
        if touching.is_none() {
            section.push(join_point);
        }
        joined_points.extend(hole.iter().map(|id| rank[id]));
        ring.splice(join + 1..join + 1, section);
    }
    Ok(ring)
}
fn angle(a: P, o: P, b: P) -> f64 {
    let d = (b.1 - o.1).atan2(b.0 - o.0) - (a.1 - o.1).atan2(a.0 - o.0);
    if d <= -std::f64::consts::PI {
        d + std::f64::consts::TAU
    } else if d > std::f64::consts::PI {
        d - std::f64::consts::TAU
    } else {
        d
    }
}
fn ears(r: &[usize], pts: &[P]) -> Result<Vec<[usize; 3]>> {
    let n = r.len();
    if n < 3 {
        return Err(failure());
    }
    let index = RTree::bulk_load(r.iter().enumerate().map(|(i, &v)| GeomWithData::new([pts[v].0, pts[v].1], i)).collect());
    let mut next = (0..n).map(|i| Some((i + 1) % n)).collect::<Vec<_>>();
    let (mut count, mut first, mut a, mut scanned) = (n, 0, 0, 0usize);
    let mut faces = Vec::new();
    while count >= 3 {
        let b = next[a].ok_or_else(failure)?;
        let c = next[b].ok_or_else(failure)?;
        let (p, q, s) = (pts[r[a]], pts[r[b]], pts[r[c]]);
        let convex = orient(p, q, s) < 0;
        let invalid = p == q || q == s || p == s;
        let mut ear = convex;
        let mut duplicate = false;
        if convex {
            let env = AABB::from_corners([p.0.min(q.0).min(s.0), p.1.min(q.1).min(s.1)], [p.0.max(q.0).max(s.0), p.1.max(q.1).max(s.1)]);
            for v in index.locate_in_envelope_intersecting(env) {
                let i = v.data;
                if i == b || next[i].is_none() {
                    continue;
                }
                let t = pts[r[i]];
                if t == q {
                    duplicate = true;
                } else if t != p && t != s && orient(p, q, t) <= 0 && orient(q, s, t) <= 0 && orient(s, p, t) <= 0 {
                    ear = false;
                    break;
                }
            }
            if ear && duplicate {
                let corner_angle = angle(p, q, s);
                let mut previous = first;
                let mut current = next[first].ok_or_else(failure)?;
                for _ in 0..count {
                    if current != b && pts[r[current]] == q {
                        let after = next[current].ok_or_else(failure)?;
                        let out = angle(p, q, pts[r[after]]);
                        let incoming = angle(p, q, pts[r[previous]]);
                        if (out > 0.0 && out < corner_angle)
                            || (incoming > 0.0 && incoming < corner_angle)
                            || (out == 0.0 && incoming == corner_angle)
                        {
                            ear = false;
                            break;
                        }
                    }
                    previous = current;
                    current = next[current].ok_or_else(failure)?;
                }
            }
        }
        if ear {
            faces.push([r[a], r[b], r[c]]);
            scanned = 0;
        } else {
            scanned += 1;
        }
        if ear || invalid {
            if first == b {
                first = next[b].ok_or_else(failure)?;
            }
            next[a] = next[b];
            next[b] = None;
            count -= 1;
        }
        if count < 3 {
            break;
        }
        if scanned > 2 * count {
            return Err(failure());
        }
        a = next[a].ok_or_else(failure)?;
    }
    Ok(faces)
}
fn adjacency(faces: &[[usize; 3]]) -> Vec<[Option<usize>; 3]> {
    let mut result = vec![[None; 3]; faces.len()];
    let mut edges: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
    for (t, v) in faces.iter().enumerate() {
        for i in 0..3 {
            let (a, b) = (v[i], v[(i + 1) % 3]);
            if let Some((s, j)) = edges.remove(&(b, a)) {
                result[t][i] = Some(s);
                result[s][j] = Some(t);
            } else {
                edges.insert((a, b), (t, i));
            }
        }
    }
    result
}
fn in_circle(a: P, b: P, c: P, p: P) -> bool {
    // Preserve JTS 1.20 TrianglePredicate.isInCircleNormalized arithmetic.
    // Rescaling changes observed GEOS hulls at extreme magnitudes; this
    // compatibility predicate is not scale-independent (see provenance docs).
    let (ax, ay, bx, by, cx, cy) = (a.0 - p.0, a.1 - p.1, b.0 - p.0, b.1 - p.1, c.0 - p.0, c.1 - p.1);
    let ab = ax * by - bx * ay;
    let bc = bx * cy - cx * by;
    let ca = cx * ay - ax * cy;
    (ax * ax + ay * ay) * bc + (bx * bx + by * by) * ca + (cx * cx + cy * cy) * ab > 0.0
}
fn improve(faces: &mut [[usize; 3]], pts: &[P]) {
    let mut adj = adjacency(faces);
    // JTS 1.20.0 TriDelaunayImprover uses 200 passes and scans size - 1
    // source triangles, including the final triangle only as a neighbor.
    // Preserve that order: concave erosion depends on triangle construction.
    // As in JTS, exhausting the cap retains the last mesh; full convergence
    // is not guaranteed for every floating-point input.
    for _ in 0..200 {
        let mut changed = false;
        for t in 0..faces.len().saturating_sub(1) {
            for i in 0..3 {
                let Some(s) = adj[t][i] else {
                    continue;
                };
                let Some(j) = adj[s].iter().position(|a| *a == Some(t)) else {
                    continue;
                };
                let (a, b, c, d) = (faces[t][i], faces[t][(i + 1) % 3], faces[t][(i + 2) % 3], faces[s][(j + 2) % 3]);
                if orient(pts[c], pts[a], pts[d]) != orient(pts[d], pts[b], pts[c]) {
                    continue;
                }
                if in_circle(pts[a], pts[c], pts[b], pts[d]) || in_circle(pts[b], pts[d], pts[a], pts[c]) {
                    // JTS Tri.flip: retain triangle IDs and update the four
                    // external neighbors, rather than rebuilding the mesh.
                    let ca = adj[t][(i + 2) % 3];
                    let bc = adj[t][(i + 1) % 3];
                    let ad = adj[s][(j + 1) % 3];
                    let db = adj[s][(j + 2) % 3];
                    faces[t] = [d, c, a];
                    faces[s] = [c, d, b];
                    adj[t] = [Some(s), ca, ad];
                    adj[s] = [Some(t), db, bc];
                    if let Some(n) = ad {
                        for neighbor in &mut adj[n] {
                            if *neighbor == Some(s) {
                                *neighbor = Some(t);
                            }
                        }
                    }
                    if let Some(n) = bc {
                        for neighbor in &mut adj[n] {
                            if *neighbor == Some(t) {
                                *neighbor = Some(s);
                            }
                        }
                    }
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}
/// Shared immutable sites for all polygons in one hull computation.
pub(crate) struct Triangulator {
    pts: Vec<P>,
    sites: RTree<GeomWithData<[f64; 2], usize>>,
}

impl Triangulator {
    pub(crate) fn new(coords: &[Coord]) -> Self {
        let pts = coords.iter().map(xy).collect::<Vec<_>>();
        let sites = RTree::bulk_load(pts.iter().enumerate().map(|(i, p)| GeomWithData::new([p.0, p.1], i)).collect());
        Self { pts, sites }
    }

    /// Preserves JTS's ear creation and flip order. Ring IDs refer to the
    /// constructor's coordinates; rings may be open or closed. Empty holes are ignored.
    pub(crate) fn triangulate(&self, shell: Vec<usize>, holes: Vec<Vec<usize>>) -> Result<Vec<[usize; 3]>> {
        let pts = &self.pts;
        let shell = nodes(&normalize(shell, pts, true), pts, &self.sites);
        if shell.len() < 3 {
            return Err(failure());
        }
        let holes =
            holes.into_iter().filter(|h| !h.is_empty()).map(|h| nodes(&normalize(h, pts, false), pts, &self.sites)).collect::<Vec<_>>();
        let ring = joined(shell, holes, pts)?;
        let mut faces = ears(&ring, pts)?;
        improve(&mut faces, pts);
        Ok(faces)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_test_matches_observed_jts_numeric_limits() {
        for scale in [1., 1e90] {
            let (a, b, c) = ((0., 0.), (scale, 0.), (0., scale));
            assert!(in_circle(a, b, c, (scale / 4., scale / 4.)));
            // JTS's executable returns false here at 1e90, despite the point
            // being inside the circle. Fixing it changes GEOS hull outcomes.
            assert_eq!(in_circle(a, b, c, (3. * scale / 4., 3. * scale / 4.)), scale == 1.);
            assert!(!in_circle(a, b, c, (2. * scale, 2. * scale)));
        }
    }
}
