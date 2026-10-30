//! Exact integer helpers: 256-bit product comparison, canonical rational points
//! and correctly rounded rational-to-binary64 conversion.
//!
//! Bounds (see notes/design.md): input ordinates are `i32`, so differences fit in
//! 33 bits, orientation determinants in 66 bits, rational node numerators in
//! 98 bits and denominators in 66 bits. Cross-multiplied node comparisons reach
//! about 164 bits and are evaluated as exact 256-bit products.

use std::cmp::Ordering;

const LOW64: u128 = u64::MAX as u128;

/// Full 256-bit product of two unsigned 128-bit values as `(high, low)`.
fn wide_mul(a: u128, b: u128) -> (u128, u128) {
    let (a1, a0) = (a >> 64, a & LOW64);
    let (b1, b0) = (b >> 64, b & LOW64);
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    let middle = (p00 >> 64) + (p01 & LOW64) + (p10 & LOW64);
    let low = (p00 & LOW64) | (middle << 64);
    let high = p11 + (p01 >> 64) + (p10 >> 64) + (middle >> 64);
    (high, low)
}

fn sign(value: i128) -> i8 {
    match value.cmp(&0) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// Exact comparison of `a * b` with `c * d`.
pub fn cmp_products(a: i128, b: i128, c: i128, d: i128) -> Ordering {
    let left_sign = sign(a) * sign(b);
    let right_sign = sign(c) * sign(d);
    if left_sign != right_sign || left_sign == 0 {
        return left_sign.cmp(&right_sign);
    }
    let left = wide_mul(a.unsigned_abs(), b.unsigned_abs());
    let right = wide_mul(c.unsigned_abs(), d.unsigned_abs());
    let magnitude = left.cmp(&right);
    if left_sign > 0 { magnitude } else { magnitude.reverse() }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

/// A rational point `(x / w, y / w)` in lowest terms with `w > 0`.
///
/// Because the representation is canonical, derived equality and hashing are
/// exact geometric equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RPoint {
    pub x: i128,
    pub y: i128,
    pub w: i128,
}

impl RPoint {
    pub fn integer(x: i64, y: i64) -> Self {
        RPoint { x: i128::from(x), y: i128::from(y), w: 1 }
    }

    /// Canonicalise `(x / w, y / w)`; `w` must be nonzero and all magnitudes
    /// below 2^126 (guaranteed by the construction bounds above).
    pub fn new(x: i128, y: i128, w: i128) -> Self {
        let (x, y, w) = if w < 0 { (-x, -y, -w) } else { (x, y, w) };
        let g = gcd(gcd(x.unsigned_abs(), y.unsigned_abs()), w.unsigned_abs());
        if g <= 1 {
            return RPoint { x, y, w };
        }
        // `g` divides each value and is at most |w| < 2^127, so it fits in i128.
        let g = g as i128;
        RPoint { x: x / g, y: y / g, w: w / g }
    }

    pub fn cmp_x(&self, other: &Self) -> Ordering {
        cmp_products(self.x, other.w, other.x, self.w)
    }

    pub fn cmp_y(&self, other: &Self) -> Ordering {
        cmp_products(self.y, other.w, other.y, self.w)
    }

    /// Lexicographic (x, then y) order.
    pub fn cmp_lex(&self, other: &Self) -> Ordering {
        self.cmp_x(other).then_with(|| self.cmp_y(other))
    }

    pub fn is_integer(&self) -> bool {
        self.w == 1
    }

    /// Smallest integer `>= y`.
    pub fn ceil_y(&self) -> i128 {
        self.y.div_euclid(self.w) + i128::from(self.y.rem_euclid(self.w) != 0)
    }

    pub fn to_f64(self) -> (f64, f64) {
        (ratio_to_f64(self.x, self.w), ratio_to_f64(self.y, self.w))
    }
}

fn pow2(exponent: i32) -> f64 {
    // All callers stay well inside the normal binary64 exponent range; clamp
    // defensively so the bit pattern is always a finite power of two.
    let biased = (exponent + 1023).clamp(1, 2046) as u64;
    f64::from_bits(biased << 52)
}

/// Nearest binary64 to `num / den` (`den > 0`), ties to even.
pub fn ratio_to_f64(num: i128, den: i128) -> f64 {
    if num == 0 || den == 0 {
        return 0.0;
    }
    let negative = (num < 0) != (den < 0);
    let n = num.unsigned_abs();
    let d = den.unsigned_abs();
    let mut q = n / d;
    let mut r = n % d;
    let mut exponent: i32 = 0;
    let mut sticky = false;
    const TOP: u128 = 1 << 55;
    const LOW: u128 = 1 << 54;
    if q >= TOP {
        while q >= TOP {
            sticky |= q & 1 == 1;
            q >>= 1;
            exponent += 1;
        }
        sticky |= r != 0;
    } else {
        // Long division, one quotient bit at a time, until 55 significant bits.
        while q < LOW {
            q <<= 1;
            if r >= d - r {
                r -= d - r;
                q |= 1;
            } else {
                r += r;
            }
            exponent -= 1;
        }
        sticky |= r != 0;
    }
    let mut mantissa = (q >> 2) as u64;
    let rest = q & 3;
    if rest == 3 || (rest == 2 && (sticky || mantissa & 1 == 1)) {
        mantissa += 1;
    }
    let value = mantissa as f64 * pow2(exponent + 2);
    if negative { -value } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn products_compare_beyond_i128() {
        let big = 1_i128 << 100;
        assert_eq!(cmp_products(big, big, big, big - 1), Ordering::Greater);
        assert_eq!(cmp_products(-big, big, big, -big), Ordering::Equal);
        assert_eq!(cmp_products(-big, big, 1, 1), Ordering::Less);
        assert_eq!(cmp_products(0, big, 0, -big), Ordering::Equal);
        assert_eq!(cmp_products(i128::MIN + 1, 3, i128::MAX, -3), Ordering::Equal);
        assert_eq!(cmp_products(-3, -5, 2, 7), Ordering::Greater);
    }

    #[test]
    fn rounding_matches_known_values() {
        assert_eq!(ratio_to_f64(42, 13), 42.0 / 13.0);
        assert_eq!(ratio_to_f64(30, 13), 2.3076923076923075);
        assert_eq!(ratio_to_f64(-7, 2), -3.5);
        assert_eq!(ratio_to_f64(1, 3), 1.0 / 3.0);
        assert_eq!(ratio_to_f64(i128::from(i32::MIN), 1), -2147483648.0);
        // 2^53 + 1 is a tie between 2^53 and 2^53 + 2: round to even.
        assert_eq!(ratio_to_f64((1 << 53) + 1, 1), 9007199254740992.0);
        assert_eq!(ratio_to_f64((1 << 53) + 3, 1), 9007199254740996.0);
        // Slightly above the tie rounds up.
        assert_eq!(ratio_to_f64(((1 << 53) + 1) * 1000 + 1, 1000), 9007199254740994.0);
        // Very small magnitudes keep full precision.
        assert_eq!(ratio_to_f64(1, 1 << 100), 2f64.powi(-100));
    }

    #[test]
    fn rounding_agrees_with_f64_division_for_exact_operands() {
        // For operands exactly representable in binary64, IEEE division is
        // correctly rounded, giving an independent check.
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..20000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let n = (seed >> 11) as i64 - (1 << 52);
            let d = ((seed.rotate_left(29) >> 12) | 1) as i64;
            assert_eq!(ratio_to_f64(i128::from(n), i128::from(d)), n as f64 / d as f64, "{n}/{d}");
        }
    }

    #[test]
    fn canonical_points() {
        let p = RPoint::new(6, -9, -3);
        assert_eq!(p, RPoint { x: -2, y: 3, w: 1 });
        assert!(p.is_integer());
        let q = RPoint::new(7, 5, 2);
        assert_eq!(q.ceil_y(), 3);
        assert_eq!(RPoint::new(-7, -5, 2).ceil_y(), -2);
        assert_eq!(RPoint::new(42, 30, 13).cmp_lex(&RPoint::integer(3, 9)), Ordering::Greater);
    }
}
