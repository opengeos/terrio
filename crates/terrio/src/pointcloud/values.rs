//! Columns of point values: one typed vector per dimension.

use serde::Serialize;

/// The type of a dimension's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueType {
    /// Unsigned 8-bit integers.
    U8,
    /// Signed 8-bit integers.
    I8,
    /// Unsigned 16-bit integers.
    U16,
    /// Signed 16-bit integers.
    I16,
    /// Unsigned 32-bit integers.
    U32,
    /// Signed 32-bit integers.
    I32,
    /// Unsigned 64-bit integers.
    U64,
    /// Signed 64-bit integers.
    I64,
    /// 32-bit floats.
    F32,
    /// 64-bit floats.
    F64,
}

impl ValueType {
    /// The size of one value in bytes.
    pub fn size(self) -> usize {
        match self {
            ValueType::U8 | ValueType::I8 => 1,
            ValueType::U16 | ValueType::I16 => 2,
            ValueType::U32 | ValueType::I32 | ValueType::F32 => 4,
            ValueType::U64 | ValueType::I64 | ValueType::F64 => 8,
        }
    }

    /// The name, as NumPy names it (`uint8`, `float64`...).
    pub fn name(self) -> &'static str {
        match self {
            ValueType::U8 => "uint8",
            ValueType::I8 => "int8",
            ValueType::U16 => "uint16",
            ValueType::I16 => "int16",
            ValueType::U32 => "uint32",
            ValueType::I32 => "int32",
            ValueType::U64 => "uint64",
            ValueType::I64 => "int64",
            ValueType::F32 => "float32",
            ValueType::F64 => "float64",
        }
    }

    /// The LAS extra-bytes data type code (1 to 10).
    pub(crate) fn las_code(self) -> u8 {
        match self {
            ValueType::U8 => 1,
            ValueType::I8 => 2,
            ValueType::U16 => 3,
            ValueType::I16 => 4,
            ValueType::U32 => 5,
            ValueType::I32 => 6,
            ValueType::U64 => 7,
            ValueType::I64 => 8,
            ValueType::F32 => 9,
            ValueType::F64 => 10,
        }
    }

    /// The type of a LAS extra-bytes data type code (1 to 10, or the 2- and 3-element array codes
    /// 11 to 30).
    pub(crate) fn from_las_code(code: u8) -> Option<(ValueType, usize)> {
        let (base, count) = match code {
            1..=10 => (code, 1),
            11..=20 => (code - 10, 2),
            21..=30 => (code - 20, 3),
            _ => return None,
        };
        let t = match base {
            1 => ValueType::U8,
            2 => ValueType::I8,
            3 => ValueType::U16,
            4 => ValueType::I16,
            5 => ValueType::U32,
            6 => ValueType::I32,
            7 => ValueType::U64,
            8 => ValueType::I64,
            9 => ValueType::F32,
            _ => ValueType::F64,
        };
        Some((t, count))
    }
}

/// A dimension's values, one per point.
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    /// Unsigned 8-bit integers.
    U8(Vec<u8>),
    /// Signed 8-bit integers.
    I8(Vec<i8>),
    /// Unsigned 16-bit integers.
    U16(Vec<u16>),
    /// Signed 16-bit integers.
    I16(Vec<i16>),
    /// Unsigned 32-bit integers.
    U32(Vec<u32>),
    /// Signed 32-bit integers.
    I32(Vec<i32>),
    /// Unsigned 64-bit integers.
    U64(Vec<u64>),
    /// Signed 64-bit integers.
    I64(Vec<i64>),
    /// 32-bit floats.
    F32(Vec<f32>),
    /// 64-bit floats.
    F64(Vec<f64>),
}

/// Applies `$body` to the vector inside a [`Values`], whatever its type.
macro_rules! each {
    ($v:expr, $x:ident => $body:expr) => {
        match $v {
            Values::U8($x) => $body,
            Values::I8($x) => $body,
            Values::U16($x) => $body,
            Values::I16($x) => $body,
            Values::U32($x) => $body,
            Values::I32($x) => $body,
            Values::U64($x) => $body,
            Values::I64($x) => $body,
            Values::F32($x) => $body,
            Values::F64($x) => $body,
        }
    };
}

impl Values {
    /// An empty column of a type, with room for `capacity` values.
    pub fn with_capacity(t: ValueType, capacity: usize) -> Self {
        match t {
            ValueType::U8 => Values::U8(Vec::with_capacity(capacity)),
            ValueType::I8 => Values::I8(Vec::with_capacity(capacity)),
            ValueType::U16 => Values::U16(Vec::with_capacity(capacity)),
            ValueType::I16 => Values::I16(Vec::with_capacity(capacity)),
            ValueType::U32 => Values::U32(Vec::with_capacity(capacity)),
            ValueType::I32 => Values::I32(Vec::with_capacity(capacity)),
            ValueType::U64 => Values::U64(Vec::with_capacity(capacity)),
            ValueType::I64 => Values::I64(Vec::with_capacity(capacity)),
            ValueType::F32 => Values::F32(Vec::with_capacity(capacity)),
            ValueType::F64 => Values::F64(Vec::with_capacity(capacity)),
        }
    }

    /// A column of `n` zeros.
    pub fn zeros(t: ValueType, n: usize) -> Self {
        match t {
            ValueType::U8 => Values::U8(vec![0; n]),
            ValueType::I8 => Values::I8(vec![0; n]),
            ValueType::U16 => Values::U16(vec![0; n]),
            ValueType::I16 => Values::I16(vec![0; n]),
            ValueType::U32 => Values::U32(vec![0; n]),
            ValueType::I32 => Values::I32(vec![0; n]),
            ValueType::U64 => Values::U64(vec![0; n]),
            ValueType::I64 => Values::I64(vec![0; n]),
            ValueType::F32 => Values::F32(vec![0.0; n]),
            ValueType::F64 => Values::F64(vec![0.0; n]),
        }
    }

    /// The type of the values.
    pub fn value_type(&self) -> ValueType {
        match self {
            Values::U8(_) => ValueType::U8,
            Values::I8(_) => ValueType::I8,
            Values::U16(_) => ValueType::U16,
            Values::I16(_) => ValueType::I16,
            Values::U32(_) => ValueType::U32,
            Values::I32(_) => ValueType::I32,
            Values::U64(_) => ValueType::U64,
            Values::I64(_) => ValueType::I64,
            Values::F32(_) => ValueType::F32,
            Values::F64(_) => ValueType::F64,
        }
    }

    /// The number of values.
    pub fn len(&self) -> usize {
        each!(self, v => v.len())
    }

    /// Whether there are no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The value at `i` as a float.
    #[allow(clippy::unnecessary_cast)]
    pub fn get_f64(&self, i: usize) -> f64 {
        each!(self, v => v[i] as f64)
    }

    /// All values as floats.
    #[allow(clippy::unnecessary_cast)]
    pub fn to_f64(&self) -> Vec<f64> {
        each!(self, v => v.iter().map(|x| *x as f64).collect())
    }

    /// The values converted to another type (saturating, as Rust's `as` does).
    pub fn cast(&self, t: ValueType) -> Values {
        if self.value_type() == t {
            return self.clone();
        }
        let mut v = Values::with_capacity(t, self.len());
        for i in 0..self.len() {
            v.push_f64(self.get_f64(i));
        }
        v
    }

    /// Appends the little-endian value at the start of `bytes`.
    pub(crate) fn push_le(&mut self, bytes: &[u8]) {
        match self {
            Values::U8(v) => v.push(bytes[0]),
            Values::I8(v) => v.push(bytes[0] as i8),
            Values::U16(v) => v.push(u16::from_le_bytes([bytes[0], bytes[1]])),
            Values::I16(v) => v.push(i16::from_le_bytes([bytes[0], bytes[1]])),
            Values::U32(v) => v.push(u32::from_le_bytes(bytes[..4].try_into().unwrap_or_default())),
            Values::I32(v) => v.push(i32::from_le_bytes(bytes[..4].try_into().unwrap_or_default())),
            Values::U64(v) => v.push(u64::from_le_bytes(bytes[..8].try_into().unwrap_or_default())),
            Values::I64(v) => v.push(i64::from_le_bytes(bytes[..8].try_into().unwrap_or_default())),
            Values::F32(v) => v.push(f32::from_le_bytes(bytes[..4].try_into().unwrap_or_default())),
            Values::F64(v) => v.push(f64::from_le_bytes(bytes[..8].try_into().unwrap_or_default())),
        }
    }

    /// Writes the value at `i`, little-endian, at the start of `out`.
    pub(crate) fn write_le(&self, i: usize, out: &mut [u8]) {
        match self {
            Values::U8(v) => out[0] = v[i],
            Values::I8(v) => out[0] = v[i] as u8,
            Values::U16(v) => out[..2].copy_from_slice(&v[i].to_le_bytes()),
            Values::I16(v) => out[..2].copy_from_slice(&v[i].to_le_bytes()),
            Values::U32(v) => out[..4].copy_from_slice(&v[i].to_le_bytes()),
            Values::I32(v) => out[..4].copy_from_slice(&v[i].to_le_bytes()),
            Values::U64(v) => out[..8].copy_from_slice(&v[i].to_le_bytes()),
            Values::I64(v) => out[..8].copy_from_slice(&v[i].to_le_bytes()),
            Values::F32(v) => out[..4].copy_from_slice(&v[i].to_le_bytes()),
            Values::F64(v) => out[..8].copy_from_slice(&v[i].to_le_bytes()),
        }
    }

    /// Appends the values of `other`, which must be of the same type.
    pub(crate) fn extend_from(&mut self, other: &Values) {
        match (self, other) {
            (Values::U8(a), Values::U8(b)) => a.extend_from_slice(b),
            (Values::I8(a), Values::I8(b)) => a.extend_from_slice(b),
            (Values::U16(a), Values::U16(b)) => a.extend_from_slice(b),
            (Values::I16(a), Values::I16(b)) => a.extend_from_slice(b),
            (Values::U32(a), Values::U32(b)) => a.extend_from_slice(b),
            (Values::I32(a), Values::I32(b)) => a.extend_from_slice(b),
            (Values::U64(a), Values::U64(b)) => a.extend_from_slice(b),
            (Values::I64(a), Values::I64(b)) => a.extend_from_slice(b),
            (Values::F32(a), Values::F32(b)) => a.extend_from_slice(b),
            (Values::F64(a), Values::F64(b)) => a.extend_from_slice(b),
            (a, b) => {
                // Different types: convert through f64.
                let n = b.len();
                for i in 0..n {
                    a.push_f64(b.get_f64(i));
                }
            }
        }
    }

    /// Appends a value given as a float, converted (saturating) to the column's type.
    pub(crate) fn push_f64(&mut self, x: f64) {
        match self {
            Values::U8(v) => v.push(x as u8),
            Values::I8(v) => v.push(x as i8),
            Values::U16(v) => v.push(x as u16),
            Values::I16(v) => v.push(x as i16),
            Values::U32(v) => v.push(x as u32),
            Values::I32(v) => v.push(x as i32),
            Values::U64(v) => v.push(x as u64),
            Values::I64(v) => v.push(x as i64),
            Values::F32(v) => v.push(x as f32),
            Values::F64(v) => v.push(x),
        }
    }

    /// The values from `start` to `end` (excluded).
    pub fn slice(&self, start: usize, end: usize) -> Values {
        match self {
            Values::U8(v) => Values::U8(v[start..end].to_vec()),
            Values::I8(v) => Values::I8(v[start..end].to_vec()),
            Values::U16(v) => Values::U16(v[start..end].to_vec()),
            Values::I16(v) => Values::I16(v[start..end].to_vec()),
            Values::U32(v) => Values::U32(v[start..end].to_vec()),
            Values::I32(v) => Values::I32(v[start..end].to_vec()),
            Values::U64(v) => Values::U64(v[start..end].to_vec()),
            Values::I64(v) => Values::I64(v[start..end].to_vec()),
            Values::F32(v) => Values::F32(v[start..end].to_vec()),
            Values::F64(v) => Values::F64(v[start..end].to_vec()),
        }
    }

    /// The values at these positions, in this order.
    pub fn take(&self, idx: &[usize]) -> Values {
        fn t<T: Copy>(v: &[T], idx: &[usize]) -> Vec<T> {
            idx.iter().map(|&i| v[i]).collect()
        }
        match self {
            Values::U8(v) => Values::U8(t(v, idx)),
            Values::I8(v) => Values::I8(t(v, idx)),
            Values::U16(v) => Values::U16(t(v, idx)),
            Values::I16(v) => Values::I16(t(v, idx)),
            Values::U32(v) => Values::U32(t(v, idx)),
            Values::I32(v) => Values::I32(t(v, idx)),
            Values::U64(v) => Values::U64(t(v, idx)),
            Values::I64(v) => Values::I64(t(v, idx)),
            Values::F32(v) => Values::F32(t(v, idx)),
            Values::F64(v) => Values::F64(t(v, idx)),
        }
    }

    /// Sets the value at `i` from a float, converted (saturating) to the column's type.
    pub fn set_f64(&mut self, i: usize, x: f64) {
        match self {
            Values::U8(v) => v[i] = x as u8,
            Values::I8(v) => v[i] = x as i8,
            Values::U16(v) => v[i] = x as u16,
            Values::I16(v) => v[i] = x as i16,
            Values::U32(v) => v[i] = x as u32,
            Values::I32(v) => v[i] = x as i32,
            Values::U64(v) => v[i] = x as u64,
            Values::I64(v) => v[i] = x as i64,
            Values::F32(v) => v[i] = x as f32,
            Values::F64(v) => v[i] = x,
        }
    }

    /// The values at the positions where `keep` is true.
    pub(crate) fn select(&self, keep: &[bool]) -> Values {
        fn sel<T: Copy>(v: &[T], keep: &[bool]) -> Vec<T> {
            v.iter().zip(keep).filter(|(_, k)| **k).map(|(x, _)| *x).collect()
        }
        match self {
            Values::U8(v) => Values::U8(sel(v, keep)),
            Values::I8(v) => Values::I8(sel(v, keep)),
            Values::U16(v) => Values::U16(sel(v, keep)),
            Values::I16(v) => Values::I16(sel(v, keep)),
            Values::U32(v) => Values::U32(sel(v, keep)),
            Values::I32(v) => Values::I32(sel(v, keep)),
            Values::U64(v) => Values::U64(sel(v, keep)),
            Values::I64(v) => Values::I64(sel(v, keep)),
            Values::F32(v) => Values::F32(sel(v, keep)),
            Values::F64(v) => Values::F64(sel(v, keep)),
        }
    }

    /// The minimum and maximum, ignoring NaNs. `None` when there is no value.
    pub fn min_max(&self) -> Option<(f64, f64)> {
        let mut r: Option<(f64, f64)> = None;
        for i in 0..self.len() {
            let x = self.get_f64(i);
            if x.is_nan() {
                continue;
            }
            r = Some(match r {
                None => (x, x),
                Some((a, b)) => (a.min(x), b.max(x)),
            });
        }
        r
    }
}

/// One dimension of a point cloud: a named column of values.
#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    /// The name, as laspy names the standard LAS dimensions (`x`, `intensity`, `return_number`,
    /// `classification`, `gps_time`, `red`...), or an extra-bytes dimension's own name.
    pub name: String,
    /// The values, one per point. For `x`, `y` and `z`, the coordinates with the header's scale
    /// and offset applied; for extra-bytes dimensions, the stored values (see `scale`/`offset`).
    pub values: Values,
    /// For extra-bytes dimensions: the scale to apply to the stored values.
    pub scale: Option<f64>,
    /// For extra-bytes dimensions: the offset to add to the scaled values.
    pub offset: Option<f64>,
    /// For extra-bytes dimensions: the value that means "no data".
    pub no_data: Option<f64>,
    /// For extra-bytes dimensions: the description.
    pub description: String,
}

impl Dimension {
    /// A dimension without scale, offset or description.
    pub fn new(name: &str, values: Values) -> Self {
        Self { name: name.to_string(), values, scale: None, offset: None, no_data: None, description: String::new() }
    }

    /// A dimension of the same name, scale, offset, nodata and description with other values.
    pub fn with_values(&self, values: Values) -> Self {
        Self {
            name: self.name.clone(),
            values,
            scale: self.scale,
            offset: self.offset,
            no_data: self.no_data,
            description: self.description.clone(),
        }
    }

    /// The value at `i` as a float, with the scale and offset applied.
    pub fn get(&self, i: usize) -> f64 {
        let v = self.values.get_f64(i);
        v * self.scale.unwrap_or(1.0) + self.offset.unwrap_or(0.0)
    }

    /// All values as floats, with the scale and offset applied.
    pub fn to_f64(&self) -> Vec<f64> {
        (0..self.values.len()).map(|i| self.get(i)).collect()
    }

    /// Whether this is one of the standard LAS dimensions.
    pub fn is_standard(&self) -> bool {
        super::las::STANDARD_DIMENSIONS.contains(&self.name.as_str())
    }
}
