// SPDX-FileCopyrightText: Copyright (c) A5 contributors
// SPDX-FileCopyrightText: Copyright (c) 2014-2025, Ecere Corporation
// SPDX-FileCopyrightText: Copyright (c) 2015-2021, Brandon Jones, Colin MacKenzie IV
// SPDX-FileCopyrightText: PROJ contributors
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause AND MIT
//
// Derived from (translated to Rust and modified):
//   PROJ 9.9.0 (Felix Palmer's polyhedral projections, derived from A5, Apache-2.0, in
//   snyder.h from DGGAL, BSD-3-Clause, and in vec3.h from gl-matrix, MIT):
//     src/projections/polyhedral.cpp
//     src/projections/polyhedral/state.h
//     src/projections/polyhedral/snyder.h
//     src/projections/polyhedral/unfold.h
//     src/projections/polyhedral/conway.h
//     src/projections/polyhedral/polyhedra/icosahedron.h
//     src/projections/polyhedral/polyhedra/dodecahedron.h
//     src/projections/polyhedral/nets/isea/isea.h
//     src/projections/polyhedral/nets/dsea/dsea.h
//     src/vec3.h
//     src/sphere.h

//! Icosahedral Snyder Equal Area (`+proj=isea`) as PROJ 9.9.0 computes it: Snyder's equal-area
//! projection of the authalic sphere onto the faces of a polyhedron, each face split into triangles
//! from its centre (Conway's meta operation), and the faces unfolded into a planar net. `isea` is
//! the icosahedron in Snyder's orientation, on PROJ's 4 × 5 strip; with `+dual` it is the
//! dodecahedron family (PROJ's `dodecahedron_family_setup(dual)`, which reads `+net`): the
//! dodecahedron's net with the icosahedron's triangles. Reference: PROJ 9.9.0, as GDAL 3.13.3 runs
//! it (`scripts/compare_isea_with_gdal.py`). See NOTICE for the licenses and attribution.

use super::{Authalic, Proj, Setup, err};
use crate::Result;
use crate::math::{HALFPI, adjlon};
use std::f64::consts::PI;

/// PROJ's `DEG_TO_RAD`.
const DEG_TO_RAD: f64 = 0.017453292519943296;

type Vec3 = [f64; 3];

fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// `std::hypot(x, y, z)` as libstdc++ computes it (`__hypot3`), which PROJ's builds on Linux use.
fn length(v: Vec3) -> f64 {
    let [x, y, z] = v.map(f64::abs);
    let a = if x < y {
        if y < z { z } else { y }
    } else if x < z {
        z
    } else {
        x
    };
    if a == 0.0 {
        return 0.0;
    }
    a * ((x / a) * (x / a) + (y / a) * (y / a) + (z / a) * (z / a)).sqrt()
}

fn normalize(v: Vec3) -> Vec3 {
    let len = length(v);
    if len == 0.0 { v } else { [v[0] / len, v[1] / len, v[2] / len] }
}

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(v: Vec3, s: f64) -> Vec3 {
    [v[0] * s, v[1] * s, v[2] * s]
}

fn lerp(a: Vec3, b: Vec3, t: f64) -> Vec3 {
    [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1]), a[2] + t * (b[2] - a[2])]
}

fn angle(a: Vec3, b: Vec3) -> f64 {
    let (aa, bb) = (scale(a, length(b)), scale(b, length(a)));
    2.0 * length(sub(aa, bb)).atan2(length(add(aa, bb)))
}

// ---------------------------------------------------------------------------------------------
// Unit-sphere geometry (`sphere.h`).

/// A value proportional to sin(angle / 2) between two unit vectors.
fn vector_difference(a: Vec3, b: Vec3) -> f64 {
    let d = length(sub(a, b));
    let s = length(add(a, b));
    d / d.hypot(s)
}

fn triple_product(a: Vec3, b: Vec3, c: Vec3) -> f64 {
    dot(a, cross(b, c))
}

/// dot(a, c × d) b - dot(b, c × d) a.
fn quadruple_product(a: Vec3, b: Vec3, c: Vec3, d: Vec3) -> Vec3 {
    let cd = cross(c, d);
    sub(scale(b, dot(a, cd)), scale(a, dot(b, cd)))
}

fn slerp(a: Vec3, b: Vec3, t: f64) -> Vec3 {
    let gamma = angle(a, b);
    if gamma == 0.0 {
        return lerp(a, b, t);
    }
    let sin_gamma = gamma.sin();
    let wa = ((1.0 - t) * gamma).sin() / sin_gamma;
    let wb = (t * gamma).sin() / sin_gamma;
    add(scale(a, wa), scale(b, wb))
}

/// The signed spherical excess of a triangle of unit vectors.
fn spherical_triangle_area(v1: Vec3, v2: Vec3, v3: Vec3) -> f64 {
    let num = triple_product(v1, v2, v3);
    let den = 1.0 + dot(v1, v2) + dot(v2, v3) + dot(v3, v1);
    2.0 * num.atan2(den)
}

// ---------------------------------------------------------------------------------------------
// Snyder's equal-area mapping of a spherical triangle onto a planar one (`snyder.h`).

type Face2D = (f64, f64);
type FaceTriangle = [Face2D; 3];
type SphericalTriangle = [Vec3; 3];

fn face_to_barycentric(p: Face2D, tri: &FaceTriangle) -> [f64; 3] {
    let [a, b, c] = *tri;
    let d31 = [a.0 - c.0, a.1 - c.1];
    let d23 = [c.0 - b.0, c.1 - b.1];
    let d3p = [p.0 - c.0, p.1 - c.1];
    let det = d23[0] * d31[1] - d23[1] * d31[0];
    let b0 = (d23[0] * d3p[1] - d23[1] * d3p[0]) / det;
    let b1 = (d31[0] * d3p[1] - d31[1] * d3p[0]) / det;
    [b0, b1, 1.0 - (b0 + b1)]
}

fn barycentric_to_face(b: [f64; 3], tri: &FaceTriangle) -> Face2D {
    let [p, q, r] = *tri;
    (b[0] * p.0 + b[1] * q.0 + b[2] * r.0, b[0] * p.1 + b[1] * q.1 + b[2] * r.1)
}

fn snyder_fwd(v: Vec3, stri: &SphericalTriangle, ftri: &FaceTriangle) -> Face2D {
    let [a, b, c] = *stri;
    let z = normalize(sub(v, a));
    let mut p = normalize(quadruple_product(a, z, b, c));
    // On the hemisphere of b and c: the product may give the antipodal intersection.
    if dot(p, b) + dot(p, c) < 0.0 {
        p = [-p[0], -p[1], -p[2]];
    }
    let h = vector_difference(a, v) / vector_difference(a, p);
    let area_abc = spherical_triangle_area(a, b, c);
    let scaled_area = h / area_abc;
    let area_apc = spherical_triangle_area(a, p, c);
    let area_abp = spherical_triangle_area(a, b, p);
    barycentric_to_face([1.0 - h, scaled_area * area_apc, scaled_area * area_abp], ftri)
}

fn snyder_inv(point: Face2D, ftri: &FaceTriangle, stri: &SphericalTriangle) -> Vec3 {
    let [a, b, c] = *stri;
    let bc = face_to_barycentric(point, ftri);
    const THRESHOLD: f64 = 1.0 - 1e-14;
    if bc[0] > THRESHOLD {
        return a;
    }
    if bc[1] > THRESHOLD {
        return b;
    }
    if bc[2] > THRESHOLD {
        return c;
    }
    let c1 = cross(b, c);
    let area_abc = spherical_triangle_area(a, b, c);
    let h = 1.0 - bc[0];
    let r = bc[2] / h;
    let alpha = r * area_abc;
    let s = alpha.sin();
    let half_c = (alpha / 2.0).sin();
    // 1 - cos(alpha).
    let cc = 2.0 * half_c * half_c;
    let c01 = dot(a, b);
    let c12 = dot(b, c);
    let c20 = dot(c, a);
    let s12 = length(c1);
    let tp = dot(a, c1);
    let f = s * tp + cc * (c01 * c12 - c20);
    let g = cc * s12 * (1.0 + c01);
    let q = (2.0 / c12.acos()) * g.atan2(f);
    let pp = slerp(b, c, q);
    let k = vector_difference(a, pp);
    let t = (h * k).asin() / k.asin();
    slerp(a, pp, t)
}

// ---------------------------------------------------------------------------------------------
// Polyhedra and their nets.

/// A polyhedron inscribed in the unit sphere, or a planar net: vertices and faces (counter-
/// clockwise from outside).
struct Mesh {
    vertices: Vec<Vec3>,
    faces: Vec<Vec<usize>>,
}

/// The regular icosahedron, V0 at the north pole (`icosahedron.h`). Its faces in four rows of
/// five: north cap, upper band, lower band, south cap.
fn icosahedron() -> Mesh {
    const A: f64 = 0.894427190999915878564;
    const B: f64 = 0.447213595499957939282;
    const C72: f64 = 0.309016994374947424102;
    const S72: f64 = 0.951056516295153572116;
    const C36: f64 = 0.809016994374947424102;
    const S36: f64 = 0.587785252292473129169;
    Mesh {
        vertices: vec![
            [0.0, 0.0, 1.0],
            [-A, 0.0, B],
            [-A * C72, -A * S72, B],
            [A * C36, -A * S36, B],
            [A * C36, A * S36, B],
            [-A * C72, A * S72, B],
            [-A * C36, -A * S36, -B],
            [A * C72, -A * S72, -B],
            [A, 0.0, -B],
            [A * C72, A * S72, -B],
            [-A * C36, A * S36, -B],
            [0.0, 0.0, -1.0],
        ],
        faces: [
            [0, 1, 2],
            [0, 2, 3],
            [0, 3, 4],
            [0, 4, 5],
            [0, 5, 1],
            [1, 6, 2],
            [2, 7, 3],
            [3, 8, 4],
            [4, 9, 5],
            [5, 10, 1],
            [6, 7, 2],
            [7, 8, 3],
            [8, 9, 4],
            [9, 10, 5],
            [10, 6, 1],
            [11, 7, 6],
            [11, 8, 7],
            [11, 9, 8],
            [11, 10, 9],
            [11, 6, 10],
        ]
        .iter()
        .map(|f| f.to_vec())
        .collect(),
    }
}

/// `(1 + √5) / 4`, cos 36°: the dodecahedron's constant `A`.
const DODECAHEDRON_A: f64 = 0.809016994374947424102293417183;

/// The regular dodecahedron, V0 at the north pole (`dodecahedron.h`): the north cap, the upper
/// and lower bands of five faces, the south cap.
fn dodecahedron() -> Mesh {
    const A: f64 = DODECAHEDRON_A;
    const B: f64 = 0.577350269189625764509148780502;
    Mesh {
        vertices: vec![
            [0.0, 0.0, 1.0],
            [2.0 / 3.0, 0.0, (4.0 * A - 1.0) / 3.0],
            [(4.0 * A - 1.0) / 3.0, B, 1.0 / 3.0],
            [(2.0 - 2.0 * A) / 3.0, 2.0 * A * B, 1.0 / 3.0],
            [-1.0 / 3.0, B, (4.0 * A - 1.0) / 3.0],
            [-1.0 / 3.0, -B, (4.0 * A - 1.0) / 3.0],
            [(4.0 * A - 1.0) / 3.0, -B, 1.0 / 3.0],
            [(1.0 + 2.0 * A) / 3.0, (2.0 * A - 1.0) * B, -1.0 / 3.0],
            [(2.0 * A - 2.0) / 3.0, 2.0 * A * B, -1.0 / 3.0],
            [-(1.0 + 2.0 * A) / 3.0, (2.0 * A - 1.0) * B, 1.0 / 3.0],
            [(2.0 - 2.0 * A) / 3.0, -2.0 * A * B, 1.0 / 3.0],
            [(1.0 + 2.0 * A) / 3.0, -(2.0 * A - 1.0) * B, -1.0 / 3.0],
            [1.0 / 3.0, B, -(4.0 * A - 1.0) / 3.0],
            [-(4.0 * A - 1.0) / 3.0, B, -1.0 / 3.0],
            [-(1.0 + 2.0 * A) / 3.0, -(2.0 * A - 1.0) * B, 1.0 / 3.0],
            [(2.0 * A - 2.0) / 3.0, -2.0 * A * B, -1.0 / 3.0],
            [1.0 / 3.0, -B, -(4.0 * A - 1.0) / 3.0],
            [0.0, 0.0, -1.0],
            [-2.0 / 3.0, 0.0, -(4.0 * A - 1.0) / 3.0],
            [-(4.0 * A - 1.0) / 3.0, -B, -1.0 / 3.0],
        ],
        faces: [
            [0, 1, 2, 3, 4],
            [3, 8, 13, 9, 4],
            [0, 4, 9, 14, 5],
            [0, 5, 10, 6, 1],
            [1, 6, 11, 7, 2],
            [2, 7, 12, 8, 3],
            [9, 13, 18, 19, 14],
            [5, 14, 19, 15, 10],
            [6, 10, 15, 16, 11],
            [7, 11, 16, 17, 12],
            [8, 12, 17, 18, 13],
            [15, 19, 18, 17, 16],
        ]
        .iter()
        .map(|f| f.to_vec())
        .collect(),
    }
}

/// The unfolding trees: each face's parent (1-based), 0 for the root (`nets/isea/isea.h`).
const NET_ISEA: [usize; 20] = [6, 7, 8, 9, 10, 11, 12, 0, 13, 14, 7, 8, 8, 9, 10, 11, 12, 13, 14, 15];
/// The dodecahedron's nets (`nets/dsea/dsea.h`).
const NET_DSEA: [usize; 12] = [4, 7, 8, 0, 9, 10, 3, 4, 4, 5, 6, 9];
const NET_A5: [usize; 12] = [4, 6, 8, 9, 6, 0, 2, 12, 10, 5, 7, 11];
const NET_CRESCENT: [usize; 12] = [4, 3, 4, 0, 4, 5, 8, 9, 4, 9, 10, 9];
const NET_FLOWER: [usize; 12] = [4, 1, 1, 0, 1, 1, 12, 12, 4, 12, 12, 9];

fn face_centroid(vertices: &[Vec3], face: &[usize]) -> Vec3 {
    let c = face.iter().fold([0.0; 3], |c, &k| add(c, vertices[k]));
    scale(c, 1.0 / face.len() as f64)
}

/// An affine map as PROJ's `Mat4`: the rows of its 3 × 4 part.
type Affine = [[f64; 4]; 2];

fn apply(m: &Affine, v: Vec3) -> Vec3 {
    [m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2] + m[0][3], m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2] + m[1][3], 0.0]
}

/// The rigid map from a face's plane to the plane z = 0 that sends `poly_a` to `net_a` and the
/// direction to `poly_b` along that from `net_a` to `net_b` (`mat4_rigid_transform`).
fn rigid_transform(normal: Vec3, poly_a: Vec3, net_a: Vec3, poly_b: Vec3, net_b: Vec3) -> Affine {
    let u = normalize(sub(poly_b, poly_a));
    let v = cross(normal, u);
    let e = normalize(sub(net_b, net_a));
    let r0 = sub(scale(u, e[0]), scale(v, e[1]));
    let r1 = add(scale(u, e[1]), scale(v, e[0]));
    let t0 = net_a[0] - dot(r0, poly_a);
    let t1 = net_a[1] - dot(r1, poly_a);
    [[r0[0], r0[1], r0[2], t0], [r1[0], r1[1], r1[2], t1]]
}

fn polygon_area_2d(vertices: &[Vec3], face: &[usize]) -> f64 {
    let n = face.len();
    let a: f64 = (0..n).map(|k| cross(vertices[face[k]], vertices[face[(k + 1) % n]])[2]).sum();
    a.abs() * 0.5
}

/// The polyhedron unfolded into a planar net (`unfold_net`): the root face laid with `up` (in
/// the polyhedron's frame) on +y, each other face hinged on its parent's shared edge, the net
/// scaled to an area of 4π with the root face's centroid at the origin.
fn unfold(poly: &Mesh, parents: &[usize], up: Vec3) -> Mesh {
    let nf = poly.faces.len();
    let nfv = poly.faces[0].len();
    let mut vertices: Vec<Vec3> = Vec::new();
    let mut faces = vec![vec![0usize; nfv]; nf];
    let root = parents.iter().position(|&p| p == 0).expect("a net has a root");

    let origin_poly = face_centroid(&poly.vertices, &poly.faces[root]);
    let normal = normalize(origin_poly);
    let step = sub(up, scale(normal, dot(up, normal)));
    let offset_poly = if length(step) < 1e-12 { poly.vertices[poly.faces[root][0]] } else { add(origin_poly, step) };
    let to_plane = rigid_transform(normal, origin_poly, [0.0; 3], offset_poly, [0.0, 1.0, 0.0]);
    for (k, vertex) in faces[root].iter_mut().enumerate() {
        vertices.push(apply(&to_plane, poly.vertices[poly.faces[root][k]]));
        *vertex = vertices.len() - 1;
    }

    let mut queue: Vec<usize> = (0..nf).filter(|&i| parents[i] == root + 1).collect();
    let mut head = 0;
    while head < queue.len() {
        let c = queue[head];
        head += 1;
        let p = parents[c] - 1;
        // The shared edge: the first two vertices the faces have in common.
        let mut shared = Vec::new();
        for kc in 0..nfv {
            for kp in 0..nfv {
                if poly.faces[c][kc] == poly.faces[p][kp] && shared.len() < 2 {
                    shared.push((kc, kp));
                }
            }
        }
        let [(ia_c, ia_p), (ib_c, ib_p)] = [shared[0], shared[1]];
        let (na, nb) = (faces[p][ia_p], faces[p][ib_p]);
        let normal_c = normalize(face_centroid(&poly.vertices, &poly.faces[c]));
        let to_plane =
            rigid_transform(normal_c, poly.vertices[poly.faces[c][ia_c]], vertices[na], poly.vertices[poly.faces[c][ib_c]], vertices[nb]);
        for (k, vertex) in faces[c].iter_mut().enumerate() {
            *vertex = if k == ia_c {
                na
            } else if k == ib_c {
                nb
            } else {
                vertices.push(apply(&to_plane, poly.vertices[poly.faces[c][k]]));
                vertices.len() - 1
            };
        }
        queue.extend((0..nf).filter(|&i| parents[i] == c + 1));
    }

    let total: f64 = faces.iter().map(|f| polygon_area_2d(&vertices, f)).sum();
    let s = (4.0 * PI / total).sqrt();
    let centroid = face_centroid(&vertices, &faces[root]);
    let vertices = vertices.into_iter().map(|v| scale(sub(v, centroid), s)).collect();
    Mesh { vertices, faces }
}

/// Conway's meta operation (`conway_meta`): each face of n vertices split into 2n triangles
/// around its centre, as (centre, edge midpoint, vertex), or (vertex, midpoint, centre) for the
/// dual. On the sphere (`sphere`) the points are normalized onto it; in the plane, the centre is
/// the vertices' mean.
fn conway_meta(mesh: &Mesh, sphere: bool, dual: bool) -> Vec<[Vec3; 3]> {
    let mut out = Vec::new();
    for face in &mesh.faces {
        let n = face.len();
        let mut fan = vec![[0.0; 3]; 2 * n];
        let mut center = [0.0; 3];
        for k in 0..n {
            let (va, vb) = (mesh.vertices[face[k]], mesh.vertices[face[(k + 1) % n]]);
            fan[2 * k] = va;
            fan[2 * k + 1] = lerp(va, vb, 0.5);
            center = add(center, va);
        }
        if sphere {
            fan.iter_mut().for_each(|v| *v = normalize(*v));
            center = normalize(center);
        } else {
            center = scale(center, 1.0 / n as f64);
        }
        for k in 0..2 * n {
            let even = k % 2 == 0;
            let vertex = fan[if even { k } else { (k + 1) % (2 * n) }];
            let midpoint = fan[if even { (k + 1) % (2 * n) } else { k }];
            out.push(if dual { [vertex, midpoint, center] } else { [center, midpoint, vertex] });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The projection (`state.h`, `polyhedral.cpp`).

/// Where the projected origin lies without `+lat_0` or `+lon_0`.
#[derive(Clone, Copy, PartialEq)]
enum DefaultOrigin {
    FaceCentroid,
    FaceBboxCenter,
}

/// A polyhedron's default orientation: the authalic latitude and the longitude brought to the
/// north pole, and the azimuth about it, in degrees.
struct Defaults {
    orient_lat: f64,
    orient_lon: f64,
    azi: f64,
    origin: DefaultOrigin,
}

#[derive(Debug)]
pub(super) struct Polyhedral {
    sph_tris: Vec<SphericalTriangle>,
    face_tris: Vec<FaceTriangle>,
    orient: [[f64; 3]; 3],
    x_offset: f64,
    y_offset: f64,
    /// `None` on a sphere.
    authalic: Option<Authalic>,
    /// The authalic radius over the semi-major axis (1 on a sphere).
    rq: f64,
}

/// A PROJ boolean (`pj_param`'s `b`): true when empty or from `t`, false from `f`, else invalid.
fn boolean(s: &Setup<'_>, key: &str) -> Result<bool> {
    if !s.p.has(key) {
        return Ok(false);
    }
    match s.p.str(key).unwrap_or("").chars().next() {
        None | Some('t' | 'T') => Ok(true),
        Some('f' | 'F') => Ok(false),
        Some(_) => Err(err(&format!("isea: invalid value for {key}"))),
    }
}

/// Whether PROJ's CRS parser takes a value of a `PROJ isea` method: a string (from a letter), a
/// flag, or else a whole angle (`dmstor`), so that `30x` and `1e400` are refused.
/// A WKT's `PROJ4` extension skips that parser: PROJ reads it as a pipeline step.
fn readable(s: &Setup<'_>, v: Option<&str>) -> bool {
    match v {
        _ if s.p.is_extension() => true,
        None | Some("") => true,
        Some(v) if v.starts_with(|c: char| c.is_ascii_alphabetic()) => true,
        Some(v) => crate::params::parse_dms(v).is_some(),
    }
}

/// A number the projection reads with `pj_param`'s `d` (C's `atof`: `10d30` is 10, `abc` 0), in
/// degrees; `None` without the parameter.
fn number(s: &Setup<'_>, key: &str) -> Result<Option<f64>> {
    if !s.p.has(key) {
        return Ok(None);
    }
    let v = s.p.str(key);
    if !readable(s, v) {
        return Err(err(&format!("isea: invalid value for {key}")));
    }
    Ok(Some(v.and_then(crate::def::leading_number).unwrap_or(0.0)))
}

/// An angle the projection reads with `pj_param`'s `r` (`dmstor`), in radians; `None` without the
/// parameter, 0 for a string. In a WKT's `PROJ4` extension, its readable start (`30x` is 30°), as
/// PROJ reads a pipeline step's.
fn dms(s: &Setup<'_>, key: &str) -> Result<Option<f64>> {
    if !s.p.has(key) {
        return Ok(None);
    }
    let v = s.p.str(key);
    let invalid = || err(&format!("isea: invalid value for {key}"));
    if !readable(s, v) {
        return Err(invalid());
    }
    let parse = if s.p.is_extension() { crate::params::parse_dms_prefix } else { crate::params::parse_dms };
    let degrees = v.and_then(parse).unwrap_or(0.0);
    if !degrees.is_finite() {
        return Err(invalid());
    }
    Ok(Some(degrees * DEG_TO_RAD))
}

impl Polyhedral {
    /// `+proj=isea`: the icosahedron family, or the dodecahedron family with `+dual`.
    pub(super) fn isea(s: &mut Setup<'_>) -> Result<Self> {
        // The authalic latitude, before the parameters: `+orient_lat` is converted with it.
        let authalic = Authalic::of(s.ell);
        let rq = authalic.as_ref().map_or(1.0, |a| (0.5 * a.qp).sqrt());
        let (mesh, parents, defaults, dual): (Mesh, Vec<usize>, Defaults, bool) = if boolean(s, "dual")? {
            // `dodecahedron_family_setup(P, true)`, with its `+net`. It does not read `+orient`,
            // which PROJ then ignores, whatever its value.
            let (parents, orient_lon) = match s.p.str("net") {
                _ if !s.p.has("net") => (NET_DSEA.to_vec(), -36.0),
                Some("dsea") => (NET_DSEA.to_vec(), -36.0),
                Some("a5") => (NET_A5.to_vec(), -36.0 - 93.0),
                Some("crescent") => (NET_CRESCENT.to_vec(), -36.0),
                Some("flower") => (NET_FLOWER.to_vec(), -36.0),
                _ => return Err(err("isea: invalid +net (expected dsea, a5, crescent, or flower)")),
            };
            // PROJ: orient_lat = atan((1+2A)/2) places V0 on the F1 axis.
            let orient_lat = ((1.0 + 2.0 * DODECAHEDRON_A) / 2.0).atan() * 180.0 / PI;
            let d = Defaults { orient_lat, orient_lon, azi: 240.0, origin: DefaultOrigin::FaceCentroid };
            (dodecahedron(), parents, d, true)
        } else {
            // Snyder's orientation: V0 at the authalic latitude arctan(φ), φ the golden ratio
            // (PROJ's 58.282525588539, used as authalic; only a user's +orient_lat is geodetic and
            // converted); orient_lon 11.25° on a sphere and 11.20° on an ellipsoid, as PROJ,
            // matches Snyder's Figure 12.
            let (mut orient_lat, mut orient_lon, mut azi) = (58.282525588539, if s.ell.es == 0.0 { 11.25 } else { 11.20 }, 0.0);
            if s.p.has("orient") {
                match s.p.str("orient") {
                    Some("isea") => {}
                    Some("pole") => (orient_lat, orient_lon, azi) = (90.0, 0.0, 0.0),
                    _ => return Err(err("isea: invalid +orient (expected isea or pole)")),
                }
            }
            let d = Defaults { orient_lat, orient_lon, azi, origin: DefaultOrigin::FaceBboxCenter };
            (icosahedron(), NET_ISEA.to_vec(), d, false)
        };

        // `load_meshes`: the net unfolded with geographic north up, both split into triangles.
        let up = {
            let (lat, az) = (defaults.orient_lat * DEG_TO_RAD, defaults.azi * DEG_TO_RAD);
            [-az.cos() * lat.cos(), -az.sin() * lat.cos(), lat.sin()]
        };
        let net = unfold(&mesh, &parents, up);
        let sph = conway_meta(&mesh, true, dual);
        let face = conway_meta(&net, false, dual);
        let mut q = Polyhedral {
            sph_tris: sph,
            face_tris: face.iter().map(|t| t.map(|v| (v[0], v[1]))).collect(),
            orient: [[0.0; 3]; 3],
            x_offset: 0.0,
            y_offset: 0.0,
            authalic,
            rq,
        };

        // `apply_polyhedral_params`: the orientation, a geodetic `+orient_lat` made authalic.
        let mut orient_lat = defaults.orient_lat;
        if let Some(lat) = number(s, "orient_lat")? {
            orient_lat = match &q.authalic {
                Some(a) => a.forward(lat * DEG_TO_RAD) / DEG_TO_RAD,
                None => lat,
            };
        }
        let orient_lon = number(s, "orient_lon")?.unwrap_or(defaults.orient_lon);
        let azi = number(s, "azi")?.unwrap_or(defaults.azi);
        q.set_orientation(orient_lat, orient_lon, azi);

        // `+lat_0` and `+lon_0` name the point projected to the origin, through the orientation,
        // not a central meridian: PROJ's core shifts nothing.
        let (lat_0, lon_0) = (dms(s, "lat_0")?, dms(s, "lon_0")?);
        s.lam0 = 0.0;
        s.phi0 = 0.0;
        if lat_0.is_some() || lon_0.is_some() {
            let v = q.to_cart(lon_0.unwrap_or(0.0), lat_0.unwrap_or(0.0));
            let i = q
                .find_triangle(v)
                .ok_or_else(|| err("isea: +lat_0 / +lon_0 lies outside the polyhedron's coverage; cannot be used as projected origin"))?;
            (q.x_offset, q.y_offset) = snyder_fwd(v, &q.sph_tris[i], &q.face_tris[i]);
        } else {
            let root = parents.iter().position(|&p| p == 0).unwrap_or(0);
            (q.x_offset, q.y_offset) = q.reference_point(root, mesh.faces[0].len(), defaults.origin);
        }
        Ok(q)
    }

    /// The rotation taking (lat, lon) (authalic, degrees) to the north pole, then the azimuth
    /// about it (`set_orient_from_angles`).
    fn set_orientation(&mut self, lat: f64, lon: f64, az: f64) {
        let (lat, lon, az) = (lat * DEG_TO_RAD, lon * DEG_TO_RAD, az * DEG_TO_RAD);
        let alpha = HALFPI - lat;
        let (ca, sa) = (alpha.cos(), alpha.sin());
        let (cl, sl) = (lon.cos(), lon.sin());
        let (cz, sz) = (az.cos(), az.sin());
        let (r00, r01, r02) = (ca * cl, ca * sl, -sa);
        let (r10, r11, r12) = (-sl, cl, 0.0);
        let (r20, r21, r22) = (sa * cl, sa * sl, ca);
        self.orient = [
            [cz * r00 - sz * r10, cz * r01 - sz * r11, cz * r02 - sz * r12],
            [sz * r00 + cz * r10, sz * r01 + cz * r11, sz * r02 + cz * r12],
            [r20, r21, r22],
        ];
    }

    /// The centroid or bounding-box centre of a face of the net (`face_reference_point`).
    fn reference_point(&self, face: usize, vertices_per_face: usize, kind: DefaultOrigin) -> Face2D {
        let fan = 2 * vertices_per_face;
        let corners: Vec<Face2D> = self.face_tris[fan * face..fan * (face + 1)].iter().flatten().copied().collect();
        let (mut sx, mut sy) = (0.0, 0.0);
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
        for &(x, y) in &corners {
            sx += x;
            sy += y;
            if x < min_x {
                min_x = x;
            }
            if x > max_x {
                max_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if y > max_y {
                max_y = y;
            }
        }
        if kind == DefaultOrigin::FaceBboxCenter {
            return (0.5 * (min_x + max_x), 0.5 * (min_y + max_y));
        }
        let n = corners.len() as f64;
        (sx / n, sy / n)
    }

    fn find_triangle(&self, v: Vec3) -> Option<usize> {
        self.sph_tris.iter().position(|&[a, b, c]| {
            if dot(v, a) <= 0.0 {
                return false;
            }
            let (t1, t2, t3) = (triple_product(v, a, b), triple_product(v, b, c), triple_product(v, c, a));
            (t1 >= 0.0 && t2 >= 0.0 && t3 >= 0.0) || (t1 <= 0.0 && t2 <= 0.0 && t3 <= 0.0)
        })
    }

    /// A geodetic point on the oriented unit (authalic) sphere (`lonlat_to_cart`).
    fn to_cart(&self, lam: f64, phi: f64) -> Vec3 {
        let lat = match &self.authalic {
            Some(a) => a.forward(phi),
            None => phi,
        };
        let (ca, sa) = (lat.cos(), lat.sin());
        let (cl, sl) = (lam.cos(), lam.sin());
        let g = [ca * cl, ca * sl, sa];
        let m = &self.orient;
        [dot(m[0], g), dot(m[1], g), dot(m[2], g)]
    }
}

impl Proj for Polyhedral {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let v = self.to_cart(lam, phi);
        let i = self.find_triangle(v)?;
        let (x, y) = snyder_fwd(v, &self.sph_tris[i], &self.face_tris[i]);
        Some(((x - self.x_offset) * self.rq, (y - self.y_offset) * self.rq))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let point = (x / self.rq + self.x_offset, y / self.rq + self.y_offset);
        let i = self.face_tris.iter().position(|t| face_to_barycentric(point, t).iter().all(|&b| b >= -1e-10))?;
        let v = snyder_inv(point, &self.face_tris[i], &self.sph_tris[i]);
        // `cart_to_lonlat`, through the inverse (transposed) orientation.
        let m = &self.orient;
        let g = [0, 1, 2].map(|c| m[0][c] * v[0] + m[1][c] * v[1] + m[2][c] * v[2]);
        if g[2].abs() > 1.0 + 1e-14 {
            return None;
        }
        let lat = g[2].clamp(-1.0, 1.0).asin();
        let phi = match &self.authalic {
            Some(a) => a.inverse(lat),
            None => lat,
        };
        // PROJ's `aatan2`: 0 at the origin.
        let lam = if g[1].abs() < 1e-50 && g[0].abs() < 1e-50 { 0.0 } else { g[1].atan2(g[0]) };
        Some((adjlon(lam), phi))
    }
}
