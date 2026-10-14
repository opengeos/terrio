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

/// A raster of `data` (band-major, of `cfg.data_type`), as `Raster::from_data_native` builds it
/// but without first allocating and filling a buffer of nodata values that `data` replaces.
pub(crate) fn raster_from_data(cfg: wbraster::RasterConfig, data: wbraster::raster::RasterData) -> crate::Result<wbraster::Raster> {
    let bands = cfg.bands.max(1);
    if data.len() != cfg.cols * cfg.rows * bands || cfg.data_type != data.data_type() {
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
