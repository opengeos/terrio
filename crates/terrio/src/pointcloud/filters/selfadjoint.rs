// SPDX-FileCopyrightText: Copyright (C) 2008-2010 Gael Guennebaud <gael.guennebaud@inria.fr>
// SPDX-FileCopyrightText: Copyright (C) 2010 Jitse Niesen <jitse@maths.leeds.ac.uk>
// SPDX-FileCopyrightText: Copyright (C) 2008 Gael Guennebaud <gael.guennebaud@inria.fr>
// SPDX-FileCopyrightText: Copyright (C) 2009 Benoit Jacob <jacob.benoit.1@gmail.com>
// SPDX-FileCopyrightText: Copyright (C) 2009 Gael Guennebaud <gael.guennebaud@inria.fr>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MPL-2.0
//
// Derived from (translated to Rust and modified):
//   Eigen 3.4.0:
//     Eigen/src/Eigenvalues/SelfAdjointEigenSolver.h
//     Eigen/src/Eigenvalues/Tridiagonalization.h
//     Eigen/src/Jacobi/Jacobi.h

// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a
// copy of the MPL was not distributed with this file, You can obtain one at
// https://mozilla.org/MPL/2.0/.

//! A port of Eigen 3.4's eigensolver for symmetric 3x3 matrices, which PDAL's covariance-based
//! filters use, kept in its own file because Eigen is licensed under the MPL-2.0: the rest of
//! terrio is MIT-licensed.

/// The eigenvalues (ascending) and eigenvectors (columns, in the same order) of a symmetric 3x3
/// matrix, as Eigen 3.4's `SelfAdjointEigenSolver<Matrix3d>` (which PDAL uses) computes them, in
/// its order of operations: the matrix scaled by its largest coefficient, tridiagonalized by one
/// Householder reflection, then diagonalized by implicit QR steps with Wilkinson shifts.
pub(super) fn eigen_selfadjoint(m: [[f64; 3]; 3]) -> ([f64; 3], [[f64; 3]; 3]) {
    // Eigen reads the lower triangle.
    let lower = [m[0][0], m[1][0], m[1][1], m[2][0], m[2][1], m[2][2]];
    let mut scale = lower.iter().fold(0.0f64, |a, v| if v.abs() > a { v.abs() } else { a });
    if scale == 0.0 {
        scale = 1.0;
    }
    let a = |r: usize, c: usize| m[r][c] / scale;
    let mut diag = [a(0, 0), 0.0, 0.0];
    let mut sub: [f64; 2];
    // The eigenvectors, column-major as Eigen stores them: q[col][row].
    let mut q = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let v1norm2 = a(2, 0) * a(2, 0);
    if v1norm2 <= f64::MIN_POSITIVE {
        diag[1] = a(1, 1);
        diag[2] = a(2, 2);
        sub = [a(1, 0), a(2, 1)];
    } else {
        let beta = (a(1, 0) * a(1, 0) + v1norm2).sqrt();
        let inv_beta = 1.0 / beta;
        let m01 = a(1, 0) * inv_beta;
        let m02 = a(2, 0) * inv_beta;
        let qq = 2.0 * m01 * a(2, 1) + m02 * (a(2, 2) - a(1, 1));
        diag[1] = a(1, 1) + m02 * qq;
        diag[2] = a(2, 2) - m02 * qq;
        sub = [beta, a(2, 1) - m01 * qq];
        q = [[1.0, 0.0, 0.0], [0.0, m01, m02], [0.0, m02, -m01]];
    }
    let n = 3usize;
    let mut end = n - 1;
    let mut start = 0usize;
    let mut iter = 0usize;
    let precision_inv = 1.0 / f64::EPSILON;
    while end > 0 {
        for i in start..end {
            if sub[i].abs() < f64::MIN_POSITIVE {
                sub[i] = 0.0;
            } else {
                let s = precision_inv * sub[i];
                if s * s <= diag[i].abs() + diag[i + 1].abs() {
                    sub[i] = 0.0;
                }
            }
        }
        while end > 0 && sub[end - 1] == 0.0 {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        iter += 1;
        if iter > 30 * n {
            break;
        }
        start = end - 1;
        while start > 0 && sub[start - 1] != 0.0 {
            start -= 1;
        }
        qr_step(&mut diag, &mut sub, start, end, &mut q);
    }
    // Sort, as Eigen does: swap each place with the first smallest after it.
    for i in 0..n - 1 {
        let mut k = 0;
        for j in 1..n - i {
            if diag[i + j] < diag[i + k] {
                k = j;
            }
        }
        if k > 0 {
            diag.swap(i, i + k);
            q.swap(i, i + k);
        }
    }
    let vals = diag.map(|d| d * scale);
    let mut vecs = [[0.0; 3]; 3];
    for (col, v) in q.iter().enumerate() {
        for row in 0..3 {
            vecs[row][col] = v[row];
        }
    }
    (vals, vecs)
}

/// Eigen's `tridiagonal_qr_step`: one implicit symmetric QR step with a Wilkinson shift on
/// `diag[start..=end]`, the rotations applied to the columns of `q`.
fn qr_step(diag: &mut [f64; 3], sub: &mut [f64; 2], start: usize, end: usize, q: &mut [[f64; 3]; 3]) {
    let td = (diag[end - 1] - diag[end]) * 0.5;
    let e = sub[end - 1];
    let mut mu = diag[end];
    if td == 0.0 {
        mu -= e.abs();
    } else if e != 0.0 {
        let e2 = e * e;
        // Eigen's hypot: the larger times √(1 + (smaller / larger)²).
        let (p, r) = (td.abs().max(e.abs()), td.abs().min(e.abs()));
        let h = if p == 0.0 { 0.0 } else { p * (1.0 + (r / p) * (r / p)).sqrt() };
        let shifted = td + if td > 0.0 { h } else { -h };
        if e2 == 0.0 {
            mu -= e / (shifted / e);
        } else {
            mu -= e2 / shifted;
        }
    }
    let mut x = diag[start] - mu;
    let mut z = sub[start];
    let mut k = start;
    while k < end && z != 0.0 {
        let (c, s) = givens(x, z);
        let sdk = s * diag[k] + c * sub[k];
        let dkp1 = s * sub[k] + c * diag[k + 1];
        diag[k] = c * (c * diag[k] - s * sub[k]) - s * (c * sub[k] - s * diag[k + 1]);
        diag[k + 1] = s * sdk + c * dkp1;
        sub[k] = c * sdk - s * dkp1;
        if k > start {
            sub[k - 1] = c * sub[k - 1] - s * z;
        }
        x = sub[k];
        if k < end - 1 {
            z = -s * sub[k + 1];
            sub[k + 1] *= c;
        }
        // Q = Q G: columns k and k + 1.
        let (left, right) = q.split_at_mut(k + 1);
        for (xi, yi) in left[k].iter_mut().zip(right[0].iter_mut()) {
            let (a, b) = (*xi, *yi);
            *xi = c * a - s * b;
            *yi = s * a + c * b;
        }
        k += 1;
    }
}

/// Eigen's `JacobiRotation::makeGivens` for reals: `(c, s)` such that the rotation zeroes `q`.
fn givens(p: f64, q: f64) -> (f64, f64) {
    if q == 0.0 {
        (if p < 0.0 { -1.0 } else { 1.0 }, 0.0)
    } else if p == 0.0 {
        (0.0, if q < 0.0 { 1.0 } else { -1.0 })
    } else if p.abs() > q.abs() {
        let t = q / p;
        let mut u = (1.0 + t * t).sqrt();
        if p < 0.0 {
            u = -u;
        }
        let c = 1.0 / u;
        (c, -t * c)
    } else {
        let t = p / q;
        let mut u = (1.0 + t * t).sqrt();
        if q < 0.0 {
            u = -u;
        }
        let s = -1.0 / u;
        (-t * s, s)
    }
}
