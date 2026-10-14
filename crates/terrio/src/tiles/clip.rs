//! Clipping geometries to rectangles (tiles), and simplifying them.
//!
//! Polygon rings are clipped one at a time (Sutherland-Hodgman), which keeps the clipped area
//! exact; a ring that the rectangle cuts into several pieces stays one ring joined along the
//! rectangle's edge, where GEOS (which GDAL uses) would return a multipolygon.

/// A rectangle `(min_x, min_y, max_x, max_y)`.
pub(crate) type Rect = (f64, f64, f64, f64);

pub(crate) type Pt = (f64, f64);

/// The bounding box of points.
pub(crate) fn bbox<'a>(pts: impl IntoIterator<Item = &'a Pt>) -> Option<Rect> {
    let mut it = pts.into_iter();
    let first = it.next()?;
    let mut r = (first.0, first.1, first.0, first.1);
    for p in it {
        r.0 = r.0.min(p.0);
        r.1 = r.1.min(p.1);
        r.2 = r.2.max(p.0);
        r.3 = r.3.max(p.1);
    }
    Some(r)
}

/// Whether `inner` lies within `outer`.
pub(crate) fn contains(outer: Rect, inner: Rect) -> bool {
    inner.0 >= outer.0 && inner.1 >= outer.1 && inner.2 <= outer.2 && inner.3 <= outer.3
}

/// Whether two rectangles overlap (more than touch).
pub(crate) fn overlaps(a: Rect, b: Rect) -> bool {
    a.0 < b.2 && a.1 < b.3 && a.2 > b.0 && a.3 > b.1
}

/// The signed area of a ring (positive when counter-clockwise, y up).
pub(crate) fn ring_area(r: &[Pt]) -> f64 {
    let n = r.len();
    if n < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..n {
        let (p, q) = (r[i], r[(i + 1) % n]);
        a += p.0 * q.1 - q.0 * p.1;
    }
    a / 2.0
}

/// Clips a line to a rectangle (Liang-Barsky on each segment): the pieces inside.
pub(crate) fn clip_line(line: &[Pt], r: Rect) -> Vec<Vec<Pt>> {
    let mut pieces: Vec<Vec<Pt>> = Vec::new();
    let mut cur: Vec<Pt> = Vec::new();
    for w in line.windows(2) {
        let (a, b) = (w[0], w[1]);
        match clip_segment(a, b, r) {
            Some((p, q, cut_end)) => {
                if cur.last() != Some(&p) {
                    if cur.len() >= 2 {
                        pieces.push(std::mem::take(&mut cur));
                    }
                    cur.clear();
                    cur.push(p);
                }
                if q != p || cur.len() == 1 {
                    cur.push(q);
                }
                if cut_end {
                    if cur.len() >= 2 {
                        pieces.push(std::mem::take(&mut cur));
                    }
                    cur.clear();
                }
            }
            None => {
                if cur.len() >= 2 {
                    pieces.push(std::mem::take(&mut cur));
                }
                cur.clear();
            }
        }
    }
    if cur.len() >= 2 {
        pieces.push(cur);
    }
    for p in &mut pieces {
        p.dedup();
    }
    pieces.retain(|p| p.len() >= 2);
    pieces
}

/// Clips a segment: its part inside the rectangle, and whether its end was cut off.
fn clip_segment(a: Pt, b: Pt, r: Rect) -> Option<(Pt, Pt, bool)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, a.0 - r.0), (dx, r.2 - a.0), (-dy, a.1 - r.1), (dy, r.3 - a.1)] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                if t > t1 {
                    return None;
                }
                t0 = t0.max(t);
            } else {
                if t < t0 {
                    return None;
                }
                t1 = t1.min(t);
            }
        }
    }
    let at = |t: f64| {
        if t == 0.0 {
            a
        } else if t == 1.0 {
            b
        } else {
            (a.0 + t * dx, a.1 + t * dy)
        }
    };
    Some((at(t0), at(t1), t1 < 1.0))
}

/// Douglas-Peucker simplification of a path with a tolerance; rings keep their first and
/// middle points.
pub(crate) fn simplify(pts: &[Pt], tol: f64) -> Vec<Pt> {
    if pts.len() <= 2 || tol <= 0.0 {
        return pts.to_vec();
    }
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let closed = pts.first() == pts.last();
    let mut stack = if closed && pts.len() > 3 {
        let mid = pts.len() / 2;
        keep[mid] = true;
        vec![(0, mid), (mid, pts.len() - 1)]
    } else {
        vec![(0, pts.len() - 1)]
    };
    while let Some((s, e)) = stack.pop() {
        if e <= s + 1 {
            continue;
        }
        let (a, b) = (pts[s], pts[e]);
        let (mut best, mut idx) = (-1.0, s);
        for (i, p) in pts.iter().enumerate().take(e).skip(s + 1) {
            let d = seg_dist(*p, a, b);
            if d > best {
                best = d;
                idx = i;
            }
        }
        if best > tol {
            keep[idx] = true;
            stack.push((s, idx));
            stack.push((idx, e));
        }
    }
    pts.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

fn seg_dist(p: Pt, a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) };
    let (x, y) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - x).powi(2) + (p.1 - y).powi(2)).sqrt()
}

/// The position of a point of the rectangle's boundary along it, counter-clockwise (y up) from
/// the bottom-left corner: 0 to 1 along the bottom, 1 to 2 up the right side, 2 to 3 along the top
/// and 3 to 4 down the left side.
fn perimeter_pos(p: Pt, r: Rect) -> f64 {
    let (w, h) = ((r.2 - r.0).max(f64::MIN_POSITIVE), (r.3 - r.1).max(f64::MIN_POSITIVE));
    let d = [(p.1 - r.1).abs(), (p.0 - r.2).abs(), (p.1 - r.3).abs(), (p.0 - r.0).abs()];
    let side = (0..4).min_by(|a, b| d[*a].total_cmp(&d[*b])).unwrap_or(0);
    match side {
        0 => ((p.0 - r.0) / w).clamp(0.0, 1.0),
        1 => 1.0 + ((p.1 - r.1) / h).clamp(0.0, 1.0),
        2 => 2.0 + ((r.2 - p.0) / w).clamp(0.0, 1.0),
        _ => (3.0 + ((r.3 - p.1) / h).clamp(0.0, 1.0)) % 4.0,
    }
}

/// The corner of the rectangle at perimeter position 1, 2, 3 or 4 (0).
fn corner(k: usize, r: Rect) -> Pt {
    match k % 4 {
        1 => (r.2, r.1),
        2 => (r.2, r.3),
        3 => (r.0, r.3),
        _ => (r.0, r.1),
    }
}

/// Whether a point is inside a ring (even-odd rule).
pub(crate) fn point_in_ring(p: Pt, ring: &[Pt]) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
    }
    inside
}

/// A part of a ring inside the rectangle, from where it enters to where it leaves.
struct Chain {
    points: Vec<Pt>,
    start: f64,
    end: f64,
}

/// The parts of a closed ring (without its closing point) inside the rectangle: `None` when the
/// ring does not leave it.
fn chains(ring: &[Pt], r: Rect) -> Option<Vec<Chain>> {
    let n = ring.len();
    let inside = |p: &Pt| p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3;
    // Start the walk at a vertex outside.
    let first_out = (0..n).find(|&i| !inside(&ring[i]))?;
    let mut out: Vec<Chain> = Vec::new();
    let mut cur: Option<Vec<Pt>> = None;
    for k in 0..n {
        let a = ring[(first_out + k) % n];
        let b = ring[(first_out + k + 1) % n];
        let Some((p, q, cut_end)) = clip_segment(a, b, r) else { continue };
        let entered = p != a || !inside(&a);
        if cur.is_none() || entered && cur.as_ref().is_some_and(|c| c.last() != Some(&p)) {
            if let Some(c) = cur.take() {
                push_chain(&mut out, c, r);
            }
            cur = Some(vec![p]);
        }
        let c = cur.as_mut().expect("set above");
        if c.last() != Some(&q) {
            c.push(q);
        }
        if cut_end && let Some(c) = cur.take() {
            push_chain(&mut out, c, r);
        }
    }
    if let Some(c) = cur.take() {
        push_chain(&mut out, c, r);
    }
    Some(out)
}

fn push_chain(out: &mut Vec<Chain>, c: Vec<Pt>, r: Rect) {
    if c.len() >= 2 {
        let (start, end) = (perimeter_pos(c[0], r), perimeter_pos(*c.last().unwrap_or(&c[0]), r));
        out.push(Chain { points: c, start, end });
    }
}

/// Clips a polygon (exterior ring first, then holes; rings closed or not) to a rectangle, as an
/// intersection with it: the polygons left, each with its exterior ring counter-clockwise and its
/// holes clockwise (y up), rings closed. The parts of the rings inside the rectangle are joined
/// along its edges, so that a polygon the rectangle cuts in several pieces gives several polygons.
pub(crate) fn clip_polygon(rings: &[Vec<Pt>], r: Rect) -> Vec<Vec<Vec<Pt>>> {
    let open = |ring: &Vec<Pt>| {
        let mut v = ring.clone();
        v.dedup();
        while v.len() > 1 && v.first() == v.last() {
            v.pop();
        }
        v
    };
    let mut rings: Vec<Vec<Pt>> = rings.iter().map(open).filter(|v| v.len() >= 3).collect();
    if rings.is_empty() {
        return Vec::new();
    }
    // Exterior counter-clockwise, holes clockwise.
    for (i, ring) in rings.iter_mut().enumerate() {
        if (ring_area(ring) > 0.0) != (i == 0) {
            ring.reverse();
        }
    }
    let mut all_chains = Vec::new();
    let mut inner_outers = Vec::new();
    let mut inner_holes = Vec::new();
    for (i, ring) in rings.iter().enumerate() {
        match chains(ring, r) {
            Some(c) => all_chains.extend(c),
            None if i == 0 => inner_outers.push(ring.clone()),
            None => inner_holes.push(ring.clone()),
        }
    }
    let mut polys: Vec<Vec<Vec<Pt>>> = inner_outers.into_iter().map(|o| vec![o]).collect();
    if all_chains.is_empty() {
        // The rectangle may lie inside the polygon.
        let c = ((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0);
        // (Unless a hole around it holds it.)
        let in_hole = rings[1..].iter().any(|h| point_in_ring(c, h) && chains(h, r).is_some());
        if polys.is_empty() && point_in_ring(c, &rings[0]) && !in_hole {
            polys.push(vec![vec![(r.0, r.1), (r.2, r.1), (r.2, r.3), (r.0, r.3)]]);
        }
    } else {
        // Walk: from the end of a chain, along the boundary counter-clockwise to the next start.
        let mut used = vec![false; all_chains.len()];
        for s in 0..all_chains.len() {
            if used[s] {
                continue;
            }
            let mut ring: Vec<Pt> = Vec::new();
            let mut cur = s;
            for _ in 0..=all_chains.len() {
                used[cur] = true;
                ring.extend_from_slice(&all_chains[cur].points);
                let end = all_chains[cur].end;
                let next = (0..all_chains.len())
                    .filter(|&k| !used[k] || k == s)
                    .min_by(|&a, &b| {
                        let da = (all_chains[a].start - end).rem_euclid(4.0);
                        let db = (all_chains[b].start - end).rem_euclid(4.0);
                        da.total_cmp(&db)
                    })
                    .unwrap_or(s);
                // The corners passed on the way.
                let target = all_chains[next].start;
                let dist = (target - end).rem_euclid(4.0);
                let mut k = end.floor() as usize + 1;
                while (k as f64) < end + dist {
                    ring.push(corner(k, r));
                    k += 1;
                }
                if next == s {
                    break;
                }
                cur = next;
            }
            ring.dedup();
            while ring.len() > 1 && ring.first() == ring.last() {
                ring.pop();
            }
            if ring.len() >= 3 && ring_area(&ring) > 0.0 {
                polys.push(vec![ring]);
            }
        }
    }
    // Holes inside the rectangle go to the polygon that holds them.
    for h in inner_holes {
        if let Some(p) = polys.iter_mut().find(|p| point_in_ring(h[0], &p[0])) {
            p.push(h);
        }
    }
    for p in &mut polys {
        for ring in p.iter_mut() {
            let f = ring[0];
            ring.push(f);
        }
    }
    polys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_rings_and_lines() {
        let sq = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        let c = clip_polygon(std::slice::from_ref(&sq), (5.0, -1.0, 20.0, 20.0));
        assert_eq!(ring_area(&c[0][0][..c[0][0].len() - 1]).abs(), 50.0);
        assert!(clip_polygon(&[sq], (10.0, 0.0, 20.0, 20.0)).is_empty());
        let line = [(-5.0, 5.0), (5.0, 5.0), (5.0, 15.0), (6.0, 15.0), (6.0, 5.0)];
        let pieces = clip_line(&line, (0.0, 0.0, 10.0, 10.0));
        assert_eq!(pieces, vec![vec![(0.0, 5.0), (5.0, 5.0), (5.0, 10.0)], vec![(6.0, 10.0), (6.0, 5.0)]]);
        assert_eq!(simplify(&[(0.0, 0.0), (1.0, 0.1), (2.0, 0.0)], 0.5), vec![(0.0, 0.0), (2.0, 0.0)]);
        // A U shape cut by the rectangle into two pieces.
        let u = vec![(0.0, 0.0), (3.0, 0.0), (3.0, 3.0), (2.0, 3.0), (2.0, 1.0), (1.0, 1.0), (1.0, 3.0), (0.0, 3.0)];
        let parts = clip_polygon(std::slice::from_ref(&u), (-1.0, 2.0, 4.0, 4.0));
        assert_eq!(parts.len(), 2);
        let area: f64 = parts.iter().map(|p| ring_area(&p[0][..p[0].len() - 1])).sum();
        assert_eq!(area, 2.0);
        // Inside, covering and with a hole crossing the edge.
        assert_eq!(clip_polygon(std::slice::from_ref(&u), (-1.0, -1.0, 4.0, 4.0)).len(), 1);
        let big = vec![(-10.0, -10.0), (10.0, -10.0), (10.0, 10.0), (-10.0, 10.0)];
        let covered = clip_polygon(std::slice::from_ref(&big), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(ring_area(&covered[0][0][..4]), 1.0);
        let hole = vec![(0.5, 0.25), (0.5, 0.75), (2.0, 0.75), (2.0, 0.25)];
        let holed = clip_polygon(&[big, hole], (0.0, 0.0, 1.0, 1.0));
        assert_eq!(holed.len(), 1);
        let a = ring_area(&holed[0][0][..holed[0][0].len() - 1]);
        assert!((a - 0.75).abs() < 1e-12, "{a}");
    }
}
