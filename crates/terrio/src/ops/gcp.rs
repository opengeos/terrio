//! Georeferencing by ground control points, as GDAL's GCP transformers compute it: polynomials of
//! order 1 to 3 fitted by least squares (`GDALCreateGCPTransformer`, from GRASS's `CRS.c`), and
//! thin plate splines (`GDALCreateTPSTransformer`), each fitted both ways (pixel to georeferenced
//! coordinates, and back).
//!
//! ```
//! use terrio::Gcp;
//! use terrio::ops::gcp::GcpTransformer;
//!
//! let gcp = |pixel: f64, line: f64, x: f64, y: f64| Gcp { pixel, line, x, y, ..Default::default() };
//! let gcps = [gcp(0.0, 0.0, 100.0, 50.0), gcp(10.0, 0.0, 110.0, 50.0), gcp(0.0, 10.0, 100.0, 40.0)];
//! let t = GcpTransformer::polynomial(&gcps, 1)?;
//! let (x, y) = t.to_geo(5.0, 5.0);
//! assert!((x - 105.0).abs() < 1e-9 && (y - 45.0).abs() < 1e-9);
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::{Error, Gcp, Result};

/// How GCPs georeference a raster, as GDAL's transformer `SRC_METHOD` option names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum GcpMethod {
    /// GDAL's choice: a homography from 4 or 5 GCPs, else a polynomial.
    #[default]
    Auto,
    /// A polynomial (`GCP_POLYNOMIAL`).
    Polynomial,
    /// Thin plate splines (`GCP_TPS`).
    Tps,
    /// A homography (`GCP_HOMOGRAPHY`).
    Homography,
}

impl GcpMethod {
    /// Parses a name: `auto`, `polynomial` (`gcp_polynomial`), `tps` (`gcp_tps`) or
    /// `homography` (`gcp_homography`).
    ///
    /// # Errors
    /// For other names.
    pub fn parse(s: &str) -> Result<Self> {
        let l = s.to_ascii_lowercase();
        Ok(match l.strip_prefix("gcp_").unwrap_or(&l) {
            "auto" | "" => Self::Auto,
            "polynomial" => Self::Polynomial,
            "tps" => Self::Tps,
            "homography" => Self::Homography,
            _ => return Err(Error::InvalidArgument(format!("unknown GCP method '{s}' (auto, polynomial, tps, homography)"))),
        })
    }
}

/// A transformation between pixel/line and georeferenced coordinates defined by GCPs.
#[derive(Debug, Clone)]
pub enum GcpTransformer {
    /// A polynomial of order 1, 2 or 3 each way.
    Polynomial(Box<Polynomial>),
    /// Thin plate splines each way.
    Tps(Box<(Spline, Spline)>),
    /// A homography and its inverse.
    Homography(Box<([f64; 9], [f64; 9])>),
}

impl GcpTransformer {
    /// The transformer GDAL's `GDALCreateGenImgProjTransformer2` picks for GCPs with a method and a
    /// polynomial order (`MAX_GCP_ORDER`, 0 for GDAL's default).
    ///
    /// # Errors
    /// When the GCPs cannot be fitted so.
    pub fn new(gcps: &[Gcp], method: GcpMethod, order: u32) -> Result<Self> {
        match method {
            GcpMethod::Auto if (4..6).contains(&gcps.len()) => Self::homography(gcps),
            GcpMethod::Auto | GcpMethod::Polynomial => Self::polynomial(gcps, order),
            GcpMethod::Tps => Self::tps(gcps),
            GcpMethod::Homography => Self::homography(gcps),
        }
    }

    /// Fits a homography to 4 or more GCPs by least squares (`GDALGCPsToHomography`), as
    /// `gdalwarp` does with 4 or 5 GCPs.
    ///
    /// # Errors
    /// When the GCPs are degenerate, or the homography does not map the image to a convex
    /// quadrilateral.
    pub fn homography(gcps: &[Gcp]) -> Result<Self> {
        let h = homography_from_gcps(gcps)?;
        let inv = invert_homography(&h).ok_or_else(|| Error::InvalidArgument("the homography of the GCPs is not invertible".into()))?;
        Ok(Self::Homography(Box::new((h, inv))))
    }

    /// Fits polynomials of `order` (1, 2 or 3; 0 picks GDAL's default: 2 from 6 GCPs, else 1) by
    /// least squares, as `gdalwarp -order` does.
    ///
    /// # Errors
    /// When there are too few GCPs for the order, or they are colinear.
    pub fn polynomial(gcps: &[Gcp], order: u32) -> Result<Self> {
        Polynomial::new(gcps, order).map(|p| Self::Polynomial(Box::new(p)))
    }

    /// Fits thin plate splines through the GCPs, as `gdalwarp -tps` does.
    ///
    /// # Errors
    /// When there are no GCPs, two GCPs share a position, or the system is degenerate.
    pub fn tps(gcps: &[Gcp]) -> Result<Self> {
        let mut seen: Vec<(f64, f64)> = Vec::new();
        for g in gcps {
            if seen.contains(&(g.pixel, g.line)) {
                return Err(Error::InvalidArgument(format!("two GCPs share the position ({}, {})", g.pixel, g.line)));
            }
            seen.push((g.pixel, g.line));
        }
        let fwd = Spline::fit(gcps.iter().map(|g| ((g.pixel, g.line), (g.x, g.y))).collect())?;
        let inv = Spline::fit(gcps.iter().map(|g| ((g.x, g.y), (g.pixel, g.line))).collect())?;
        Ok(Self::Tps(Box::new((fwd, inv))))
    }

    /// Georeferenced coordinates of a pixel/line position.
    pub fn to_geo(&self, pixel: f64, line: f64) -> (f64, f64) {
        match self {
            Self::Polynomial(p) => p.eval(false, pixel, line),
            Self::Tps(s) => s.0.eval(pixel, line),
            Self::Homography(h) => apply_homography(&h.0, pixel, line),
        }
    }

    /// The pixel/line position of georeferenced coordinates.
    pub fn to_pixel(&self, x: f64, y: f64) -> (f64, f64) {
        match self {
            Self::Polynomial(p) => p.eval(true, x, y),
            Self::Homography(h) => apply_homography(&h.1, x, y),
            Self::Tps(s) => {
                // As GDAL 3.12: the reverse spline's guess, refined by Newton iterations on the
                // forward spline (`GDALGenericInverse2D`).
                let (gx, gy) = s.1.eval(x, y);
                generic_inverse(x, y, gx, gy, |a, b| s.0.eval(a, b))
            }
        }
    }
}

/// Applies a homography (GDAL's 9 coefficients). Non-finite where its denominator vanishes.
fn apply_homography(h: &[f64; 9], a: f64, b: f64) -> (f64, f64) {
    let w = h[6] + a * h[7] + b * h[8];
    if w.abs() < 1e-15 {
        return (f64::NAN, f64::NAN);
    }
    ((h[0] + a * h[1] + b * h[2]) / w, (h[3] + a * h[4] + b * h[5]) / w)
}

/// `GDALInvHomography`.
fn invert_homography(h: &[f64; 9]) -> Option<[f64; 9]> {
    if h[2] == 0.0 && h[4] == 0.0 && h[1] != 0.0 && h[5] != 0.0 && h[7] == 0.0 && h[8] == 0.0 && h[6] != 0.0 {
        return Some([-h[0] / h[1] / h[6], 1.0 / h[1], 0.0, -h[3] / h[5] / h[6], 0.0, 1.0 / h[5], 1.0 / h[6], 0.0, 0.0]);
    }
    let det = h[1] * h[5] * h[6] - h[2] * h[4] * h[6] + h[2] * h[3] * h[7] - h[0] * h[5] * h[7] + h[0] * h[4] * h[8] - h[1] * h[3] * h[8];
    let magnitude = h[1].abs().max(h[2].abs()).max(h[4].abs().max(h[5].abs()));
    if det.abs() <= 1e-10 * magnitude * magnitude {
        return None;
    }
    let i = 1.0 / det;
    let mut o = [0.0; 9];
    o[1] = (h[5] * h[6] - h[3] * h[8]) * i;
    o[4] = (h[3] * h[7] - h[4] * h[6]) * i;
    o[7] = (h[4] * h[8] - h[5] * h[7]) * i;
    o[2] = (h[0] * h[8] - h[2] * h[6]) * i;
    o[5] = (h[1] * h[6] - h[0] * h[7]) * i;
    o[8] = (h[2] * h[7] - h[1] * h[8]) * i;
    o[0] = (h[2] * h[3] - h[0] * h[5]) * i;
    o[3] = (h[0] * h[4] - h[1] * h[3]) * i;
    o[6] = (h[1] * h[5] - h[2] * h[4]) * i;
    Some(o)
}

/// `GDALComposeHomographies`: `h1` then `h2`.
fn compose_homographies(h1: &[f64; 9], h2: &[f64; 9]) -> [f64; 9] {
    let mut w = [0.0; 9];
    for (row, base) in [(0usize, 0usize), (3, 3), (6, 6)] {
        let (a, b, c) = (h2[base + 1], h2[base + 2], h2[base]);
        w[row + 1] = a * h1[1] + b * h1[4] + c * h1[7];
        w[row + 2] = a * h1[2] + b * h1[5] + c * h1[8];
        w[row] = a * h1[0] + b * h1[3] + c * h1[6];
    }
    w
}

/// `GDALGCPsToHomography`: the least squares homography of normalized GCPs.
fn homography_from_gcps(gcps: &[Gcp]) -> Result<[f64; 9]> {
    if gcps.len() < 4 {
        return Err(Error::InvalidArgument(format!("a homography needs 4 GCPs or more, not {}", gcps.len())));
    }
    let range = |f: fn(&Gcp) -> f64| {
        let v: Vec<f64> = gcps.iter().map(f).collect();
        (v.iter().copied().fold(f64::INFINITY, f64::min), v.iter().copied().fold(f64::NEG_INFINITY, f64::max))
    };
    let (p0, p1) = range(|g| g.pixel);
    let (l0, l1) = range(|g| g.line);
    let (x0, x1) = range(|g| g.x);
    let (y0, y1) = range(|g| g.y);
    if [(p0, p1), (l0, l1), (x0, x1), (y0, y1)].iter().any(|(a, b)| (b - a).abs() < 1e-12) {
        return Err(Error::InvalidArgument("the GCPs are degenerate in at least one dimension".into()));
    }
    let pl = [-p0 / (p1 - p0), 1.0 / (p1 - p0), 0.0, -l0 / (l1 - l0), 0.0, 1.0 / (l1 - l0), 1.0, 0.0, 0.0];
    let geo = [-x0 / (x1 - x0), 1.0 / (x1 - x0), 0.0, -y0 / (y1 - y0), 0.0, 1.0 / (y1 - y0), 1.0, 0.0, 0.0];
    let inv_geo = invert_homography(&geo).ok_or_else(|| Error::InvalidArgument("degenerate GCPs".into()))?;
    let mut ata = vec![0.0f64; 81];
    let mut rhs = vec![0.0f64; 9];
    rhs[6] = 1.0;
    ata[6 * 9 + 6] = 1.0;
    for g in gcps {
        let (p, l) = apply_homography(&pl, g.pixel, g.line);
        let (gx, gy) = apply_homography(&geo, g.x, g.y);
        let ax = [1.0, p, l, 0.0, 0.0, 0.0, -gx, -gx * p, -gx * l];
        let ay = [0.0, 0.0, 0.0, 1.0, p, l, -gy, -gy * p, -gy * l];
        for j in 0..9 {
            for k in j..9 {
                ata[j * 9 + k] += ax[j] * ax[k] + ay[j] * ay[k];
            }
        }
    }
    for j in 0..9 {
        for k in 0..j {
            ata[j * 9 + k] = ata[k * 9 + j];
        }
    }
    let sol = solve(ata, rhs, 9, 1).ok_or_else(|| Error::InvalidArgument("the GCPs' homography system is singular".into()))?;
    let hn: [f64; 9] = sol.try_into().map_err(|_| Error::InvalidArgument("homography".into()))?;
    if hn[6].abs() < 1e-15 {
        return Err(Error::InvalidArgument("the GCPs' homography is degenerate".into()));
    }
    // The unit square must map to a convex quadrilateral.
    let mut c = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)].map(|(a, b)| apply_homography(&hn, a, b));
    for i in (0..4).rev() {
        c[i] = (c[i].0 - c[0].0, c[i].1 - c[0].1);
    }
    let cross12 = c[1].0 * c[2].1 - c[2].0 * c[1].1;
    let cross23 = c[2].0 * c[3].1 - c[3].0 * c[2].1;
    if !(cross12 * cross23 > 0.0) {
        return Err(Error::InvalidArgument("the GCPs' homography does not map the image to a convex quadrilateral".into()));
    }
    Ok(compose_homographies(&compose_homographies(&pl, &hn), &inv_geo))
}

/// GDAL's `GDALGenericInverse2D`: the input of `forward` mapping to `(x, y)`, by Newton-Raphson
/// iterations from a guess, the Jacobian estimated numerically at the first iteration only, until
/// within a relative 1e-12 of `(x, y)` (at most 15 iterations; the last estimate otherwise).
fn generic_inverse(x: f64, y: f64, gx: f64, gy: f64, forward: impl Fn(f64, f64) -> (f64, f64)) -> (f64, f64) {
    let abs_out = gx.abs().max(gy.abs());
    let eps = if abs_out > 0.0 { abs_out * 1e-6 } else { 1e-6 };
    let abs_in = x.abs().max(y.abs());
    let tol = if abs_in > 0.0 { abs_in * 1e-12 } else { 1e-12 };
    let (mut ox, mut oy) = (gx, gy);
    let (mut lx, mut ly, mut px, mut py) = (0.0, 0.0, 0.0, 0.0);
    for i in 0..15 {
        let (ax, ay) = forward(ox, oy);
        let (dx, dy) = (ax - x, ay - y);
        if dx.abs() < tol && dy.abs() < tol {
            break;
        }
        if i == 0 {
            let (x1, y1) = forward(ox + eps, oy);
            let (xl, yl) = ((x1 - ax) / eps, (y1 - ay) / eps);
            let (x2, y2) = forward(ox, oy + eps);
            let (xp, yp) = ((x2 - ax) / eps, (y2 - ay) / eps);
            let det = xl * yp - xp * yl;
            if det == 0.0 {
                break;
            }
            (lx, ly, px, py) = (yp / det, -xp / det, -yl / det, xl / det);
        }
        ox -= dx * lx + dy * ly;
        oy -= dx * px + dy * py;
    }
    (ox, oy)
}

/// GDAL's GCP polynomials: coefficients each way, on coordinates centred on the GCPs' means.
#[derive(Debug, Clone)]
pub struct Polynomial {
    order: u32,
    to_geo: ([f64; 10], [f64; 10]),
    from_geo: ([f64; 10], [f64; 10]),
    pixel_mean: (f64, f64),
    geo_mean: (f64, f64),
}

/// The term `n` (from 1) of a polynomial, in GRASS's order: 1, e, n, e², en, n², e³, e²n, en², n³.
fn term(n: usize, e: f64, nn: f64) -> f64 {
    match n {
        1 => 1.0,
        2 => e,
        3 => nn,
        4 => e * e,
        5 => e * nn,
        6 => nn * nn,
        7 => e * e * e,
        8 => e * e * nn,
        9 => e * nn * nn,
        10 => nn * nn * nn,
        _ => 0.0,
    }
}

impl Polynomial {
    fn new(gcps: &[Gcp], order: u32) -> Result<Self> {
        let order = match order {
            0 if gcps.len() >= 6 => 2,
            0 => 1,
            o @ 1..=3 => o,
            o => return Err(Error::InvalidArgument(format!("the GCP polynomial order must be 1, 2 or 3, not {o}"))),
        };
        let mut pts: Vec<(f64, f64, f64, f64)> = gcps.iter().map(|g| (g.pixel, g.line, g.x, g.y)).collect();
        // Two GCPs at opposite corners of a rectangle: GDAL adds a third corner.
        if pts.len() == 2 && order == 1 && pts[0].2 != pts[1].2 && pts[0].3 != pts[1].3 {
            pts.push((pts[1].0, pts[0].1, pts[1].2, pts[0].3));
        }
        if pts.is_empty() {
            return Err(Error::InvalidArgument("no GCPs".into()));
        }
        let n = pts.len() as f64;
        let mean = |f: fn(&(f64, f64, f64, f64)) -> f64| pts.iter().map(f).sum::<f64>() / n;
        let pixel_mean = (mean(|p| p.0), mean(|p| p.1));
        let geo_mean = (mean(|p| p.2), mean(|p| p.3));
        let fwd: Vec<_> = pts.iter().map(|p| (p.0, p.1, p.2, p.3)).collect();
        let inv: Vec<_> = pts.iter().map(|p| (p.2, p.3, p.0, p.1)).collect();
        let to_geo = coefficients(&fwd, pixel_mean, order)?;
        let from_geo = coefficients(&inv, geo_mean, order)?;
        Ok(Self { order, to_geo, from_geo, pixel_mean, geo_mean })
    }

    fn eval(&self, inverse: bool, a: f64, b: f64) -> (f64, f64) {
        let (c, m) = if inverse { (&self.from_geo, self.geo_mean) } else { (&self.to_geo, self.pixel_mean) };
        let (e, n) = (a - m.0, b - m.1);
        let k = ((self.order + 1) * (self.order + 2) / 2) as usize;
        let (mut x, mut y) = (0.0, 0.0);
        for t in 1..=k {
            let v = term(t, e, n);
            x += c.0[t - 1] * v;
            y += c.1[t - 1] * v;
        }
        (x, y)
    }
}

/// The coefficients mapping `(e1, n1)` to `(e2, n2)`: exactly with as many points as terms, else
/// by least squares (GRASS's `calccoef`).
fn coefficients(pts: &[(f64, f64, f64, f64)], mean: (f64, f64), order: u32) -> Result<([f64; 10], [f64; 10])> {
    let m = ((order + 1) * (order + 2) / 2) as usize;
    if pts.len() < m {
        return Err(Error::InvalidArgument(format!("{} GCPs are too few for a polynomial of order {order} (at least {m})", pts.len())));
    }
    let mut mat = vec![vec![0.0f64; m]; m];
    let mut a = vec![0.0f64; m];
    let mut b = vec![0.0f64; m];
    if pts.len() == m {
        for (row, p) in pts.iter().enumerate() {
            for j in 0..m {
                mat[row][j] = term(j + 1, p.0 - mean.0, p.1 - mean.1);
            }
            a[row] = p.2;
            b[row] = p.3;
        }
    } else {
        for p in pts {
            let (e, n) = (p.0 - mean.0, p.1 - mean.1);
            for i in 0..m {
                for j in i..m {
                    mat[i][j] += term(i + 1, e, n) * term(j + 1, e, n);
                }
                a[i] += p.2 * term(i + 1, e, n);
                b[i] += p.3 * term(i + 1, e, n);
            }
        }
        for i in 1..m {
            for j in 0..i {
                mat[i][j] = mat[j][i];
            }
        }
    }
    // Gauss-Jordan elimination with partial pivoting (GRASS's `solvemat`).
    for i in 0..m {
        let mut pivot = mat[i][i];
        let mut mark = i;
        for (i2, row) in mat.iter().enumerate().skip(i + 1) {
            if row[i].abs() > pivot.abs() {
                pivot = row[i];
                mark = i2;
            }
        }
        if pivot == 0.0 {
            return Err(Error::InvalidArgument("the GCPs are colinear: no polynomial fits them".into()));
        }
        if mark != i {
            mat.swap(mark, i);
            a.swap(mark, i);
            b.swap(mark, i);
        }
        for i2 in 0..m {
            if i2 != i {
                let factor = mat[i2][i] / pivot;
                for j2 in i..m {
                    mat[i2][j2] -= factor * mat[i][j2];
                }
                a[i2] -= factor * a[i];
                b[i2] -= factor * b[i];
            }
        }
    }
    let (mut e, mut n) = ([0.0; 10], [0.0; 10]);
    for i in 0..m {
        e[i] = a[i] / mat[i][i];
        n[i] = b[i] / mat[i][i];
    }
    Ok((e, n))
}

/// A thin plate spline from 2-D points to 2-D values: GDAL's `VizGeorefSpline2D`.
#[derive(Debug, Clone)]
pub struct Spline {
    kind: SplineKind,
    /// The points (centred on their mean for the full spline).
    x: Vec<f64>,
    y: Vec<f64>,
    values: Vec<(f64, f64)>,
    mean: (f64, f64),
    /// The coefficients of each variable: 3 affine terms, then one per point.
    coef: [Vec<f64>; 2],
    /// The direction of a one-dimensional spline, and the points' order along it.
    dir: (f64, f64),
    u: Vec<f64>,
    order: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SplineKind {
    One,
    Two,
    OneDimensional,
    Full,
}

/// The thin plate spline radial basis, `r² ln r²`.
fn base(x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    let d = (x2 - x1) * (x2 - x1) + (y2 - y1) * (y2 - y1);
    if d != 0.0 { d * d.ln() } else { 0.0 }
}

impl Spline {
    fn fit(points: Vec<((f64, f64), (f64, f64))>) -> Result<Self> {
        let n = points.len();
        let x: Vec<f64> = points.iter().map(|p| p.0.0).collect();
        let y: Vec<f64> = points.iter().map(|p| p.0.1).collect();
        let values: Vec<(f64, f64)> = points.iter().map(|p| p.1).collect();
        let mut s = Self {
            kind: SplineKind::One,
            x,
            y,
            values,
            mean: (0.0, 0.0),
            coef: [Vec::new(), Vec::new()],
            dir: (0.0, 0.0),
            u: Vec::new(),
            order: Vec::new(),
        };
        match n {
            0 => return Err(Error::InvalidArgument("no GCPs".into())),
            1 => return Ok(s),
            2 => {
                let (dx, dy) = (s.x[1] - s.x[0], s.y[1] - s.y[0]);
                let denom = dx * dx + dy * dy;
                if denom == 0.0 {
                    return Err(Error::InvalidArgument("degenerate GCPs".into()));
                }
                s.dir = (dx / denom, dy / denom);
                s.kind = SplineKind::Two;
                return Ok(s);
            }
            _ => {}
        }
        let nf = n as f64;
        let (mut xmin, mut xmax, mut ymin, mut ymax) = (s.x[0], s.x[0], s.y[0], s.y[0]);
        let (mut sx, mut sy, mut sx2, mut sy2, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for p in 0..n {
            let (xx, yy) = (s.x[p], s.y[p]);
            xmax = xmax.max(xx);
            xmin = xmin.min(xx);
            ymax = ymax.max(yy);
            ymin = ymin.min(yy);
            sx += xx;
            sx2 += xx * xx;
            sy += yy;
            sy2 += yy * yy;
            sxy += xx * yy;
        }
        let (delx, dely) = (xmax - xmin, ymax - ymin);
        let ssxx = sx2 - sx * sx / nf;
        let ssyy = sy2 - sy * sy / nf;
        let ssxy = sxy - sx * sy / nf;
        if ssxx * ssyy == 0.0 {
            return Err(Error::InvalidArgument("degenerate GCPs: thin plate splines cannot be fitted".into()));
        }
        if delx < 0.001 * dely || dely < 0.001 * delx || (ssxy * ssxy / (ssxx * ssyy)).abs() > 0.99 {
            s.kind = SplineKind::OneDimensional;
            let (dx, dy) = (nf * sx2 - sx * sx, nf * sy2 - sy * sy);
            let fact = 1.0 / (dx * dx + dy * dy).sqrt();
            s.dir = (dx * fact, dy * fact);
            s.u = (0..n).map(|p| s.dir.0 * (s.x[p] - s.x[0]) + s.dir.1 * (s.y[p] - s.y[0])).collect();
            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by(|a, b| s.u[*a].total_cmp(&s.u[*b]));
            s.order = order;
            return Ok(s);
        }
        s.kind = SplineKind::Full;
        let k = n + 3;
        s.mean = (sx / nf, sy / nf);
        for p in 0..n {
            s.x[p] -= s.mean.0;
            s.y[p] -= s.mean.1;
        }
        let mut a = vec![0.0f64; k * k];
        for c in 0..n {
            a[c + 3] = 1.0;
            a[k + c + 3] = s.x[c];
            a[2 * k + c + 3] = s.y[c];
            a[(c + 3) * k] = 1.0;
            a[(c + 3) * k + 1] = s.x[c];
            a[(c + 3) * k + 2] = s.y[c];
        }
        for r in 0..n {
            for c in r..n {
                let v = base(s.x[r], s.y[r], s.x[c], s.y[c]);
                a[(r + 3) * k + c + 3] = v;
                a[(c + 3) * k + r + 3] = v;
            }
        }
        let mut rhs = vec![0.0f64; k * 2];
        for p in 0..n {
            rhs[(p + 3) * 2] = s.values[p].0;
            rhs[(p + 3) * 2 + 1] = s.values[p].1;
        }
        let sol = solve(a, rhs, k, 2).ok_or_else(|| Error::InvalidArgument("the thin plate spline system is singular".into()))?;
        s.coef = [(0..k).map(|r| sol[r * 2]).collect(), (0..k).map(|r| sol[r * 2 + 1]).collect()];
        Ok(s)
    }

    fn eval(&self, px: f64, py: f64) -> (f64, f64) {
        let n = self.values.len();
        match self.kind {
            SplineKind::One => self.values[0],
            SplineKind::Two => {
                let f = self.dir.0 * (px - self.x[0]) + self.dir.1 * (py - self.y[0]);
                let (a, b) = (self.values[0], self.values[1]);
                ((1.0 - f) * a.0 + f * b.0, (1.0 - f) * a.1 + f * b.1)
            }
            SplineKind::OneDimensional => {
                let pu = self.dir.0 * (px - self.x[0]) + self.dir.1 * (py - self.y[0]);
                let o = &self.order;
                let (l, r) = if pu <= self.u[o[0]] {
                    (o[0], o[1])
                } else if pu >= self.u[o[n - 1]] {
                    (o[n - 2], o[n - 1])
                } else {
                    let mut pair = (o[0], o[1]);
                    for i in 1..n {
                        pair = (o[i - 1], o[i]);
                        if pu >= self.u[pair.0] && pu <= self.u[pair.1] {
                            break;
                        }
                    }
                    pair
                };
                let f = (pu - self.u[l]) / (self.u[r] - self.u[l]);
                let (a, b) = (self.values[l], self.values[r]);
                ((1.0 - f) * a.0 + f * b.0, (1.0 - f) * a.1 + f * b.1)
            }
            SplineKind::Full => {
                let (x, y) = (px - self.mean.0, py - self.mean.1);
                let [c0, c1] = &self.coef;
                let mut v = (c0[0] + c0[1] * x + c0[2] * y, c1[0] + c1[1] * x + c1[2] * y);
                for r in 0..n {
                    let t = base(x, y, self.x[r], self.y[r]);
                    v.0 += c0[r + 3] * t;
                    v.1 += c1[r + 3] * t;
                }
                v
            }
        }
    }
}

/// Solves `A X = B` (`A` is `n` x `n`, `B` is `n` x `m`, row-major) by Gaussian elimination with
/// partial pivoting. `None` when `A` is singular.
fn solve(mut a: Vec<f64>, mut b: Vec<f64>, n: usize, m: usize) -> Option<Vec<f64>> {
    for col in 0..n {
        let piv = (col..n).max_by(|&i, &j| a[i * n + col].abs().total_cmp(&a[j * n + col].abs()))?;
        if a[piv * n + col] == 0.0 {
            return None;
        }
        if piv != col {
            for j in 0..n {
                a.swap(piv * n + j, col * n + j);
            }
            for j in 0..m {
                b.swap(piv * m + j, col * m + j);
            }
        }
        let d = a[col * n + col];
        for r in col + 1..n {
            let f = a[r * n + col] / d;
            if f == 0.0 {
                continue;
            }
            for j in col..n {
                a[r * n + j] -= f * a[col * n + j];
            }
            for j in 0..m {
                b[r * m + j] -= f * b[col * m + j];
            }
        }
    }
    let mut x = vec![0.0f64; n * m];
    for r in (0..n).rev() {
        for j in 0..m {
            let mut s = b[r * m + j];
            for k in r + 1..n {
                s -= a[r * n + k] * x[k * m + j];
            }
            x[r * m + j] = s / a[r * n + r];
        }
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gcp(pixel: f64, line: f64, x: f64, y: f64) -> Gcp {
        Gcp { pixel, line, x, y, ..Default::default() }
    }

    #[test]
    fn polynomials_and_splines_interpolate_an_affine_grid() {
        let gcps: Vec<Gcp> = (0..4)
            .flat_map(|i| (0..4).map(move |j| (i as f64 * 10.0, j as f64 * 10.0)))
            .map(|(p, l)| gcp(p, l, 1000.0 + 2.0 * p + 0.5 * l, 500.0 - 2.0 * l + 0.1 * p))
            .collect();
        for t in [
            GcpTransformer::polynomial(&gcps, 1).unwrap(),
            GcpTransformer::polynomial(&gcps, 3).unwrap(),
            GcpTransformer::tps(&gcps).unwrap(),
        ] {
            let (x, y) = t.to_geo(15.0, 7.0);
            assert!((x - (1000.0 + 30.0 + 3.5)).abs() < 1e-6 && (y - (500.0 - 14.0 + 1.5)).abs() < 1e-6, "{x} {y}");
            let (p, l) = t.to_pixel(x, y);
            assert!((p - 15.0).abs() < 1e-6 && (l - 7.0).abs() < 1e-6, "{p} {l}");
        }
        assert!(GcpTransformer::polynomial(&gcps[..5], 2).is_err());
    }
}
