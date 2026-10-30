// Written to match the behavior of:
//   GEOS 3.14.1:
//     src/operation/valid/MakeValid.cpp
// No code is translated from these sources.

//! Planar subdivision of the noded linework, the iterative linework fill and
//! polygon assembly. See notes/design.md for the behavioural model.

use super::arrangement::{Arrangement, Edge, Segment, V2, cross};
use super::exact::RPoint;
use super::{P, Polygon, RepairError};
use std::cmp::Ordering;

const NONE: usize = usize::MAX;

pub struct UnionFind {
    parent: Vec<usize>,
    size: Vec<u32>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        UnionFind { parent: (0..n).collect(), size: vec![1; n] }
    }

    pub fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            let grand = self.parent[self.parent[x]];
            self.parent[x] = grand;
            x = grand;
        }
        x
    }

    pub fn union(&mut self, a: usize, b: usize) -> usize {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return a;
        }
        let (big, small) = if self.size[a] >= self.size[b] { (a, b) } else { (b, a) };
        self.parent[small] = big;
        self.size[big] = self.size[big].saturating_add(self.size[small]);
        big
    }
}

/// The noded graph with exact vertex ranks and integer edge directions.
pub struct Graph {
    pub points: Vec<RPoint>,
    pub rank: Vec<usize>,
    pub edges: Vec<Edge>,
    segments: Vec<Segment>,
    dirs: Vec<V2>,
    /// Counter-clockwise rotation of all half-edges around every vertex.
    pub all: Rotation,
}

impl Graph {
    pub fn new(arrangement: Arrangement) -> Result<Self, RepairError> {
        let Arrangement { segments, points, edges } = arrangement;
        let dirs = edges.iter().map(|e| segments[e.segment].direction()).collect();
        let mut order: Vec<usize> = (0..points.len()).collect();
        order.sort_unstable_by(|&a, &b| points[a].cmp_lex(&points[b]));
        let mut rank = vec![0; points.len()];
        for (r, &v) in order.iter().enumerate() {
            rank[v] = r;
        }
        let mut graph = Graph { points, rank, edges, segments, dirs, all: Rotation::empty() };
        graph.all = graph.rotation(|_| true)?;
        Ok(graph)
    }

    pub fn half_edges(&self) -> usize {
        self.edges.len() * 2
    }

    pub fn origin(&self, h: usize) -> usize {
        let e = &self.edges[h >> 1];
        if h & 1 == 0 { e.u } else { e.v }
    }

    pub fn dest(&self, h: usize) -> usize {
        self.origin(h ^ 1)
    }

    pub fn dir(&self, h: usize) -> V2 {
        let d = self.dirs[h >> 1];
        if h & 1 == 0 { d } else { (-d.0, -d.1) }
    }

    /// Counter-clockwise rotation restricted to edges accepted by `keep`.
    /// Built by filtering the full rotation when it exists (order is preserved).
    pub fn rotation(&self, keep: impl Fn(usize) -> bool) -> Result<Rotation, RepairError> {
        let n = self.points.len();
        let mut start = vec![0usize; n + 1];
        let mut list = Vec::new();
        let mut pos = vec![NONE; self.half_edges()];
        if self.all.start.len() == n + 1 {
            for (v, slot) in start.iter_mut().enumerate().take(n) {
                *slot = list.len();
                for &h in &self.all.list[self.all.start[v]..self.all.start[v + 1]] {
                    if keep(h >> 1) {
                        pos[h] = list.len();
                        list.push(h);
                    }
                }
            }
            start[n] = list.len();
            return Ok(Rotation { start, list, pos });
        }
        for h in 0..self.half_edges() {
            start[self.origin(h) + 1] += 1;
        }
        for v in 0..n {
            start[v + 1] += start[v];
        }
        let mut fill = start.clone();
        list = vec![0; self.half_edges()];
        for h in 0..self.half_edges() {
            let o = self.origin(h);
            list[fill[o]] = h;
            fill[o] += 1;
        }
        for v in 0..n {
            let slice = &mut list[start[v]..start[v + 1]];
            slice.sort_unstable_by(|&a, &b| angle_cmp(self.dir(a), self.dir(b)));
            for w in slice.windows(2) {
                if angle_cmp(self.dir(w[0]), self.dir(w[1])) != Ordering::Less {
                    return Err(RepairError::new("two edges leave a vertex in the same direction"));
                }
            }
        }
        for (i, &h) in list.iter().enumerate() {
            pos[h] = i;
        }
        Ok(Rotation { start, list, pos })
    }
}

fn half_plane(d: V2) -> u8 {
    if d.1 > 0 || (d.1 == 0 && d.0 > 0) { 0 } else { 1 }
}

/// Counter-clockwise angular order starting at direction (1, 0).
fn angle_cmp(a: V2, b: V2) -> Ordering {
    half_plane(a).cmp(&half_plane(b)).then_with(|| 0.cmp(&cross(a, b)))
}

pub struct Rotation {
    start: Vec<usize>,
    list: Vec<usize>,
    pos: Vec<usize>,
}

impl Rotation {
    fn empty() -> Self {
        Rotation { start: Vec::new(), list: Vec::new(), pos: Vec::new() }
    }

    /// Keep only half-edges whose edge satisfies `keep`, preserving order.
    fn retain(&mut self, keep: impl Fn(usize) -> bool) {
        let n = self.start.len().saturating_sub(1);
        let mut write = 0;
        let mut read = self.start.first().copied().unwrap_or(0);
        for v in 0..n {
            let end = self.start[v + 1];
            self.start[v] = write;
            for i in read..end {
                let h = self.list[i];
                if keep(h >> 1) {
                    self.list[write] = h;
                    self.pos[h] = write;
                    write += 1;
                } else {
                    self.pos[h] = NONE;
                }
            }
            read = end;
        }
        if n > 0 || !self.start.is_empty() {
            self.start[n] = write;
        }
        self.list.truncate(write);
    }

    fn clone_all(&self) -> Self {
        Rotation { start: self.start.clone(), list: self.list.clone(), pos: self.pos.clone() }
    }

    pub fn around(&self, v: usize) -> &[usize] {
        &self.list[self.start[v]..self.start[v + 1]]
    }

    /// Face-tracing successor: the clockwise neighbour of the twin at the head.
    fn next(&self, graph: &Graph, h: usize) -> Result<usize, RepairError> {
        let twin = h ^ 1;
        let w = graph.origin(twin);
        let p = self.pos[twin];
        let (s, e) = (self.start[w], self.start[w + 1]);
        if p == NONE || p < s || p >= e {
            return Err(RepairError::new("half-edge missing from rotation"));
        }
        Ok(self.list[if p == s { e - 1 } else { p - 1 }])
    }
}

/// Reusable scratch space for ring tracing.
struct Tracer {
    stamp: Vec<u32>,
    generation: u32,
    at_vertex: Vec<usize>,
    walk: Vec<usize>,
    stack: Vec<usize>,
}

impl Tracer {
    fn new(graph: &Graph) -> Self {
        Tracer {
            stamp: vec![0; graph.half_edges()],
            generation: 0,
            at_vertex: vec![NONE; graph.points.len()],
            walk: Vec::new(),
            stack: Vec::new(),
        }
    }

    /// Trace every face walk of `rotation` that starts from a half-edge accepted
    /// by `seed`, split each at repeated vertices into simple rings, and call
    /// `ring` on each.
    fn rings(
        &mut self,
        graph: &Graph,
        rotation: &Rotation,
        seed: impl Fn(usize) -> bool,
        mut ring: impl FnMut(&[usize]) -> Result<(), RepairError>,
    ) -> Result<(), RepairError> {
        self.generation = self.generation.checked_add(1).ok_or(RepairError::new("tracer generation overflow"))?;
        let generation = self.generation;
        let total = graph.half_edges();
        for &h in &rotation.list {
            if self.stamp[h] == generation || !seed(h) {
                continue;
            }
            self.walk.clear();
            let mut current = h;
            loop {
                if self.stamp[current] == generation || self.walk.len() > total {
                    return Err(RepairError::new("face walk does not close"));
                }
                self.stamp[current] = generation;
                self.walk.push(current);
                current = rotation.next(graph, current)?;
                if current == h {
                    break;
                }
            }
            if graph.origin(self.walk[0]) != graph.dest(self.walk[self.walk.len() - 1]) {
                return Err(RepairError::new("face walk is not closed"));
            }
            self.stack.clear();
            for i in 0..self.walk.len() {
                let g = self.walk[i];
                let o = graph.origin(g);
                let p = self.at_vertex[o];
                if p != NONE {
                    for &r in &self.stack[p..] {
                        self.at_vertex[graph.origin(r)] = NONE;
                    }
                    ring(&self.stack[p..])?;
                    self.stack.truncate(p);
                }
                self.at_vertex[o] = self.stack.len();
                self.stack.push(g);
            }
            for &r in &self.stack {
                self.at_vertex[graph.origin(r)] = NONE;
            }
            ring(&self.stack)?;
        }
        Ok(())
    }
}

/// Exact orientation of a simple ring from the turn at its minimum vertex.
fn ring_is_ccw(graph: &Graph, ring: &[usize]) -> Result<bool, RepairError> {
    if ring.len() < 3 {
        return Err(RepairError::new("degenerate ring in planar graph"));
    }
    let mut best = 0;
    for i in 1..ring.len() {
        if graph.rank[graph.origin(ring[i])] < graph.rank[graph.origin(ring[best])] {
            best = i;
        }
    }
    let incoming = ring[if best == 0 { ring.len() - 1 } else { best - 1 }];
    match cross(graph.dir(incoming), graph.dir(ring[best])).cmp(&0) {
        Ordering::Greater => Ok(true),
        Ordering::Less => Ok(false),
        Ordering::Equal => Err(RepairError::new("ring has no turn at its minimum vertex")),
    }
}

/// Faces of the full arrangement: an id per half-edge (face on its left).
pub struct Faces {
    pub of_half_edge: Vec<usize>,
    pub count: usize,
    pub unbounded: usize,
}

struct RayEdge {
    px: i128,
    py: i128,
    dx: i128,
    dy: i128,
}

impl RayEdge {
    fn numerator(&self, y: i128) -> i128 {
        self.px * self.dy + (y - self.py) * self.dx
    }

    /// Order of the two lines at height `y + epsilon`.
    fn cmp_at(&self, other: &RayEdge, y: i128) -> Ordering {
        (self.numerator(y) * other.dy).cmp(&(other.numerator(y) * self.dy)).then_with(|| (self.dx * other.dy).cmp(&(other.dx * self.dy)))
    }

    fn left_of(&self, x: i128, y: i128) -> bool {
        self.numerator(y) < x * self.dy
    }
}

pub fn atomic_faces(graph: &Graph) -> Result<Faces, RepairError> {
    let total = graph.half_edges();
    let rotation = &graph.all;
    let mut walk_of = vec![NONE; total];
    let mut walks = 0;
    for h in 0..total {
        if walk_of[h] != NONE {
            continue;
        }
        let mut current = h;
        let mut steps = 0;
        loop {
            walk_of[current] = walks;
            current = rotation.next(graph, current)?;
            steps += 1;
            if current == h {
                break;
            }
            if walk_of[current] != NONE || steps > total {
                return Err(RepairError::new("arrangement walk does not close"));
            }
        }
        walks += 1;
    }

    // Connected components and the outer walk of each.
    let n = graph.points.len();
    let mut components = UnionFind::new(n);
    for e in &graph.edges {
        components.union(e.u, e.v);
    }
    let mut lowest = vec![NONE; n];
    for v in 0..n {
        if rotation.around(v).is_empty() {
            continue;
        }
        let root = components.find(v);
        if lowest[root] == NONE || graph.rank[v] < graph.rank[lowest[root]] {
            lowest[root] = v;
        }
    }
    let mut queries: Vec<(usize, usize)> = Vec::new(); // (min vertex, outer walk)
    for (root, &v) in lowest.iter().enumerate() {
        if v == NONE || components.find(root) != root {
            continue;
        }
        let around = rotation.around(v);
        let upper = around.iter().rposition(|&h| half_plane(graph.dir(h)) == 0);
        let steepest = around[upper.unwrap_or(around.len() - 1)];
        queries.push((v, walk_of[steepest]));
    }
    queries.sort_unstable_by_key(|q| graph.rank[q.0]);

    let unbounded_node = walks;
    let mut uf = UnionFind::new(walks + 1);
    if queries.len() == 1 {
        uf.union(queries[0].1, unbounded_node);
    } else if !queries.is_empty() {
        let hits = locate(graph, &queries)?;
        for (q, hit) in queries.iter().zip(hits) {
            uf.union(q.1, hit.map_or(unbounded_node, |h| walk_of[h]));
        }
    }
    let mut dense = vec![NONE; walks + 1];
    let mut count = 0;
    let mut id_of = |w: usize, uf: &mut UnionFind| {
        let r = uf.find(w);
        if dense[r] == NONE {
            dense[r] = count;
            count += 1;
        }
        dense[r]
    };
    let unbounded = id_of(unbounded_node, &mut uf);
    let of_half_edge = walk_of.iter().map(|&w| id_of(w, &mut uf)).collect();
    Ok(Faces { of_half_edge, count, unbounded })
}

/// For each component's integer minimum vertex, the nearest edge hit by a ray
/// towards -x at height y + epsilon, returned as its downward half-edge.
fn locate(graph: &Graph, queries: &[(usize, usize)]) -> Result<Vec<Option<usize>>, RepairError> {
    let mut qpoints = Vec::with_capacity(queries.len());
    for &(v, _) in queries {
        let p = graph.points[v];
        if !p.is_integer() {
            return Err(RepairError::new("component minimum is not an input vertex"));
        }
        qpoints.push((p.x, p.y));
    }
    let mut ys: Vec<i128> = qpoints.iter().map(|p| p.1).collect();
    ys.sort_unstable();
    ys.dedup();
    let size = ys.len().next_power_of_two();
    let mut nodes: Vec<Vec<usize>> = vec![Vec::new(); 2 * size];
    let mut rays = Vec::with_capacity(graph.edges.len());
    for (index, e) in graph.edges.iter().enumerate() {
        let Segment { a, b } = graph.segments[e.segment];
        let (p, q) = if a.1 <= b.1 { (a, b) } else { (b, a) };
        rays.push(RayEdge { px: i128::from(p.0), py: i128::from(p.1), dx: i128::from(q.0 - p.0), dy: i128::from(q.1 - p.1) });
        let (u, v) = (graph.points[e.u], graph.points[e.v]);
        let (low, high) = if u.cmp_y(&v) == Ordering::Greater { (v, u) } else { (u, v) };
        let first = low.ceil_y();
        let last = high.ceil_y() - 1;
        if first > last {
            continue;
        }
        let mut l = ys.partition_point(|&y| y < first) + size;
        let mut r = ys.partition_point(|&y| y <= last) + size;
        while l < r {
            if l & 1 == 1 {
                nodes[l].push(index);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                nodes[r].push(index);
            }
            l >>= 1;
            r >>= 1;
        }
    }
    let mut lowest_leaf = vec![0usize; 2 * size];
    for (i, slot) in lowest_leaf.iter_mut().enumerate().skip(1) {
        let mut leaf = i;
        while leaf < size {
            leaf <<= 1;
        }
        *slot = leaf - size;
    }
    for (i, list) in nodes.iter_mut().enumerate() {
        if list.len() > 1 {
            let y = ys[lowest_leaf[i]];
            list.sort_unstable_by(|&a, &b| rays[a].cmp_at(&rays[b], y));
        }
    }
    let mut hits = Vec::with_capacity(queries.len());
    for &(qx, qy) in &qpoints {
        let leaf = ys.partition_point(|&y| y < qy);
        let mut node = leaf + size;
        let mut best: Option<usize> = None;
        while node >= 1 {
            let list = &nodes[node];
            let k = list.partition_point(|&e| rays[e].left_of(qx, qy));
            if k > 0 {
                let candidate = list[k - 1];
                if best.is_none_or(|b| rays[candidate].cmp_at(&rays[b], qy) == Ordering::Greater) {
                    best = Some(candidate);
                }
            }
            node >>= 1;
        }
        hits.push(best.map(|e| if graph.dir(2 * e).1 < 0 { 2 * e } else { 2 * e + 1 }));
    }
    Ok(hits)
}

/// The iterative linework fill; returns membership of each atomic face in R.
///
/// Each iteration costs O(n + alive edges + live faces); the number of
/// iterations is bounded by the edge count because each removes an edge.
pub fn linework_fill(graph: &Graph, faces: &Faces) -> Result<Vec<bool>, RepairError> {
    let m = graph.edges.len();
    let total = graph.half_edges();
    let face = &faces.of_half_edge;
    let mut uf = UnionFind::new(faces.count);
    let mut in_r = vec![false; faces.count];
    let mut alive: Vec<usize> = (0..m).collect();
    let mut live_faces: Vec<usize> = (0..faces.count).filter(|&f| f != faces.unbounded).collect();
    let mut effective = vec![true; m];
    let mut rotation = graph.all.clone_all();
    let mut tracer = Tracer::new(graph);
    let mut cw = vec![false; total];
    let mut class_h = vec![NONE; total];
    let mut class_of = vec![NONE; faces.count];
    let mut class_stamp = vec![0u32; faces.count];
    let mut roots: Vec<usize> = Vec::new();
    let mut start: Vec<usize> = Vec::new();
    let mut bucket: Vec<usize> = Vec::new();
    let mut depth: Vec<i64> = Vec::new();
    let mut kept: Vec<bool> = Vec::new();
    let mut queue: Vec<usize> = Vec::new();
    let mut removed: Vec<usize> = Vec::new();
    let mut iteration = 0u32;
    loop {
        iteration += 1;
        if iteration as usize > m + 1 {
            return Err(RepairError::new("linework iteration did not terminate"));
        }
        // Edges with one face on both sides (dangles, cut edges) never matter again.
        alive.retain(|&e| {
            let keep = uf.find(face[2 * e]) != uf.find(face[2 * e + 1]);
            effective[e] = keep;
            keep
        });
        if alive.is_empty() {
            break;
        }
        rotation.retain(|e| effective[e]);
        tracer.rings(
            graph,
            &rotation,
            |_| true,
            |ring| {
                let clockwise = !ring_is_ccw(graph, ring)?;
                for &h in ring {
                    cw[h] = clockwise;
                }
                Ok(())
            },
        )?;
        // Dense classes (current faces) of the alive half-edges.
        roots.clear();
        for &e in &alive {
            for h in [2 * e, 2 * e + 1] {
                let r = uf.find(face[h]);
                if class_stamp[r] != iteration {
                    class_stamp[r] = iteration;
                    class_of[r] = roots.len();
                    roots.push(r);
                }
                class_h[h] = class_of[r];
            }
        }
        let classes = roots.len();
        let unbounded_root = uf.find(faces.unbounded);
        if class_stamp[unbounded_root] != iteration {
            return Err(RepairError::new("unbounded face has no boundary"));
        }
        let unbounded = class_of[unbounded_root];
        start.clear();
        start.resize(classes + 1, 0);
        for &e in &alive {
            start[class_h[2 * e] + 1] += 1;
            start[class_h[2 * e + 1] + 1] += 1;
        }
        for c in 0..classes {
            start[c + 1] += start[c];
        }
        bucket.clear();
        bucket.resize(start[classes], 0);
        {
            let mut fill = start.clone();
            for &e in &alive {
                for h in [2 * e, 2 * e + 1] {
                    let c = class_h[h];
                    bucket[fill[c]] = h;
                    fill[c] += 1;
                }
            }
        }
        // Depth = number of clockwise minimal rings of bounded faces enclosing a
        // face; crossing half-edge h changes it by cw(h) - cw(twin).
        let counts = |h: usize| i64::from(cw[h] && class_h[h] != unbounded);
        depth.clear();
        depth.resize(classes, i64::MIN);
        depth[unbounded] = 0;
        queue.clear();
        queue.push(unbounded);
        let mut head = 0;
        while head < queue.len() {
            let c = queue[head];
            head += 1;
            for &h in &bucket[start[c]..start[c + 1]] {
                let other = class_h[h ^ 1];
                let d = depth[c] + counts(h) - counts(h ^ 1);
                if depth[other] == i64::MIN {
                    depth[other] = d;
                    queue.push(other);
                } else if depth[other] != d {
                    return Err(RepairError::new("inconsistent nesting depth"));
                }
            }
        }
        if depth.iter().any(|&d| d < 0) {
            return Err(RepairError::new("face unreachable or negative depth"));
        }
        kept.clear();
        kept.extend((0..classes).map(|c| c != unbounded && depth[c] % 2 == 0));
        live_faces.retain(|&a| uf.find(a) != unbounded_root);
        for &a in &live_faces {
            let r = uf.find(a);
            if class_stamp[r] == iteration && kept[class_of[r]] {
                in_r[a] = !in_r[a];
            }
        }
        removed.clear();
        removed.extend(alive.iter().copied().filter(|&e| kept[class_h[2 * e]] != kept[class_h[2 * e + 1]]));
        if removed.is_empty() {
            break;
        }
        for &e in &removed {
            uf.union(face[2 * e], face[2 * e + 1]);
        }
    }
    Ok(in_r)
}

/// Build output polygons from the atomic faces in R.
pub fn assemble(graph: &Graph, faces: &Faces, in_r: &[bool]) -> Result<Vec<Polygon<P>>, RepairError> {
    let face = &faces.of_half_edge;
    let m = graph.edges.len();
    let boundary: Vec<bool> = (0..m).map(|e| in_r[face[2 * e]] != in_r[face[2 * e + 1]]).collect();
    let rotation = graph.rotation(|e| boundary[e])?;
    let mut rings: Vec<Vec<usize>> = Vec::new();
    Tracer::new(graph).rings(
        graph,
        &rotation,
        |h| in_r[face[h]],
        |ring| {
            rings.push(ring.to_vec());
            Ok(())
        },
    )?;
    let mut interiors = UnionFind::new(faces.count);
    for e in 0..m {
        if in_r[face[2 * e]] && in_r[face[2 * e + 1]] {
            interiors.union(face[2 * e], face[2 * e + 1]);
        }
    }
    // (component root, is shell, rank key, coordinates)
    let mut parts: Vec<(usize, bool, usize, Vec<P>)> = Vec::with_capacity(rings.len());
    for ring in &rings {
        let shell = ring_is_ccw(graph, ring)?;
        let component = interiors.find(face[ring[0]]);
        // Reverse to the oracle convention (shells clockwise, holes counter-clockwise).
        let mut vertices: Vec<usize> = ring.iter().rev().map(|&h| graph.dest(h)).collect();
        let lowest = (0..vertices.len()).min_by_key(|&i| graph.rank[vertices[i]]).unwrap_or(0);
        vertices.rotate_left(lowest);
        let key = graph.rank[vertices[0]];
        let mut coordinates: Vec<P> = Vec::with_capacity(vertices.len() + 1);
        for &v in &vertices {
            let c = graph.points[v].to_f64();
            if coordinates.last() != Some(&c) {
                coordinates.push(c);
            }
        }
        while coordinates.len() > 1 && coordinates.first() == coordinates.last() {
            coordinates.pop();
        }
        let mut distinct = coordinates.clone();
        distinct.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        distinct.dedup();
        if distinct.len() < 3 {
            // Only possible when distinct exact nodes round to one binary64 value.
            coordinates.clear();
        } else {
            coordinates.push(coordinates[0]);
        }
        parts.push((component, shell, key, coordinates));
    }
    parts.sort_unstable_by_key(|p| (p.0, !p.1, p.2));
    let mut polygons: Vec<(usize, Polygon<P>)> = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let component = parts[i].0;
        let mut j = i;
        while j < parts.len() && parts[j].0 == component {
            j += 1;
        }
        // Sorted so that a component's shell (if any) comes first.
        if !parts[i].1 || parts[i + 1..j].iter().any(|p| p.1) {
            return Err(RepairError::new("area component without exactly one shell"));
        }
        if !parts[i].3.is_empty() {
            let key = parts[i].2;
            let polygon: Polygon<P> = parts[i..j].iter_mut().filter(|p| !p.3.is_empty()).map(|p| std::mem::take(&mut p.3)).collect();
            polygons.push((key, polygon));
        }
        i = j;
    }
    polygons.sort_unstable_by_key(|p| p.0);
    Ok(polygons.into_iter().map(|p| p.1).collect())
}
