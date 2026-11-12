//! Typed pixel access: code generic over the sample types of `RasterData`, instead of converting
//! every pixel to `f64` and back.

/// A sample type of `RasterData`.
pub(crate) trait Pixel: Copy + Default + Send + Sync + PartialEq + 'static {
    /// The size of a sample in bytes.
    const SIZE: usize;
    /// The sample as `f64` (`as` conversion).
    fn to_f64(self) -> f64;
    /// A value as this type: integers rounded half away from zero and saturated (NaN becomes 0),
    /// `f32` rounded to single precision. The conversion of the GeoTIFF writer.
    fn from_f64(v: f64) -> Self;
    /// Writes the sample's little-endian bytes into `out` (`SIZE` bytes).
    fn put_le(self, out: &mut [u8]);
}

macro_rules! int_pixel {
    ($($t:ty),*) => {$(
        impl Pixel for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            #[inline]
            fn to_f64(self) -> f64 {
                self as f64
            }
            #[inline]
            fn from_f64(v: f64) -> Self {
                v.round() as $t
            }
            #[inline]
            fn put_le(self, out: &mut [u8]) {
                out.copy_from_slice(&self.to_le_bytes());
            }
        }
    )*};
}
int_pixel!(u8, i8, u16, i16, u32, i32, u64, i64);

impl Pixel for f32 {
    const SIZE: usize = 4;
    #[inline]
    fn to_f64(self) -> f64 {
        self as f64
    }
    #[inline]
    fn from_f64(v: f64) -> Self {
        v as f32
    }
    #[inline]
    fn put_le(self, out: &mut [u8]) {
        out.copy_from_slice(&self.to_le_bytes());
    }
}

impl Pixel for f64 {
    const SIZE: usize = 8;
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn from_f64(v: f64) -> Self {
        v
    }
    #[inline]
    fn put_le(self, out: &mut [u8]) {
        out.copy_from_slice(&self.to_le_bytes());
    }
}

/// Evaluates `$body` with `$v` bound to the typed vector inside a `RasterData` (by reference, or
/// by mutable reference with `mut`).
macro_rules! with_data {
    ($data:expr, $v:ident => $body:expr) => {
        match $data {
            RasterData::U8($v) => $body,
            RasterData::I8($v) => $body,
            RasterData::U16($v) => $body,
            RasterData::I16($v) => $body,
            RasterData::U32($v) => $body,
            RasterData::I32($v) => $body,
            RasterData::U64($v) => $body,
            RasterData::I64($v) => $body,
            RasterData::F32($v) => $body,
            RasterData::F64($v) => $body,
        }
    };
}
pub(crate) use with_data;

/// Collect real/imaginary component planes directly in their storage type, without a temporary
/// floating-point buffer. Integer conversion is rounded and saturated as in GDAL 3.13.3.
///
/// # Errors
/// When the target is not a supported complex component type.
pub fn complex_components_from_iter(
    components: impl Iterator<Item = f64>,
    target: wbraster::DataType,
) -> crate::Result<wbraster::raster::RasterData> {
    use wbraster::{DataType, raster::RasterData};
    macro_rules! cast {
        ($t:ty, $v:ident) => {
            RasterData::$v(components.map(<$t as Pixel>::from_f64).collect())
        };
    }
    Ok(match target {
        DataType::I16 => cast!(i16, I16),
        DataType::I32 => cast!(i32, I32),
        DataType::F32 => cast!(f32, F32),
        DataType::F64 => cast!(f64, F64),
        _ => return Err(crate::Error::InvalidArgument("complex components must be Int16, Int32, Float32 or Float64".into())),
    })
}

/// Builds a `RasterData` of the variant of `like` from a function generic over the sample type.
macro_rules! map_data {
    ($data:expr, $v:ident => $body:expr) => {
        match $data {
            RasterData::U8($v) => RasterData::U8($body),
            RasterData::I8($v) => RasterData::I8($body),
            RasterData::U16($v) => RasterData::U16($body),
            RasterData::I16($v) => RasterData::I16($body),
            RasterData::U32($v) => RasterData::U32($body),
            RasterData::I32($v) => RasterData::I32($body),
            RasterData::U64($v) => RasterData::U64($body),
            RasterData::I64($v) => RasterData::I64($body),
            RasterData::F32($v) => RasterData::F32($body),
            RasterData::F64($v) => RasterData::F64($body),
        }
    };
}
pub(crate) use map_data;

// The fields are mapped as `wbraster::Raster::new` maps them (wbraster 0.2.1): keep the two in
// step, which `raster_from_data_matches_from_data_native` checks.
/// A raster of `data` (band-major, of `cfg.data_type`), as `Raster::from_data_native` builds it
/// but without first allocating and filling a buffer of nodata values that `data` replaces.
///
/// # Errors
///
/// When the length of `data` is not `cols × rows × bands` (at least one band), or overflows, or
/// its type is not `cfg.data_type`. Like `from_data_native`, nothing else is checked: zero
/// columns or rows and any cell size are accepted as given.
pub fn raster_from_data(cfg: wbraster::RasterConfig, data: wbraster::raster::RasterData) -> crate::Result<wbraster::Raster> {
    let bands = cfg.bands.max(1);
    let Some(len) = cfg.cols.checked_mul(cfg.rows).and_then(|n| n.checked_mul(bands)) else {
        return Err(wbraster::RasterError::InvalidDimensions { cols: cfg.cols, rows: cfg.rows }.into());
    };
    if data.len() != len || cfg.data_type != data.data_type() {
        // Let wbraster report the error.
        return Ok(wbraster::Raster::from_data_native(cfg, data)?);
    }
    Ok(wbraster::Raster {
        cols: cfg.cols,
        rows: cfg.rows,
        bands,
        x_min: cfg.x_min,
        y_min: cfg.y_min,
        cell_size_x: cfg.cell_size,
        cell_size_y: cfg.cell_size_y.map(|v| v.abs()).unwrap_or(cfg.cell_size),
        nodata: cfg.nodata,
        data_type: cfg.data_type,
        crs: cfg.crs,
        metadata: cfg.metadata,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::{CrsInfo, DataType, Raster, RasterConfig, raster::RasterData};

    fn cfg() -> RasterConfig {
        RasterConfig {
            cols: 3,
            rows: 2,
            bands: 2,
            x_min: 10.0,
            y_min: -5.0,
            cell_size: 2.0,
            cell_size_y: Some(-3.0),
            nodata: -9.0,
            data_type: DataType::I16,
            crs: CrsInfo::from_epsg(32611),
            metadata: vec![("k".into(), "v".into())],
        }
    }

    #[test]
    fn raster_from_data_matches_from_data_native() {
        let configs = [
            cfg(),
            RasterConfig { cell_size_y: None, ..cfg() },
            RasterConfig { bands: 0, cols: 6, ..cfg() },
            RasterConfig { crs: CrsInfo::default(), metadata: Vec::new(), nodata: f64::NAN, ..cfg() },
            RasterConfig { cell_size: f64::NAN, cell_size_y: Some(f64::INFINITY), ..cfg() },
            RasterConfig { cell_size: 0.0, cell_size_y: Some(-0.0), ..cfg() },
            RasterConfig { cols: 0, ..cfg() },
            RasterConfig { rows: 0, bands: 0, ..cfg() },
        ];
        for c in configs {
            let n = c.cols * c.rows * c.bands.max(1);
            let data = || RasterData::I16((0..n as i16).collect());
            let ours = raster_from_data(c.clone(), data()).unwrap();
            let theirs = Raster::from_data_native(c, data()).unwrap();
            assert_eq!(format!("{ours:?}"), format!("{theirs:?}"));
        }
        // Mismatches are refused as wbraster refuses them.
        assert!(raster_from_data(cfg(), RasterData::I16(vec![0; 11])).is_err());
        assert!(raster_from_data(cfg(), RasterData::U16(vec![0; 12])).is_err());
    }

    #[test]
    fn raster_from_data_rejects_overflowing_dimensions() {
        let big = RasterConfig { cols: usize::MAX, rows: 2, bands: 1, ..cfg() };
        assert!(raster_from_data(big, RasterData::I16(Vec::new())).is_err());
    }
}
