// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_translate.rst
// No code is translated from these sources.

//! Output options shared by the raster processing commands.

use terrio::formats::RasterDriver;
use terrio::{RasterDataset, RasterWriteOptions};

/// How to write an output raster.
#[derive(clap::Args)]
pub struct OutputArgs {
    /// The output format's short name (`GTiff`, `COG`...). By default, chosen from the output's
    /// extension.
    #[arg(short = 'f', long = "format", visible_alias = "of")]
    pub format: Option<String>,
    /// GeoTIFF/COG compression: NONE, LZW, DEFLATE, ZSTD, PACKBITS, LERC, LERC_DEFLATE, LERC_ZSTD, JXL, JPEG, WEBP. GeoPackage tiles: PNG, JPEG.
    #[arg(long)]
    pub compress: Option<String>,
    /// GeoTIFF/COG tile size.
    #[arg(long)]
    pub tile_size: Option<u32>,
    /// Write BigTIFF.
    #[arg(long)]
    pub bigtiff: bool,
    /// A creation option, as GDAL names it: `KEY=VALUE` (Zarr: FORMAT=ZARR_V2|ZARR_V3; netCDF:
    /// FORMAT=NC|NC2|NC5|NC4|NC4C, COMPRESS=DEFLATE, ZLEVEL=1..9, CHUNKING=YES|NO).
    #[arg(long = "co", value_name = "KEY=VALUE")]
    pub creation_options: Vec<String>,
}

impl OutputArgs {
    /// Writes a raster to `path`.
    pub fn write(&self, ds: &RasterDataset, path: &str) -> terrio::Result<()> {
        terrio::write_raster_dataset(ds, path, &self.options()?)
    }

    /// The write options.
    pub fn options(&self) -> terrio::Result<RasterWriteOptions> {
        let driver = self
            .format
            .as_deref()
            .map(|f| RasterDriver::from_name(f).ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown raster format '{f}'"))))
            .transpose()?;
        let creation_options =
            super::convert::key_values(&self.creation_options)?.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Ok(RasterWriteOptions {
            driver,
            compression: self.compress.clone(),
            tile_size: self.tile_size,
            bigtiff: self.bigtiff,
            creation_options,
            ..Default::default()
        })
    }
}

/// Opens a raster quietly, for reading by windows (GeoTIFFs are read in place; other formats
/// are read whole).
pub fn open_lazy(path: &str) -> terrio::Result<terrio::LazyRaster> {
    super::quiet(|| terrio::open_raster_lazy(path))
}

/// Whether two paths are the same existing file, under any name.
pub fn same_file(a: &str, b: &str) -> bool {
    matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Opens a raster quietly.
pub fn open(path: &str) -> terrio::Result<RasterDataset> {
    super::quiet(|| terrio::open_raster(path))
}

/// Parses an optional CRS argument.
pub fn srs(s: &Option<String>) -> terrio::Result<Option<terrio::crs::SpatialRef>> {
    s.as_deref().map(terrio::crs::SpatialRef::parse).transpose()
}

/// Parses a bounding box `xmin,ymin,xmax,ymax`, as GDAL's `--bbox` takes it.
pub fn parse_bbox(s: &str) -> terrio::Result<[f64; 4]> {
    let v: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| terrio::Error::InvalidArgument(format!("bounding box '{s}' is not xmin,ymin,xmax,ymax")))?;
    match v.as_slice() {
        [x0, y0, x1, y1] if x0 < x1 && y0 < y1 => Ok([*x0, *y0, *x1, *y1]),
        _ => Err(terrio::Error::InvalidArgument(format!("bounding box '{s}' is not xmin,ymin,xmax,ymax with xmin < xmax and ymin < ymax"))),
    }
}
