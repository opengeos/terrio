//! A vectorized evaluator of [`Expr`]essions with NumPy's typing rules, so that results match
//! `gdal_calc.py`, which evaluates its expressions with NumPy:
//!
//! - arrays keep their data type, and integer arithmetic wraps around;
//! - mixed types are promoted as NumPy promotes them, and Python number literals are "weak"
//!   (NEP 50): `A + 1` keeps the type of `A`, `A * 0.5` is `float64` for an integer `A`;
//! - `/` gives `float64` for integers; the floating-point functions (`sqrt`, `log`...) give
//!   `float16` for 8-bit integers, `float32` for 16-bit integers and `float64` for wider ones;
//! - integer division and modulo by zero give 0; floating-point ones follow IEEE 754.

use super::expr::{BinOp, CmpOp, Expr, UnaryOp};
use crate::{Error, Result};
use half::f16;

/// A NumPy data type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DType {
    /// `bool`.
    Bool,
    /// `uint8`.
    U8,
    /// `int8`.
    I8,
    /// `uint16`.
    U16,
    /// `int16`.
    I16,
    /// `uint32`.
    U32,
    /// `int32`.
    I32,
    /// `uint64`.
    U64,
    /// `int64`.
    I64,
    /// `float16`.
    F16,
    /// `float32`.
    F32,
    /// `float64`.
    F64,
}

impl DType {
    pub(crate) fn is_float(self) -> bool {
        matches!(self, Self::F16 | Self::F32 | Self::F64)
    }

    fn is_int(self) -> bool {
        !self.is_float() && self != Self::Bool
    }

    fn is_signed(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    fn bits(self) -> u32 {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 8,
            Self::U16 | Self::I16 | Self::F16 => 16,
            Self::U32 | Self::I32 | Self::F32 => 32,
            Self::U64 | Self::I64 | Self::F64 => 64,
        }
    }

    fn int(bits: u32, signed: bool) -> Self {
        match (bits, signed) {
            (8, false) => Self::U8,
            (8, true) => Self::I8,
            (16, false) => Self::U16,
            (16, true) => Self::I16,
            (32, false) => Self::U32,
            (32, true) => Self::I32,
            (64, false) => Self::U64,
            _ => Self::I64,
        }
    }

    /// The range of an integer type.
    fn range(self) -> (i128, i128) {
        match self {
            Self::Bool => (0, 1),
            Self::U8 => (0, u8::MAX as i128),
            Self::I8 => (i8::MIN as i128, i8::MAX as i128),
            Self::U16 => (0, u16::MAX as i128),
            Self::I16 => (i16::MIN as i128, i16::MAX as i128),
            Self::U32 => (0, u32::MAX as i128),
            Self::I32 => (i32::MIN as i128, i32::MAX as i128),
            Self::U64 => (0, u64::MAX as i128),
            _ => (i64::MIN as i128, i64::MAX as i128),
        }
    }

    /// NumPy's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 => "uint8",
            Self::I8 => "int8",
            Self::U16 => "uint16",
            Self::I16 => "int16",
            Self::U32 => "uint32",
            Self::I32 => "int32",
            Self::U64 => "uint64",
            Self::I64 => "int64",
            Self::F16 => "float16",
            Self::F32 => "float32",
            Self::F64 => "float64",
        }
    }

    /// Parses a NumPy (or GDAL) type name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "bool" | "bool_" => Self::Bool,
            "uint8" | "byte" | "ubyte" => Self::U8,
            "int8" => Self::I8,
            "uint16" | "ushort" => Self::U16,
            "int16" | "short" => Self::I16,
            "uint32" | "uintc" => Self::U32,
            "int32" | "intc" => Self::I32,
            "uint64" | "uint" => Self::U64,
            "int64" | "int" | "int_" | "intp" => Self::I64,
            "float16" | "half" => Self::F16,
            "float32" | "single" => Self::F32,
            "float64" | "float" | "double" | "float_" => Self::F64,
            _ => return None,
        })
    }

    /// The floating-point type NumPy's float functions use for this type.
    fn float_of(self) -> Self {
        match self {
            Self::Bool | Self::U8 | Self::I8 => Self::F16,
            Self::U16 | Self::I16 => Self::F32,
            Self::U32 | Self::I32 | Self::U64 | Self::I64 => Self::F64,
            f => f,
        }
    }
}

/// NumPy's promotion of two types.
fn promote(a: DType, b: DType) -> DType {
    use DType::*;
    if a == b {
        return a;
    }
    if a == Bool {
        return b;
    }
    if b == Bool {
        return a;
    }
    match (a.is_float(), b.is_float()) {
        (true, true) => a.max(b),
        (true, false) | (false, true) => {
            let (f, i) = if a.is_float() { (a, b) } else { (b, a) };
            let need = match i.bits() {
                8 => F16,
                16 => F32,
                _ => F64,
            };
            f.max(need)
        }
        (false, false) => {
            if a.is_signed() == b.is_signed() {
                return DType::int(a.bits().max(b.bits()), a.is_signed());
            }
            let (u, s) = if a.is_signed() { (b, a) } else { (a, b) };
            if s.bits() > u.bits() {
                s
            } else if u.bits() < 64 {
                DType::int(u.bits() * 2, true)
            } else {
                F64
            }
        }
    }
}

/// Array storage: booleans, integers (any integer type, as `i128`) or floats (any float type,
/// as `f64`, rounded to the type's precision).
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    /// Booleans.
    B(Vec<bool>),
    /// Integers.
    I(Vec<i128>),
    /// Floats.
    F(Vec<f64>),
}

/// An array of one data type. Arrays of length 1 broadcast (NumPy's 0-d arrays and scalars).
#[derive(Debug, Clone, PartialEq)]
pub struct Arr {
    /// The data type.
    pub dt: DType,
    /// The values.
    pub data: Data,
}

impl Arr {
    pub(crate) fn len(&self) -> usize {
        match &self.data {
            Data::B(v) => v.len(),
            Data::I(v) => v.len(),
            Data::F(v) => v.len(),
        }
    }

    /// The values as integers (the array must be of an integer or boolean type).
    fn ints(&self) -> std::borrow::Cow<'_, [i128]> {
        match &self.data {
            Data::I(v) => std::borrow::Cow::Borrowed(v),
            Data::B(v) => std::borrow::Cow::Owned(v.iter().map(|b| *b as i128).collect()),
            Data::F(v) => std::borrow::Cow::Owned(v.iter().map(|f| *f as i128).collect()),
        }
    }

    /// The values as floats.
    pub(crate) fn floats(&self) -> std::borrow::Cow<'_, [f64]> {
        match &self.data {
            Data::F(v) => std::borrow::Cow::Borrowed(v),
            Data::I(v) => std::borrow::Cow::Owned(v.iter().map(|i| *i as f64).collect()),
            Data::B(v) => std::borrow::Cow::Owned(v.iter().map(|b| *b as u8 as f64).collect()),
        }
    }

    /// The truth value of each element.
    fn truth(&self) -> Vec<bool> {
        match &self.data {
            Data::B(v) => v.clone(),
            Data::I(v) => v.iter().map(|i| *i != 0).collect(),
            Data::F(v) => v.iter().map(|f| *f != 0.0).collect(),
        }
    }

    /// Converts to another type (NumPy's `astype`).
    pub(crate) fn cast(&self, dt: DType) -> Arr {
        if dt == self.dt {
            return self.clone();
        }
        let data = match dt {
            DType::Bool => Data::B(self.truth()),
            d if d.is_float() => Data::F(self.floats().iter().map(|v| round_float(*v, d)).collect()),
            d => match &self.data {
                Data::F(v) => Data::I(v.iter().map(|f| wrap(float_to_int(*f, d), d)).collect()),
                _ => Data::I(self.ints().iter().map(|i| wrap(*i, d)).collect()),
            },
        };
        Arr { dt, data }
    }
}

/// Wraps an integer around to an integer type's range.
fn wrap(v: i128, dt: DType) -> i128 {
    match dt {
        DType::Bool => (v != 0) as i128,
        DType::U8 => v as u8 as i128,
        DType::I8 => v as i8 as i128,
        DType::U16 => v as u16 as i128,
        DType::I16 => v as i16 as i128,
        DType::U32 => v as u32 as i128,
        DType::I32 => v as i32 as i128,
        DType::U64 => v as u64 as i128,
        _ => v as i64 as i128,
    }
}

/// C's conversion of a double to an integer type as compiled on x86-64: truncation, with
/// out-of-range values and NaN giving the "integer indefinite" value.
fn float_to_int(f: f64, dt: DType) -> i128 {
    let t = f.trunc();
    if dt.bits() <= 32 && dt != DType::U32 {
        if t.is_finite() && t >= i32::MIN as f64 && t <= i32::MAX as f64 { t as i128 } else { i32::MIN as i128 }
    } else if (dt == DType::U64 && (0.0..18446744073709551616.0).contains(&t)) || (t >= i64::MIN as f64 && t < 9223372036854775808.0) {
        t as i128
    } else {
        i64::MIN as i128
    }
}

/// Rounds a float to a float type's precision.
fn round_float(v: f64, dt: DType) -> f64 {
    match dt {
        DType::F16 => f16::from_f64(v).to_f64(),
        DType::F32 => v as f32 as f64,
        _ => v,
    }
}

/// A value: an array, or a Python number (a "weak" scalar).
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    /// An array (or a NumPy scalar, of length 1).
    Arr(Arr),
    /// A Python `int`.
    Int(i128),
    /// A Python `float`.
    Float(f64),
}

impl Val {
    fn scalar(dt: DType, data: Data) -> Self {
        Val::Arr(Arr { dt, data })
    }

    fn is_weak(&self) -> bool {
        !matches!(self, Val::Arr(_))
    }

    /// The value as an array, weak scalars taking NumPy's default types.
    fn strong(&self) -> Result<Arr> {
        Ok(match self {
            Val::Arr(a) => a.clone(),
            Val::Int(i) => {
                if *i >= i64::MIN as i128 && *i <= i64::MAX as i128 {
                    Arr { dt: DType::I64, data: Data::I(vec![*i]) }
                } else if *i >= 0 && *i <= u64::MAX as i128 {
                    Arr { dt: DType::U64, data: Data::I(vec![*i]) }
                } else {
                    return Err(Error::InvalidArgument(format!("Python integer {i} too large to convert to a NumPy integer")));
                }
            }
            Val::Float(f) => Arr { dt: DType::F64, data: Data::F(vec![*f]) },
        })
    }

    /// The value converted to `dt`. Weak integers outside the type's range are an error when
    /// `strict`, and wrap around otherwise.
    fn to(&self, dt: DType, strict: bool) -> Result<Arr> {
        match self {
            Val::Arr(a) => Ok(a.cast(dt)),
            Val::Int(i) if dt.is_int() || dt == DType::Bool => {
                let (lo, hi) = dt.range();
                if strict && dt != DType::Bool && (*i < lo || *i > hi) {
                    return Err(Error::InvalidArgument(format!("Python integer {i} out of bounds for {}", dt.name())));
                }
                Ok(Arr { dt: DType::I64, data: Data::I(vec![*i]) }.cast(dt))
            }
            Val::Int(i) => Ok(Arr { dt, data: Data::F(vec![round_float(*i as f64, dt)]) }),
            Val::Float(f) => Ok(Arr { dt: DType::F64, data: Data::F(vec![*f]) }.cast(dt)),
        }
    }
}

/// The result type of an operation on these values, with NEP 50's weak Python scalars.
fn result_type(vals: &[&Val]) -> Result<DType> {
    let strong: Vec<DType> = vals
        .iter()
        .filter_map(|v| match v {
            Val::Arr(a) => Some(a.dt),
            _ => None,
        })
        .collect();
    if strong.is_empty() {
        let mut dt = vals[0].strong()?.dt;
        for v in &vals[1..] {
            dt = promote(dt, v.strong()?.dt);
        }
        return Ok(dt);
    }
    let mut dt = strong.iter().copied().reduce(promote).expect("not empty");
    for v in vals {
        match v {
            Val::Float(_) if !dt.is_float() => dt = promote(dt, DType::F64),
            Val::Int(_) if dt == DType::Bool => dt = DType::I64,
            _ => {}
        }
    }
    Ok(dt)
}

/// Broadcasts two lengths.
fn bcast(a: usize, b: usize) -> Result<usize> {
    if a == b || b == 1 {
        Ok(a)
    } else if a == 1 {
        Ok(b)
    } else {
        Err(Error::InvalidArgument(format!("operands could not be broadcast together ({a} and {b} elements)")))
    }
}

#[inline]
fn at<T: Copy>(v: &[T], i: usize) -> T {
    if v.len() == 1 { v[0] } else { v[i] }
}

fn map2<T: Copy, U: Copy, R>(a: &[T], b: &[U], f: impl Fn(T, U) -> R) -> Result<Vec<R>> {
    let n = bcast(a.len(), b.len())?;
    Ok((0..n).map(|i| f(at(a, i), at(b, i))).collect())
}

/// Floating-point arithmetic in a given precision.
trait Fl:
    Copy
    + PartialOrd
    + std::ops::Add<Output = Self>
    + std::ops::Sub<Output = Self>
    + std::ops::Mul<Output = Self>
    + std::ops::Div<Output = Self>
{
    const ZERO: Self;
    const ONE: Self;
    const HALF: Self;
    fn fmod(self, o: Self) -> Self;
    fn floor(self) -> Self;
    fn copysign(self, o: Self) -> Self;
    fn is_nan(self) -> bool;
    fn powf(self, o: Self) -> Self;
    fn atan2(self, o: Self) -> Self;
    fn hypot(self, o: Self) -> Self;
    fn unary(self, f: &str) -> Option<Self>;
}

macro_rules! impl_fl {
    ($t:ty, $pi:expr) => {
        impl Fl for $t {
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            const HALF: Self = 0.5;
            fn fmod(self, o: Self) -> Self {
                self % o
            }
            fn floor(self) -> Self {
                <$t>::floor(self)
            }
            fn copysign(self, o: Self) -> Self {
                <$t>::copysign(self, o)
            }
            fn is_nan(self) -> bool {
                <$t>::is_nan(self)
            }
            fn powf(self, o: Self) -> Self {
                <$t>::powf(self, o)
            }
            fn atan2(self, o: Self) -> Self {
                <$t>::atan2(self, o)
            }
            fn hypot(self, o: Self) -> Self {
                <$t>::hypot(self, o)
            }
            fn unary(self, f: &str) -> Option<Self> {
                const PI: $t = $pi;
                Some(match f {
                    "sqrt" => self.sqrt(),
                    "exp" => self.exp(),
                    "exp2" => self.exp2(),
                    "expm1" => self.exp_m1(),
                    "log" => self.ln(),
                    "log10" => self.log10(),
                    "log2" => self.log2(),
                    "log1p" => self.ln_1p(),
                    "sin" => self.sin(),
                    "cos" => self.cos(),
                    "tan" => self.tan(),
                    "arcsin" => self.asin(),
                    "arccos" => self.acos(),
                    "arctan" => self.atan(),
                    "sinh" => self.sinh(),
                    "cosh" => self.cosh(),
                    "tanh" => self.tanh(),
                    "arcsinh" => self.asinh(),
                    "arccosh" => self.acosh(),
                    "arctanh" => self.atanh(),
                    "cbrt" => self.cbrt(),
                    "degrees" => self * (180.0 / PI),
                    "radians" => self * (PI / 180.0),
                    "rint" => self.round_ties_even(),
                    "fabs" => self.abs(),
                    "reciprocal" => 1.0 / self,
                    _ => return None,
                })
            }
        }
    };
}
impl_fl!(f32, std::f32::consts::PI);
impl_fl!(f64, std::f64::consts::PI);

/// NumPy's `npy_divmod`: the floored quotient and the modulo.
fn divmod<T: Fl>(a: T, b: T) -> (T, T) {
    let m = a.fmod(b);
    if b == T::ZERO {
        return (a / b, m);
    }
    let mut div = (a - m) / b;
    let mut m = m;
    if m != T::ZERO {
        if (b < T::ZERO) != (m < T::ZERO) {
            m = m + b;
            div = div - T::ONE;
        }
    } else {
        m = T::ZERO.copysign(b);
    }
    let floordiv = if div != T::ZERO {
        let mut f = div.floor();
        if div - f > T::HALF {
            f = f + T::ONE;
        }
        f
    } else {
        T::ZERO.copysign(a / b)
    };
    (floordiv, m)
}

/// Applies a binary floating-point operation in the precision of `dt`.
fn fbin(dt: DType, a: f64, b: f64, f: fn(Fbin, f64, f64) -> f64, f32op: fn(Fbin, f32, f32) -> f32, op: Fbin) -> f64 {
    match dt {
        DType::F64 => f(op, a, b),
        DType::F32 => f32op(op, a as f32, b as f32) as f64,
        _ => f16::from_f32(f32op(op, a as f32, b as f32)).to_f64(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fbin {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Fmod,
    Atan2,
    Hypot,
    Min,
    Max,
    Fmin,
    Fmax,
    Copysign,
}

fn fbin_t<T: Fl>(op: Fbin, a: T, b: T) -> T {
    match op {
        Fbin::Add => a + b,
        Fbin::Sub => a - b,
        Fbin::Mul => a * b,
        Fbin::Div => a / b,
        Fbin::FloorDiv => divmod(a, b).0,
        Fbin::Mod => {
            if b == T::ZERO {
                a.fmod(b)
            } else {
                divmod(a, b).1
            }
        }
        Fbin::Pow => a.powf(b),
        Fbin::Fmod => a.fmod(b),
        Fbin::Atan2 => a.atan2(b),
        Fbin::Hypot => a.hypot(b),
        Fbin::Min => {
            if a.is_nan() || a <= b {
                a
            } else {
                b
            }
        }
        Fbin::Max => {
            if a.is_nan() || a >= b {
                a
            } else {
                b
            }
        }
        Fbin::Fmin => {
            if b.is_nan() || a <= b {
                a
            } else {
                b
            }
        }
        Fbin::Fmax => {
            if b.is_nan() || a >= b {
                a
            } else {
                b
            }
        }
        Fbin::Copysign => a.copysign(b),
    }
}

fn float_binary(op: Fbin, a: &Arr, b: &Arr, dt: DType) -> Result<Arr> {
    let (a, b) = (a.cast(dt), b.cast(dt));
    // NumPy computes powers by a scalar 2, 0.5, -1 or 1 as a square, a square root, a
    // reciprocal or a copy.
    if op == Fbin::Pow && b.len() == 1 && matches!(b.floats()[0], 2.0 | 0.5 | -1.0 | 1.0) {
        let e = b.floats()[0];
        let f = |x: f64| -> f64 {
            if e == 2.0 {
                fbin(dt, x, x, fbin_t::<f64>, fbin_t::<f32>, Fbin::Mul)
            } else if e == -1.0 {
                fbin(dt, 1.0, x, fbin_t::<f64>, fbin_t::<f32>, Fbin::Div)
            } else if e == 0.5 {
                round_float(x.sqrt(), dt)
            } else {
                x
            }
        };
        return Ok(Arr { dt, data: Data::F(a.floats().iter().map(|x| f(*x)).collect()) });
    }
    let v = map2(&a.floats(), &b.floats(), |x, y| fbin(dt, x, y, fbin_t::<f64>, fbin_t::<f32>, op))?;
    Ok(Arr { dt, data: Data::F(v) })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ibin {
    Add,
    Sub,
    Mul,
    FloorDiv,
    Mod,
    Pow,
    Fmod,
    And,
    Or,
    Xor,
    Min,
    Max,
}

fn int_pow(mut base: i128, mut exp: i128) -> i128 {
    let mut r: i128 = 1;
    while exp > 0 {
        if exp & 1 == 1 {
            r = r.wrapping_mul(base);
        }
        base = base.wrapping_mul(base);
        exp >>= 1;
    }
    r
}

fn int_binary(op: Ibin, a: &Arr, b: &Arr, dt: DType) -> Result<Arr> {
    let (a, b) = (a.cast(dt), b.cast(dt));
    let (av, bv) = (a.ints(), b.ints());
    if op == Ibin::Pow && bv.iter().any(|e| *e < 0) {
        return Err(Error::InvalidArgument("integers to negative integer powers are not allowed".into()));
    }
    let v = map2(&av, &bv, |x, y| {
        let r = match op {
            Ibin::Add => x.wrapping_add(y),
            Ibin::Sub => x.wrapping_sub(y),
            Ibin::Mul => x.wrapping_mul(y),
            Ibin::FloorDiv => {
                if y == 0 {
                    0
                } else {
                    let q = x / y;
                    if x % y != 0 && ((x < 0) != (y < 0)) { q - 1 } else { q }
                }
            }
            Ibin::Mod => {
                if y == 0 {
                    0
                } else {
                    let r = x % y;
                    if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r }
                }
            }
            Ibin::Fmod => {
                if y == 0 {
                    0
                } else {
                    x % y
                }
            }
            Ibin::Pow => int_pow(x, y),
            Ibin::And => x & y,
            Ibin::Or => x | y,
            Ibin::Xor => x ^ y,
            Ibin::Min => x.min(y),
            Ibin::Max => x.max(y),
        };
        wrap(r, dt)
    })?;
    Ok(Arr { dt, data: Data::I(v) })
}

fn bool_binary(op: Ibin, a: &Arr, b: &Arr) -> Result<Arr> {
    let v = map2(&a.truth(), &b.truth(), |x, y| match op {
        Ibin::Add | Ibin::Or | Ibin::Max => x | y,
        Ibin::Mul | Ibin::And | Ibin::Min => x & y,
        _ => x ^ y,
    })?;
    Ok(Arr { dt: DType::Bool, data: Data::B(v) })
}

/// Python's arithmetic on two Python numbers.
fn python_binary(op: BinOp, a: &Val, b: &Val) -> Result<Val> {
    let zero = || Error::InvalidArgument("division by zero".into());
    let overflow = || Error::InvalidArgument("integer overflow".into());
    if let (Val::Int(x), Val::Int(y)) = (a, b) {
        let (x, y) = (*x, *y);
        return Ok(match op {
            BinOp::Add => Val::Int(x.checked_add(y).ok_or_else(overflow)?),
            BinOp::Sub => Val::Int(x.checked_sub(y).ok_or_else(overflow)?),
            BinOp::Mul => Val::Int(x.checked_mul(y).ok_or_else(overflow)?),
            BinOp::Div if y == 0 => return Err(zero()),
            BinOp::Div => Val::Float(x as f64 / y as f64),
            BinOp::FloorDiv | BinOp::Mod if y == 0 => return Err(zero()),
            BinOp::FloorDiv => Val::Int(x.div_euclid(y) - if y < 0 && x.rem_euclid(y) != 0 { 1 } else { 0 }),
            BinOp::Mod => {
                let r = x % y;
                Val::Int(if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r })
            }
            BinOp::Pow if y < 0 => Val::Float((x as f64).powf(y as f64)),
            BinOp::Pow => Val::Int(x.checked_pow(u32::try_from(y).map_err(|_| overflow())?).ok_or_else(overflow)?),
            BinOp::BitAnd => Val::Int(x & y),
            BinOp::BitOr => Val::Int(x | y),
            BinOp::BitXor => Val::Int(x ^ y),
        });
    }
    let f = |v: &Val| match v {
        Val::Int(i) => *i as f64,
        Val::Float(f) => *f,
        Val::Arr(_) => unreachable!("weak scalars only"),
    };
    let (x, y) = (f(a), f(b));
    Ok(Val::Float(match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div | BinOp::FloorDiv | BinOp::Mod if y == 0.0 => return Err(zero()),
        BinOp::Div => x / y,
        BinOp::FloorDiv => divmod(x, y).0,
        BinOp::Mod => divmod(x, y).1,
        BinOp::Pow => x.powf(y),
        _ => return Err(Error::InvalidArgument("bitwise operators need integers".into())),
    }))
}

/// Applies a binary operator.
pub(crate) fn binary(op: BinOp, a: &Val, b: &Val) -> Result<Val> {
    if a.is_weak() && b.is_weak() {
        return python_binary(op, a, b);
    }
    let dt = result_type(&[a, b])?;
    let strict = !dt.is_float();
    let (x, y) = (a.to(dt, strict)?, b.to(dt, strict)?);
    let iop = match op {
        BinOp::Add => Ibin::Add,
        BinOp::Sub => Ibin::Sub,
        BinOp::Mul => Ibin::Mul,
        BinOp::FloorDiv => Ibin::FloorDiv,
        BinOp::Mod => Ibin::Mod,
        BinOp::Pow => Ibin::Pow,
        BinOp::BitAnd => Ibin::And,
        BinOp::BitOr => Ibin::Or,
        BinOp::BitXor => Ibin::Xor,
        BinOp::Div => {
            let ft = if dt.is_float() { dt } else { DType::F64 };
            return Ok(Val::Arr(float_binary(Fbin::Div, &x, &y, ft)?));
        }
    };
    arith(iop, &x, &y, dt).map(Val::Arr)
}

/// An arithmetic operation on arrays already converted to their common type `dt`.
fn arith(op: Ibin, x: &Arr, y: &Arr, dt: DType) -> Result<Arr> {
    if dt == DType::Bool {
        return match op {
            Ibin::Sub => Err(Error::InvalidArgument("the - operator is not supported for booleans; use ^ or logical_xor".into())),
            Ibin::FloorDiv | Ibin::Mod | Ibin::Pow | Ibin::Fmod => int_binary(op, x, y, DType::I8),
            _ => bool_binary(op, x, y),
        };
    }
    if dt.is_float() {
        let fop = match op {
            Ibin::Add => Fbin::Add,
            Ibin::Sub => Fbin::Sub,
            Ibin::Mul => Fbin::Mul,
            Ibin::FloorDiv => Fbin::FloorDiv,
            Ibin::Mod => Fbin::Mod,
            Ibin::Pow => Fbin::Pow,
            Ibin::Fmod => Fbin::Fmod,
            Ibin::Min => Fbin::Min,
            Ibin::Max => Fbin::Max,
            Ibin::And | Ibin::Or | Ibin::Xor => {
                return Err(Error::InvalidArgument(format!("bitwise operators are not supported for {}", dt.name())));
            }
        };
        return float_binary(fop, x, y, dt);
    }
    int_binary(op, x, y, dt)
}

/// Applies a comparison.
fn compare(op: CmpOp, a: &Val, b: &Val) -> Result<Arr> {
    let cmp = |o: std::cmp::Ordering| match op {
        CmpOp::Lt => o.is_lt(),
        CmpOp::Le => o.is_le(),
        CmpOp::Gt => o.is_gt(),
        CmpOp::Ge => o.is_ge(),
        CmpOp::Eq => o.is_eq(),
        CmpOp::Ne => o.is_ne(),
    };
    let dt = if a.is_weak() && b.is_weak() { promote(a.strong()?.dt, b.strong()?.dt) } else { result_type(&[a, b])? };
    let v = if dt.is_float() {
        let (x, y) = (a.to(dt, false)?, b.to(dt, false)?);
        map2(&x.floats(), &y.floats(), |p, q| match p.partial_cmp(&q) {
            Some(o) => cmp(o),
            None => op == CmpOp::Ne,
        })?
    } else {
        // Integers compare exactly, Python integers outside the type's range included.
        let ints = |v: &Val| -> Result<Vec<i128>> {
            Ok(match v {
                Val::Int(i) => vec![*i],
                _ => v.to(dt, false)?.ints().into_owned(),
            })
        };
        map2(&ints(a)?, &ints(b)?, |p, q| cmp(p.cmp(&q)))?
    };
    Ok(Arr { dt: DType::Bool, data: Data::B(v) })
}

/// `a == b`, element by element.
pub(crate) fn binary_compare_eq(a: &Val, b: &Val) -> Result<Vec<bool>> {
    match compare(CmpOp::Eq, a, b)?.data {
        Data::B(v) => Ok(v),
        _ => unreachable!("comparisons give booleans"),
    }
}

impl Val {
    /// The value as an array (weak scalars taking NumPy's default types).
    pub(crate) fn into_arr(self) -> Arr {
        match self {
            Val::Arr(a) => a,
            v => v.strong().unwrap_or(Arr { dt: DType::F64, data: Data::F(vec![f64::NAN]) }),
        }
    }
}

impl From<Arr> for Val {
    fn from(a: Arr) -> Self {
        Val::Arr(a)
    }
}

fn unary_op(op: UnaryOp, v: &Val) -> Result<Val> {
    Ok(match (op, v) {
        (UnaryOp::Pos, v) => v.clone(),
        (UnaryOp::Neg, Val::Int(i)) => Val::Int(-i),
        (UnaryOp::Neg, Val::Float(f)) => Val::Float(-f),
        (UnaryOp::Invert, Val::Int(i)) => Val::Int(!i),
        (UnaryOp::Invert, Val::Float(_)) => return Err(Error::InvalidArgument("~ needs an integer".into())),
        (UnaryOp::Neg, Val::Arr(a)) => Val::Arr(match &a.data {
            Data::B(_) => return Err(Error::InvalidArgument("the - operator is not supported for booleans".into())),
            Data::I(v) => Arr { dt: a.dt, data: Data::I(v.iter().map(|x| wrap(-x, a.dt)).collect()) },
            Data::F(v) => Arr { dt: a.dt, data: Data::F(v.iter().map(|x| -x).collect()) },
        }),
        (UnaryOp::Invert, Val::Arr(a)) => Val::Arr(match &a.data {
            Data::B(v) => Arr { dt: a.dt, data: Data::B(v.iter().map(|x| !x).collect()) },
            Data::I(v) => Arr { dt: a.dt, data: Data::I(v.iter().map(|x| wrap(!x, a.dt)).collect()) },
            Data::F(_) => return Err(Error::InvalidArgument(format!("~ is not supported for {}", a.dt.name()))),
        }),
    })
}

fn logical(vals: &[Val], f: impl Fn(&[bool]) -> bool) -> Result<Val> {
    let arrs = vals.iter().map(|v| v.strong()).collect::<Result<Vec<_>>>()?;
    let truths: Vec<Vec<bool>> = arrs.iter().map(|a| a.truth()).collect();
    let n = truths.iter().map(|t| t.len()).try_fold(1, bcast)?;
    let v = (0..n).map(|i| f(&truths.iter().map(|t| at(t, i)).collect::<Vec<_>>())).collect();
    Ok(Val::scalar(DType::Bool, Data::B(v)))
}

/// The value of a named constant.
fn constant(name: &str) -> Option<Val> {
    Some(match name {
        "pi" => Val::Float(std::f64::consts::PI),
        "e" => Val::Float(std::f64::consts::E),
        "nan" | "NaN" | "NAN" => Val::Float(f64::NAN),
        "inf" | "Inf" | "Infinity" | "PINF" | "infty" => Val::Float(f64::INFINITY),
        "NINF" => Val::Float(f64::NEG_INFINITY),
        "euler_gamma" => Val::Float(0.577_215_664_901_532_9),
        _ => return None,
    })
}

/// Evaluates expressions over named arrays.
pub struct Evaluator<'a> {
    /// Looks up a name, returning `None` for names that are not inputs.
    pub lookup: &'a dyn Fn(&str) -> Option<Val>,
}

impl Evaluator<'_> {
    /// Evaluates an expression.
    pub fn eval(&self, e: &Expr) -> Result<Val> {
        Ok(match e {
            Expr::Int(i) => Val::Int(*i),
            Expr::Float(f) => Val::Float(*f),
            Expr::Bool(b) => Val::scalar(DType::Bool, Data::B(vec![*b])),
            Expr::Name(n) => match (self.lookup)(n).or_else(|| constant(n)) {
                Some(v) => v,
                None => return Err(Error::InvalidArgument(format!("name '{n}' is not defined"))),
            },
            Expr::Unary(op, x) => unary_op(*op, &self.eval(x)?)?,
            Expr::Binary(op, a, b) => binary(*op, &self.eval(a)?, &self.eval(b)?)?,
            Expr::Compare(first, rest) => {
                let mut left = self.eval(first)?;
                let mut acc: Option<Val> = None;
                for (op, r) in rest {
                    let right = self.eval(r)?;
                    let c = Val::Arr(compare(*op, &left, &right)?);
                    acc = Some(match acc {
                        None => c,
                        Some(prev) => logical(&[prev, c], |t| t[0] && t[1])?,
                    });
                    left = right;
                }
                acc.expect("at least one comparison")
            }
            Expr::And(items) => logical(&items.iter().map(|x| self.eval(x)).collect::<Result<Vec<_>>>()?, |t| t.iter().all(|b| *b))?,
            Expr::Or(items) => logical(&items.iter().map(|x| self.eval(x)).collect::<Result<Vec<_>>>()?, |t| t.iter().any(|b| *b))?,
            Expr::Not(x) => logical(&[self.eval(x)?], |t| !t[0])?,
            Expr::Call(name, args, kwargs) => self.call(name, args, kwargs)?,
        })
    }

    fn call(&self, name: &str, args: &[Expr], kwargs: &[(String, Expr)]) -> Result<Val> {
        let nargs = |n: std::ops::RangeInclusive<usize>| -> Result<()> {
            if n.contains(&args.len()) {
                Ok(())
            } else {
                Err(Error::InvalidArgument(format!("{name}() takes {} to {} arguments, not {}", n.start(), n.end(), args.len())))
            }
        };
        let kw = |key: &str| kwargs.iter().find(|(k, _)| k == key).map(|(_, e)| e);
        if let Some((k, _)) =
            kwargs.iter().find(|(k, _)| !matches!((name, k.as_str()), ("round" | "around", "decimals") | ("astype", "dtype")))
        {
            return Err(Error::InvalidArgument(format!("{name}() got an unexpected keyword argument '{k}'")));
        }
        // Type conversions: `A.astype(float32)`, `float32(A)`.
        if name == "astype" || DType::parse(name).is_some() && !matches!(name, "float" | "int" | "bool") {
            let (value, ty) = if name == "astype" {
                let ty = kw("dtype").or(args.get(1)).ok_or_else(|| Error::InvalidArgument("astype() needs a type".into()))?;
                nargs(1..=2)?;
                (&args[0], type_name(ty)?)
            } else {
                nargs(1..=1)?;
                (&args[0], DType::parse(name).expect("checked"))
            };
            return Ok(Val::Arr(self.eval(value)?.strong()?.cast(ty)));
        }
        let vals = args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        let canonical = match name {
            "asin" => "arcsin",
            "acos" => "arccos",
            "atan" => "arctan",
            "asinh" => "arcsinh",
            "acosh" => "arccosh",
            "atanh" => "arctanh",
            "ln" => "log",
            "deg2rad" => "radians",
            "rad2deg" => "degrees",
            "atan2" => "arctan2",
            "absolute" => "abs",
            "clamp" => "clip",
            "amin" => "min",
            "amax" => "max",
            "around" => "round",
            "true_divide" => "divide",
            "pow" => "power",
            n => n,
        };
        match canonical {
            "sqrt" | "exp" | "exp2" | "expm1" | "log" | "log10" | "log2" | "log1p" | "sin" | "cos" | "tan" | "arcsin" | "arccos"
            | "arctan" | "sinh" | "cosh" | "tanh" | "arcsinh" | "arccosh" | "arctanh" | "cbrt" | "degrees" | "radians" | "rint"
            | "fabs" | "reciprocal" => {
                nargs(1..=1)?;
                let a = vals[0].strong()?;
                let dt = a.dt.float_of();
                let a = a.cast(dt);
                let v: Vec<f64> = a
                    .floats()
                    .iter()
                    .map(|x| match dt {
                        DType::F64 => x.unary(canonical).expect("known function"),
                        DType::F32 => (*x as f32).unary(canonical).expect("known function") as f64,
                        _ => f16::from_f32((*x as f32).unary(canonical).expect("known function")).to_f64(),
                    })
                    .collect();
                Ok(Val::scalar(dt, Data::F(v)))
            }
            "floor" | "ceil" | "trunc" | "fix" => {
                nargs(1..=1)?;
                let a = vals[0].strong()?;
                Ok(Val::Arr(match &a.data {
                    Data::F(v) => Arr {
                        dt: a.dt,
                        data: Data::F(
                            v.iter()
                                .map(|x| match canonical {
                                    "floor" => x.floor(),
                                    "ceil" => x.ceil(),
                                    _ => x.trunc(),
                                })
                                .collect(),
                        ),
                    },
                    _ => a,
                }))
            }
            "round" => {
                nargs(1..=2)?;
                let decimals = match kw("decimals").or(args.get(1)) {
                    None => 0,
                    Some(e) => match self.eval(e)? {
                        Val::Int(d) => d,
                        _ => return Err(Error::InvalidArgument("round() decimals must be an integer".into())),
                    },
                };
                let a = vals[0].strong()?;
                Ok(Val::Arr(match &a.data {
                    Data::F(v) => {
                        let dt = a.dt;
                        let r = |x: f64| -> f64 {
                            if decimals == 0 {
                                return x.round_ties_even();
                            }
                            let f = round_float(10f64.powi(decimals.unsigned_abs() as i32), dt);
                            let y = if decimals > 0 { round_float(x * f, dt) } else { round_float(x / f, dt) };
                            let y = y.round_ties_even();
                            if decimals > 0 { y / f } else { y * f }
                        };
                        Arr { dt, data: Data::F(v.iter().map(|x| round_float(r(*x), dt)).collect()) }
                    }
                    _ if decimals >= 0 => a,
                    _ => {
                        let p = 10i128.pow(decimals.unsigned_abs() as u32);
                        let v = a.ints().iter().map(|x| wrap((x.div_euclid(p) + i128::from(x.rem_euclid(p) * 2 > p)) * p, a.dt)).collect();
                        Arr { dt: a.dt, data: Data::I(v) }
                    }
                }))
            }
            "abs" => {
                nargs(1..=1)?;
                let a = vals[0].strong()?;
                Ok(Val::Arr(match &a.data {
                    Data::B(_) => a,
                    Data::I(v) => Arr { dt: a.dt, data: Data::I(v.iter().map(|x| wrap(x.abs(), a.dt)).collect()) },
                    Data::F(v) => Arr { dt: a.dt, data: Data::F(v.iter().map(|x| x.abs()).collect()) },
                }))
            }
            "sign" => {
                nargs(1..=1)?;
                let a = vals[0].strong()?;
                Ok(Val::Arr(match &a.data {
                    Data::B(_) => return Err(Error::InvalidArgument("sign() is not supported for booleans".into())),
                    Data::I(v) => Arr { dt: a.dt, data: Data::I(v.iter().map(|x| x.signum()).collect()) },
                    Data::F(v) => Arr {
                        dt: a.dt,
                        data: Data::F(
                            v.iter()
                                .map(|x| {
                                    if *x > 0.0 {
                                        1.0
                                    } else if *x < 0.0 {
                                        -1.0
                                    } else if *x == 0.0 {
                                        0.0
                                    } else {
                                        *x
                                    }
                                })
                                .collect(),
                        ),
                    },
                }))
            }
            "square" => {
                nargs(1..=1)?;
                let v = &vals[0];
                let a = v.strong()?;
                let a = if a.dt == DType::Bool { a.cast(DType::I8) } else { a };
                arith(Ibin::Mul, &a, &a, a.dt).map(Val::Arr)
            }
            "negative" => {
                nargs(1..=1)?;
                unary_op(UnaryOp::Neg, &Val::Arr(vals[0].strong()?))
            }
            "positive" => {
                nargs(1..=1)?;
                Ok(Val::Arr(vals[0].strong()?))
            }
            "invert" | "bitwise_not" => {
                nargs(1..=1)?;
                unary_op(UnaryOp::Invert, &Val::Arr(vals[0].strong()?))
            }
            "isnan" | "isinf" | "isfinite" | "signbit" => {
                nargs(1..=1)?;
                let a = vals[0].strong()?;
                let v = match &a.data {
                    Data::F(v) => v
                        .iter()
                        .map(|x| match canonical {
                            "isnan" => x.is_nan(),
                            "isinf" => x.is_infinite(),
                            "isfinite" => x.is_finite(),
                            _ => x.is_sign_negative(),
                        })
                        .collect(),
                    Data::I(v) => v.iter().map(|x| if canonical == "signbit" { *x < 0 } else { canonical == "isfinite" }).collect(),
                    Data::B(v) => vec![canonical == "isfinite"; v.len()],
                };
                Ok(Val::scalar(DType::Bool, Data::B(v)))
            }
            "logical_not" => {
                nargs(1..=1)?;
                logical(&vals, |t| !t[0])
            }
            "logical_and" | "logical_or" | "logical_xor" => {
                nargs(2..=2)?;
                logical(&vals, |t| match canonical {
                    "logical_and" => t[0] && t[1],
                    "logical_or" => t[0] || t[1],
                    _ => t[0] ^ t[1],
                })
            }
            "where" => {
                nargs(3..=3)?;
                let cond = vals[0].strong()?.truth();
                let dt = result_type(&[&vals[1], &vals[2]])?;
                let (x, y) = (vals[1].to(dt, false)?, vals[2].to(dt, false)?);
                let n = [cond.len(), x.len(), y.len()].into_iter().try_fold(1, bcast)?;
                let data = match (&x.data, &y.data) {
                    (Data::B(a), Data::B(b)) => Data::B((0..n).map(|i| if at(&cond, i) { at(a, i) } else { at(b, i) }).collect()),
                    (Data::I(a), Data::I(b)) => Data::I((0..n).map(|i| if at(&cond, i) { at(a, i) } else { at(b, i) }).collect()),
                    (Data::F(a), Data::F(b)) => Data::F((0..n).map(|i| if at(&cond, i) { at(a, i) } else { at(b, i) }).collect()),
                    _ => unreachable!("same type"),
                };
                Ok(Val::scalar(dt, data))
            }
            "clip" => {
                nargs(3..=3)?;
                let dt = result_type(&[&vals[0], &vals[1], &vals[2]])?;
                // Python integers outside the type's range clip to the range.
                let bound = |v: &Val| match v {
                    Val::Int(i) if dt.is_int() => {
                        let (lo, hi) = dt.range();
                        Val::Int((*i).clamp(lo, hi))
                    }
                    v => v.clone(),
                };
                let (a, lo, hi) = (vals[0].to(dt, false)?, bound(&vals[1]).to(dt, false)?, bound(&vals[2]).to(dt, false)?);
                let m = arith(Ibin::Max, &a, &lo, dt)?;
                arith(Ibin::Min, &m, &hi, dt).map(Val::Arr)
            }
            "minimum" | "maximum" | "fmin" | "fmax" | "min" | "max" => {
                if vals.len() < 2 {
                    return Err(Error::InvalidArgument(format!("{name}() takes at least 2 arguments (reductions are not supported)")));
                }
                if !matches!(canonical, "min" | "max") {
                    nargs(2..=2)?;
                }
                let mut acc = vals[0].clone();
                for v in &vals[1..] {
                    let dt = result_type(&[&acc, v])?;
                    let strict = !dt.is_float();
                    let (x, y) = (acc.to(dt, strict)?, v.to(dt, strict)?);
                    acc = Val::Arr(if dt.is_float() {
                        let op = match canonical {
                            "fmin" => Fbin::Fmin,
                            "fmax" => Fbin::Fmax,
                            "minimum" | "min" => Fbin::Min,
                            _ => Fbin::Max,
                        };
                        float_binary(op, &x, &y, dt)?
                    } else {
                        arith(if matches!(canonical, "minimum" | "min" | "fmin") { Ibin::Min } else { Ibin::Max }, &x, &y, dt)?
                    });
                }
                Ok(acc)
            }
            "add" | "subtract" | "multiply" | "divide" | "floor_divide" | "remainder" | "mod" | "power" | "bitwise_and" | "bitwise_or"
            | "bitwise_xor" => {
                nargs(2..=2)?;
                let op = match canonical {
                    "add" => BinOp::Add,
                    "subtract" => BinOp::Sub,
                    "multiply" => BinOp::Mul,
                    "divide" => BinOp::Div,
                    "floor_divide" => BinOp::FloorDiv,
                    "remainder" | "mod" => BinOp::Mod,
                    "power" => BinOp::Pow,
                    "bitwise_and" => BinOp::BitAnd,
                    "bitwise_or" => BinOp::BitOr,
                    _ => BinOp::BitXor,
                };
                let (a, b) = if vals[0].is_weak() && vals[1].is_weak() {
                    (Val::Arr(vals[0].strong()?), Val::Arr(vals[1].strong()?))
                } else {
                    (vals[0].clone(), vals[1].clone())
                };
                binary(op, &a, &b)
            }
            "fmod" => {
                nargs(2..=2)?;
                let dt = function_type(&vals[0], &vals[1])?;
                let strict = !dt.is_float();
                let (x, y) = (vals[0].to(dt, strict)?, vals[1].to(dt, strict)?);
                arith(Ibin::Fmod, &x, &y, dt).map(Val::Arr)
            }
            "arctan2" | "hypot" | "copysign" => {
                nargs(2..=2)?;
                let dt = function_type(&vals[0], &vals[1])?.float_of();
                let (x, y) = (vals[0].to(dt, false)?, vals[1].to(dt, false)?);
                let op = match canonical {
                    "arctan2" => Fbin::Atan2,
                    "hypot" => Fbin::Hypot,
                    _ => Fbin::Copysign,
                };
                float_binary(op, &x, &y, dt).map(Val::Arr)
            }
            _ => Err(Error::InvalidArgument(format!("unknown function '{name}'"))),
        }
    }
}

/// The type of a NumPy function's operands, Python numbers taking default types when all
/// operands are Python numbers.
fn function_type(a: &Val, b: &Val) -> Result<DType> {
    if a.is_weak() && b.is_weak() { Ok(promote(a.strong()?.dt, b.strong()?.dt)) } else { result_type(&[a, b]) }
}

/// A type named in an expression (`float32`, `np.uint8`, `"int16"`).
fn type_name(e: &Expr) -> Result<DType> {
    match e {
        Expr::Name(n) => DType::parse(n).ok_or_else(|| Error::InvalidArgument(format!("unknown data type '{n}'"))),
        _ => Err(Error::InvalidArgument("astype() needs a type name such as float32".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(dt: DType, v: &[f64]) -> Val {
        let data = if dt.is_float() { Data::F(v.to_vec()) } else { Data::I(v.iter().map(|x| *x as i128).collect()) };
        Val::Arr(Arr { dt, data })
    }

    fn eval(s: &str, a: &Val, b: &Val) -> Result<Val> {
        let (a, b) = (a.clone(), b.clone());
        let lookup = move |n: &str| match n {
            "A" => Some(a.clone()),
            "B" => Some(b.clone()),
            _ => None,
        };
        Evaluator { lookup: &lookup }.eval(&Expr::parse(s)?)
    }

    fn check(s: &str, a: &Val, b: &Val, dt: DType, expected: &[f64]) {
        let Val::Arr(r) = eval(s, a, b).unwrap() else { panic!("{s}: not an array") };
        assert_eq!(r.dt, dt, "{s}");
        let got: Vec<f64> = r.floats().into_owned();
        for (g, e) in got.iter().zip(expected) {
            assert!(g == e || g.is_nan() && e.is_nan(), "{s}: {got:?} != {expected:?}");
        }
    }

    #[test]
    fn numpy_typing() {
        let u8a = arr(DType::U8, &[200.0, 3.0]);
        let i16a = arr(DType::I16, &[-5.0, 7.0]);
        let f32a = arr(DType::F32, &[1.5, -2.5]);
        check("A + B", &u8a, &i16a, DType::I16, &[195.0, 10.0]);
        check("A * 2", &u8a, &i16a, DType::U8, &[144.0, 6.0]);
        check("A - 201", &u8a, &i16a, DType::U8, &[255.0, 58.0]);
        check("A > 300", &u8a, &i16a, DType::Bool, &[0.0, 0.0]);
        check("A / A", &i16a, &i16a, DType::F64, &[1.0, 1.0]);
        check("A ** 2", &u8a, &i16a, DType::U8, &[64.0, 9.0]);
        check("A + B", &f32a, &u8a, DType::F32, &[201.5, 0.5]);
        check("A + 1", &f32a, &u8a, DType::F32, &[2.5, -1.5]);
        check("A * 0.5", &u8a, &u8a, DType::F64, &[100.0, 1.5]);
        check("where(A > 100, A, -1)", &u8a, &u8a, DType::U8, &[200.0, 255.0]);
        check("where(A > 100, 1, 2)", &u8a, &u8a, DType::I64, &[1.0, 2.0]);
        check("-A", &u8a, &u8a, DType::U8, &[56.0, 253.0]);
        check("~A", &u8a, &u8a, DType::U8, &[55.0, 252.0]);
        check("A % 0", &u8a, &u8a, DType::U8, &[0.0, 0.0]);
        check("A // 0", &f32a, &u8a, DType::F32, &[f64::INFINITY, f64::NEG_INFINITY]);
        check("A % 0", &f32a, &u8a, DType::F32, &[f64::NAN, f64::NAN]);
        check("A // 2", &arr(DType::F32, &[-7.5]), &u8a, DType::F32, &[-4.0]);
        check("A // 2", &arr(DType::I16, &[-7.0]), &u8a, DType::I16, &[-4.0]);
        check("A % 3", &arr(DType::I16, &[-7.0]), &u8a, DType::I16, &[2.0]);
        check("fmod(A, B)", &arr(DType::I16, &[-5.0, 7.0]), &arr(DType::I16, &[3.0, -3.0]), DType::I16, &[-2.0, 1.0]);
        check("clip(A, 0, 300)", &u8a, &u8a, DType::U8, &[200.0, 3.0]);
        check("round(A)", &f32a, &u8a, DType::F32, &[2.0, -2.0]);
        check("A.astype(float32) / 2", &u8a, &u8a, DType::F32, &[100.0, 1.5]);
        check("logical_and(A > 100, B < 0) | (A == 3)", &u8a, &f32a, DType::Bool, &[0.0, 1.0]);
        check("A > 100 and B < 0 or A == 3", &u8a, &f32a, DType::Bool, &[0.0, 1.0]);
        check("1 < A < 100", &u8a, &u8a, DType::Bool, &[0.0, 1.0]);
        check("arctan2(A, 2)", &u8a, &u8a, DType::F16, &[1.560546875, 0.98291015625]);
        let Val::Arr(s) = eval("sqrt(A)", &u8a, &u8a).unwrap() else { panic!() };
        assert_eq!(s.dt, DType::F16);
        assert_eq!(s.floats()[0], f16::from_f64(200f64.sqrt()).to_f64());
        assert!(eval("A + 300", &u8a, &u8a).is_err());
        assert!(eval("B ** -1", &u8a, &i16a).is_err());
        assert!(eval("A & 1", &f32a, &u8a).is_err());
        assert!(eval("C + 1", &u8a, &u8a).is_err());
        assert_eq!(eval("2 ** -1", &u8a, &u8a).unwrap(), Val::Float(0.5));
        assert_eq!(eval("7 // 2 + pi * 0", &u8a, &u8a).unwrap(), Val::Float(3.0));
    }

    #[test]
    fn promotion_table() {
        use DType::*;
        assert_eq!(promote(U8, I8), I16);
        assert_eq!(promote(U16, I8), I32);
        assert_eq!(promote(U32, I32), I64);
        assert_eq!(promote(U64, I64), F64);
        assert_eq!(promote(I16, F16), F32);
        assert_eq!(promote(U8, F16), F16);
        assert_eq!(promote(I32, F32), F64);
        assert_eq!(promote(Bool, U16), U16);
    }
}
