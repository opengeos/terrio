//! Creating a raster from scratch or like a template, as `gdal_create` and `gdal raster create`
//! do: its size, bands, data type, CRS, extent, nodata value and metadata, every band filled with
//! a burn value (else the nodata value, else 0).
//!
//! ```no_run
//! use terrio::ops::create::{CreateOptions, create_raster_file};
//!
//! // gdal raster create --size 1000,800 --band-count 3 --ot UInt16 --burn 1 --burn 2 --burn 3 out.tif
//! let o = CreateOptions {
//!     size: Some((1000, 800)),
//!     band_count: Some(3),
//!     data_type: Some(terrio::DataType::U16),
//!     burn: vec![1.0, 2.0, 3.0],
//!     ..Default::default()
//! };
//! create_raster_file("out.tif", None, &o, &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::crs::SpatialRef;
use crate::{Error, LazyRaster, RasterDataset, RasterWriteOptions, Result};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// What [`create_raster`] creates. Without a template, `size` is required; with one, the options
/// given override what is taken from it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CreateOptions {
    /// The width and height in pixels.
    pub size: Option<(usize, usize)>,
    /// The number of bands (1).
    pub band_count: Option<usize>,
    /// The data type (Byte).
    pub data_type: Option<DataType>,
    /// The CRS.
    pub crs: Option<SpatialRef>,
    /// The extent `[xmin, ymin, xmax, ymax]`; without one (and no template), the raster is not
    /// georeferenced.
    pub bbox: Option<[f64; 4]>,
    /// The nodata value of every band.
    pub nodata: Option<f64>,
    /// No nodata value, even when the template has one (`--nodata none`).
    pub unset_nodata: bool,
    /// The value of every pixel: one for every band, or one per band. By default the nodata
    /// value, else 0.
    pub burn: Vec<f64>,
    /// Dataset metadata items to add.
    pub metadata: Vec<(String, String)>,
    /// Copy the template's dataset and band metadata.
    pub copy_metadata: bool,
    /// Build the template's overview levels (GeoTIFF outputs), computed from the burnt values.
    pub copy_overviews: bool,
    /// Replace an existing output after validating the creation parameters.
    pub overwrite: bool,
}

/// The header (a dataset without pixels) and fill values of the raster to create.
fn plan(like: Option<&LazyRaster>, o: &CreateOptions) -> Result<(RasterDataset, Vec<f64>)> {
    let (w, h) = match (o.size, like) {
        (Some(s), _) => s,
        (None, Some(l)) => (l.width(), l.height()),
        (None, None) => return Err(Error::InvalidArgument("the size of the raster to create is needed (or a template)".into())),
    };
    if w == 0 || h == 0 {
        return Err(Error::InvalidArgument("the raster to create cannot be empty".into()));
    }
    let bands = o.band_count.or(like.map(|l| l.band_count())).unwrap_or(1);
    if bands == 0 {
        return Err(Error::InvalidArgument("the raster to create needs a band".into()));
    }
    if (o.copy_metadata || o.copy_overviews) && like.is_none() {
        return Err(Error::InvalidArgument("copying metadata or overviews needs a template (like)".into()));
    }
    let data_type = o.data_type.or(like.map(|l| l.data_type())).unwrap_or(DataType::U8);
    let nodata = if o.unset_nodata { None } else { o.nodata.or(like.and_then(|l| l.nodata())) };
    let crs = o.crs.clone().unwrap_or_else(|| like.map(|l| l.crs()).unwrap_or_default());
    let gt = match (o.bbox, like) {
        (Some(b), _) => {
            if !(b.iter().all(|v| v.is_finite()) && b[0] < b[2] && b[1] < b[3]) {
                return Err(Error::InvalidArgument("the bounding box is not xmin,ymin,xmax,ymax".into()));
            }
            Some([b[0], (b[2] - b[0]) / w as f64, 0.0, b[3], 0.0, -(b[3] - b[1]) / h as f64])
        }
        // The template's geotransform, whatever the size (as GDAL keeps it), when it has one.
        (None, Some(l)) => l.has_geo_transform().then(|| l.geo_transform()),
        (None, None) => None,
    };
    let fill = match o.burn.len() {
        0 => vec![nodata.unwrap_or(0.0); bands],
        1 => vec![o.burn[0]; bands],
        n if n == bands => o.burn.clone(),
        n => return Err(Error::InvalidArgument(format!("{n} burn values for {bands} band(s): give one, or one per band"))),
    };
    let fill: Vec<f64> = fill.into_iter().map(|v| super::convert_value(v, data_type)).collect();
    let grid_gt = match gt {
        Some(gt) if crate::is_north_up(&gt) && gt[1] > 0.0 && gt[5] < 0.0 => gt,
        _ => [0.0, 1.0, 0.0, h as f64, 0.0, -1.0],
    };
    let grid = super::Grid { width: w, height: h, geo_transform: grid_gt };
    let mut header = super::raster_header(grid, data_type, bands, nodata, &crs)?;
    if let Some(gt) = gt.filter(|g| *g != grid_gt) {
        header.set_geo_transform(gt);
    }
    if gt.is_none() {
        // Not georeferenced: written without a geotransform, with a template's GCPs if it has any.
        header.raster.metadata.push((NOT_GEOREFERENCED.into(), "YES".into()));
        header.gcps = like.map(|l| l.header().gcps).unwrap_or_default();
    }
    if o.copy_metadata
        && let Some(l) = like
    {
        let src = l.header();
        header.raster.metadata.extend(src.raster.metadata.iter().filter(|(k, _)| k != "AREA_OR_POINT").cloned());
        header.band_metadata = src.band_metadata.iter().take(bands).cloned().collect();
    }
    if o.metadata.iter().any(|(k, _)| k == NOT_GEOREFERENCED) {
        return Err(Error::InvalidArgument(format!("'{NOT_GEOREFERENCED}' is a reserved metadata key")));
    }
    for (k, v) in &o.metadata {
        header.raster.metadata.retain(|(n, _)| n != k);
        header.raster.metadata.push((k.clone(), v.clone()));
    }
    Ok((header, fill))
}

/// The marker of a raster without georeferencing, taken out before it is written.
const NOT_GEOREFERENCED: &str = "_TERRIO_NOT_GEOREFERENCED";

/// Creates a raster in memory.
pub fn create_raster(like: Option<&LazyRaster>, options: &CreateOptions) -> Result<RasterDataset> {
    let (mut header, fill) = plan(like, options)?;
    header.raster.metadata.retain(|(k, _)| k != NOT_GEOREFERENCED);
    let n = header.raster.cols * header.raster.rows;
    let mut data = RasterData::new_filled(header.raster.data_type, n * fill.len(), 0.0);
    for (b, v) in fill.iter().enumerate() {
        fill_band(&mut data, b * n..(b + 1) * n, *v);
    }
    header.raster.data = data;
    Ok(header)
}

fn fill_band(data: &mut RasterData, range: std::ops::Range<usize>, v: f64) {
    crate::pixel::with_data!(data, d => {
        let x = crate::pixel::Pixel::from_f64(v);
        d[range].fill(x);
    });
}

/// Creates a raster file at `path` (refusing an existing one), written a strip of rows at a time.
/// `like` is the template's path. With `copy_overviews`, the template's overview levels are built
/// (nearest), for GeoTIFF outputs.
pub fn create_raster_file(path: &str, like: Option<&str>, options: &CreateOptions, write: &RasterWriteOptions) -> Result<()> {
    let template = like.map(crate::open_raster_lazy).transpose()?;
    create_raster_file_from_template(path, template.as_ref(), options, write)
}

/// Creates a raster file from an already-open template without materializing its pixels.
pub fn create_raster_file_from_template(
    path: &str,
    template: Option<&LazyRaster>,
    options: &CreateOptions,
    write: &RasterWriteOptions,
) -> Result<()> {
    if std::path::Path::new(path).exists() && !options.overwrite {
        return Err(Error::InvalidArgument(format!("'{path}' exists: remove it first, or allow overwriting")));
    }
    if options.copy_overviews
        && write.driver.or_else(|| crate::formats::RasterDriver::for_extension(path)) != Some(crate::formats::RasterDriver::GTiff)
    {
        return Err(Error::Unsupported("copy-overviews requires a GeoTIFF output".into()));
    }
    let (mut header, fill) = plan(template, options)?;
    let mut write = write.clone();
    if header.raster.metadata.iter().any(|(k, _)| k == NOT_GEOREFERENCED) {
        header.raster.metadata.retain(|(k, _)| k != NOT_GEOREFERENCED);
        write.creation_options.push(("GEOREFERENCED".into(), "NO".into()));
    }
    let (w, h, bands) = (header.raster.cols, header.raster.rows, header.raster.bands);
    let rows = super::strips::strip_rows(w, bands, header.raster.data_type.size_bytes(), None);
    let mut out = super::strips::StripWriter::create(&header, path, &write)?;
    let mut y = 0;
    while y < h {
        let n = rows.min(h - y);
        let mut data = RasterData::new_filled(header.raster.data_type, w * n * bands, 0.0);
        for (b, v) in fill.iter().enumerate() {
            fill_band(&mut data, b * w * n..(b + 1) * w * n, *v);
        }
        out.write(data, n)?;
        y += n;
    }
    out.finish()?;
    if options.copy_overviews
        && let Some(t) = template
        && !t.overviews().is_empty()
    {
        let full = (t.width(), t.height());
        let levels = t.overviews().iter().map(|o| super::overview::overview_factor(*o, full)).collect();
        let o = super::overview::OverviewOptions { levels, ..Default::default() };
        super::overview::add_overviews(path, &o)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_with_burn_values_then_nodata() {
        let o = CreateOptions {
            size: Some((4, 3)),
            band_count: Some(2),
            data_type: Some(DataType::I16),
            nodata: Some(-99.0),
            ..Default::default()
        };
        let ds = create_raster(None, &o).unwrap();
        assert_eq!(ds.raster.data.to_f64_vec(), vec![-99.0; 24]);
        let o = CreateOptions { burn: vec![1.6, 300.0], data_type: Some(DataType::U8), ..o };
        let ds = create_raster(None, &o).unwrap();
        assert_eq!(&ds.raster.data.to_f64_vec()[..12], &[2.0; 12]);
        assert_eq!(&ds.raster.data.to_f64_vec()[12..], &[255.0; 12]);
        assert!(create_raster(None, &CreateOptions { burn: vec![1.0, 2.0, 3.0], ..o }).is_err());
        assert!(create_raster(None, &CreateOptions::default()).is_err());
    }

    #[test]
    fn templates_without_a_geotransform_keep_their_gcps_or_absence() {
        use crate::drivers::gtiff_write as gw;
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 1, data_type: DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data(cfg, vec![1.0; 12]).unwrap();
        let gcps: Vec<crate::Gcp> = [(0.0, 0.0), (4.0, 0.0), (0.0, 3.0)]
            .map(|(pixel, line)| crate::Gcp { pixel, line, x: 10.0 + pixel, y: 20.0 - line, ..Default::default() })
            .to_vec();
        let dir = tempfile::tempdir().unwrap();
        for (name, opts) in [
            ("gcps", gw::TiffOptions { gcps: gcps.clone(), ..Default::default() }),
            ("none", gw::TiffOptions { georeferenced: false, ..Default::default() }),
        ] {
            let like = LazyRaster::from_tiff_bytes(gw::encode(&r, &opts).unwrap()).unwrap().unwrap();
            assert!(!like.has_geo_transform());
            let out = dir.path().join(format!("{name}.tif"));
            create_raster_file_from_template(out.to_str().unwrap(), Some(&like), &CreateOptions::default(), &Default::default()).unwrap();
            let created = crate::open_raster_lazy(out.to_str().unwrap()).unwrap();
            assert!(!created.has_geo_transform(), "{name}");
            assert_eq!(created.header().gcps, like.header().gcps, "{name}");
            assert_eq!(created.header().gcps.len(), opts.gcps.len(), "{name}");
        }
    }

    #[test]
    fn copy_options_need_a_template() {
        for o in [CreateOptions { copy_metadata: true, ..Default::default() }, CreateOptions { copy_overviews: true, ..Default::default() }]
        {
            let o = CreateOptions { size: Some((2, 2)), ..o };
            assert!(matches!(create_raster(None, &o), Err(Error::InvalidArgument(_))));
        }
    }

    #[test]
    fn reserved_not_georeferenced_key_is_rejected() {
        let o = CreateOptions { size: Some((2, 2)), metadata: vec![(NOT_GEOREFERENCED.into(), "x".into())], ..Default::default() };
        assert!(create_raster(None, &o).is_err());
    }
}
