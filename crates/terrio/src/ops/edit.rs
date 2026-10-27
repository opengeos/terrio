//! Editing a GeoTIFF in place, as `gdal_edit.py` and `gdal raster edit` do: its CRS,
//! geotransform (or ground control points), nodata value, metadata, band statistics and
//! histograms ([`edit_raster`]); and moving its georeferencing to another CRS without resampling,
//! as `gdalmove.py` does ([`move_raster`]).
//!
//! The changed directories are appended to the file and the header pointed at them (pixels are
//! not rewritten), through a copy renamed over the file: an error leaves it as it was. Histograms
//! go to the `.aux.xml` sidecar, as GDAL stores them. That sidecar is replaced atomically
//! after the TIFF: a histogram-sidecar error can leave the TIFF edit committed.
//!
//! ```no_run
//! use terrio::ops::edit::{EditOptions, edit_raster};
//!
//! // gdal_edit.py -a_srs EPSG:32613 -a_nodata -9999 -mo SOURCE=survey -stats dem.tif
//! let o = EditOptions {
//!     crs: Some(terrio::crs::SpatialRef::parse("EPSG:32613")?),
//!     nodata: Some(-9999.0),
//!     metadata: vec![("SOURCE".into(), "survey".into())],
//!     stats: true,
//!     ..Default::default()
//! };
//! edit_raster("dem.tif", &o)?;
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::crs::SpatialRef;
use crate::drivers::gtiff_update::{self, Appender, Entry};
use crate::drivers::gtiff_write::{self as gw, Value};
use crate::{Error, Gcp, LazyRaster, Result, Window, WindowOptions};
use std::io::{Read, Seek, Write};
use wbraster::DataType;

/// What [`edit_raster`] changes. Fields left at their default leave the raster as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditOptions {
    /// Assign this CRS, without reprojecting (`-a_srs`, `--crs`).
    pub crs: Option<SpatialRef>,
    /// Remove the CRS.
    pub unset_crs: bool,
    /// Set the geotransform (`-a_gt`).
    pub geo_transform: Option<[f64; 6]>,
    /// Set a north-up geotransform from the extent `[xmin, ymin, xmax, ymax]` (`--bbox`,
    /// `-a_ullr`).
    pub bbox: Option<[f64; 4]>,
    /// Georeference by these ground control points instead (`--gcp`).
    pub gcps: Vec<Gcp>,
    /// Set the nodata value of every band (`-a_nodata`, `--nodata`).
    pub nodata: Option<f64>,
    /// Remove the nodata value (`-unsetnodata`, `--nodata none`).
    pub unset_nodata: bool,
    /// Add or replace dataset metadata items (`-mo`, `--metadata`).
    pub metadata: Vec<(String, String)>,
    /// Remove dataset metadata items (`--unset-metadata`).
    pub unset_metadata: Vec<String>,
    /// Remove every dataset metadata item first (`-unsetmd`).
    pub unset_all_metadata: bool,
    /// Compute and store band statistics from every pixel (`-stats`, `--stats`).
    pub stats: bool,
    /// Compute them approximately, from an overview or from sampled blocks, as GDAL does
    /// (`-approx_stats`).
    pub approx_stats: bool,
    /// Remove the stored band statistics (`-unsetstats`).
    pub unset_stats: bool,
    /// Compute and store each band's default histogram in the `.aux.xml` sidecar (`--hist`).
    pub histogram: bool,
    /// Update a Cloud Optimized GeoTIFF, breaking its layout (GDAL's `IGNORE_COG_LAYOUT_BREAK`);
    /// refused otherwise.
    pub ignore_cog_layout_break: bool,
}

/// Edits a local GeoTIFF in place.
pub fn edit_raster(path: &str, options: &EditOptions) -> Result<()> {
    let file = gtiff_update::writable_local_file(path)?;
    validate(options)?;
    let mut src = open_tiff(path)?;
    let o = options.clone();
    gtiff_update::update_file(&file, move |f| {
        let out = edit_tiff(f, &mut src, &o);
        drop(src);
        out
    })?;
    if options.histogram {
        write_histograms(path)?;
    }
    Ok(())
}

/// [`edit_raster`] on a GeoTIFF held in memory: returns the edited file. Histograms, which go to
/// a sidecar, are refused.
pub fn edit_raster_bytes(tiff: &[u8], options: &EditOptions) -> Result<Vec<u8>> {
    validate(options)?;
    if options.histogram {
        return Err(Error::Unsupported("histograms are stored in a .aux.xml sidecar: edit a file to store them".into()));
    }
    let mut src = LazyRaster::from_tiff_bytes(tiff.to_vec())?.ok_or_else(|| Error::InvalidData("not a TIFF file".into()))?;
    let mut out = std::io::Cursor::new(tiff.to_vec());
    edit_tiff(&mut out, &mut src, options)?;
    Ok(out.into_inner())
}

fn validate(o: &EditOptions) -> Result<()> {
    let exclusive =
        |a: bool, b: bool, what: &str| if a && b { Err(Error::InvalidArgument(format!("{what} exclude each other"))) } else { Ok(()) };
    exclusive(o.crs.is_some(), o.unset_crs, "a CRS and unsetting the CRS")?;
    exclusive(o.geo_transform.is_some(), o.bbox.is_some(), "a geotransform and a bounding box")?;
    exclusive(o.geo_transform.is_some() || o.bbox.is_some(), !o.gcps.is_empty(), "a geotransform and ground control points")?;
    exclusive(o.nodata.is_some(), o.unset_nodata, "a nodata value and unsetting it")?;
    exclusive(o.stats || o.approx_stats, o.unset_stats, "computing statistics and unsetting them")?;
    if let Some(b) = o.bbox
        && !(b.iter().all(|v| v.is_finite()) && b[0] < b[2] && b[1] < b[3])
    {
        return Err(Error::InvalidArgument("the bounding box is not xmin,ymin,xmax,ymax".into()));
    }
    if let Some(gt) = o.geo_transform
        && (!gt.iter().all(|v| v.is_finite()) || gt[1] * gt[5] - gt[2] * gt[4] == 0.0)
    {
        return Err(Error::InvalidArgument("the geotransform needs finite values and a non-zero pixel size".into()));
    }
    if o.gcps.iter().any(|g| ![g.pixel, g.line, g.x, g.y, g.z].iter().all(|v| v.is_finite())) {
        return Err(Error::InvalidArgument("ground control points need finite values".into()));
    }
    Ok(())
}

fn open_tiff(path: &str) -> Result<LazyRaster> {
    let r = LazyRaster::open(path)?;
    if r.driver() != crate::formats::RasterDriver::GTiff || !r.is_lazy() {
        return Err(Error::Unsupported(format!("only GeoTIFFs are edited in place, not '{path}'")));
    }
    Ok(r)
}

/// A `GDAL_METADATA` item: its attributes and its text (with GDAL's CPL escaping, XML-unescaped
/// once).
#[derive(Debug, Clone)]
struct Item {
    name: String,
    sample: Option<String>,
    role: Option<String>,
    domain: Option<String>,
    text: String,
}

impl Item {
    fn dataset(name: &str, value: &str) -> Self {
        Self { name: name.into(), sample: None, role: None, domain: None, text: gw::xml_escape(value) }
    }

    fn band(band: usize, name: &str, value: &str) -> Self {
        Self { sample: Some(band.to_string()), ..Self::dataset(name, value) }
    }

    /// A dataset item of the default domain.
    fn is_dataset(&self) -> bool {
        self.sample.is_none() && self.role.is_none() && self.domain.as_deref().is_none_or(str::is_empty)
    }

    /// A band item of the default domain (no role), numbered from 0.
    fn band_index(&self) -> Option<usize> {
        if self.role.is_some() || self.domain.as_deref().is_some_and(|d| !d.is_empty()) {
            return None;
        }
        self.sample.as_deref()?.parse().ok()
    }
}

fn parse_items(xml: &str) -> Vec<Item> {
    gw::gdal_metadata_items(xml)
        .into_iter()
        .map(|e| Item {
            name: e.attr("name").unwrap_or_default().to_string(),
            sample: e.attr("sample").map(str::to_string),
            role: e.attr("role").map(str::to_string),
            domain: e.attr("domain").map(str::to_string),
            text: e.text.clone(),
        })
        .collect()
}

fn items_xml(items: &[Item]) -> String {
    let mut xml = String::from("<GDALMetadata>\n");
    for i in items {
        let mut attrs = format!("name=\"{}\"", gw::xml_escape(&i.name));
        if let Some(d) = &i.domain {
            attrs.push_str(&format!(" domain=\"{}\"", gw::xml_escape(d)));
        }
        if let Some(s) = &i.sample {
            attrs.push_str(&format!(" sample=\"{s}\""));
        }
        if let Some(r) = &i.role {
            attrs.push_str(&format!(" role=\"{r}\""));
        }
        xml.push_str(&format!("  <Item {attrs}>{}</Item>\n", gw::xml_escape(&i.text)));
    }
    xml.push_str("</GDALMetadata>");
    xml
}

fn set_item(items: &mut Vec<Item>, item: Item) {
    match items.iter_mut().find(|i| i.name == item.name && i.sample == item.sample && i.role == item.role && i.domain == item.domain) {
        Some(i) => i.text = item.text,
        None => items.push(item),
    }
}

/// The georeferencing tags (pixel scale, tie points or transformation, GeoKeys) of a raster of
/// this size with this CRS and geotransform (or GCPs), as terrio's writer writes them.
fn georeferencing(
    src: &LazyRaster,
    crs: &SpatialRef,
    gt: [f64; 6],
    gcps: &[Gcp],
    area_or_point: Option<&str>,
    georeferenced: bool,
) -> Result<Vec<(u16, Value)>> {
    let (cols, rows) = (src.width(), src.height());
    let mut metadata = Vec::new();
    if let Some(v) = area_or_point {
        metadata.push(("AREA_OR_POINT".to_string(), v.to_string()));
    }
    let north_up = crate::is_north_up(&gt) && gt[1] > 0.0 && gt[5] < 0.0;
    let (cx, cy) = if north_up { (gt[1], -gt[5]) } else { (1.0, 1.0) };
    let raster = wbraster::Raster {
        cols,
        rows,
        bands: src.band_count(),
        x_min: gt[0],
        y_min: gt[3] - cy * rows as f64,
        cell_size_x: cx,
        cell_size_y: cy,
        nodata: 0.0,
        data_type: src.data_type(),
        crs: crs.to_crs_info(),
        metadata,
        data: wbraster::raster::RasterData::new_filled(src.data_type(), 0, 0.0),
    };
    let opts = gw::TiffOptions { geo_transform: Some(gt), gcps: gcps.to_vec(), georeferenced, ..Default::default() };
    gw::georeferencing_tags(&raster, &opts)
}

/// The tags of the georeferencing (geotransform or tie points) and of the CRS (GeoKeys).
const TRANSFORM_TAGS: [u16; 3] = [33550, 33922, 34264];
const CRS_TAGS: [u16; 3] = [34735, 34736, 34737];

fn edit_tiff<F: Read + Write + Seek>(file: &mut F, src: &mut LazyRaster, o: &EditOptions) -> Result<()> {
    let tiff = gtiff_update::parse(file)?;
    let main = tiff
        .dirs
        .iter()
        .position(|d| !d.is_overview() && !d.is_mask())
        .ok_or_else(|| Error::InvalidData("the TIFF has no image".into()))?;
    let mut dirs = tiff.dirs.clone();
    let big = tiff.big;
    let (w, h) = (src.width() as f64, src.height() as f64);
    let metadata = src.metadata();
    let current_aop = metadata.iter().find(|(k, _)| k == "AREA_OR_POINT").map(|(_, v)| v.clone());
    let new_aop = o.metadata.iter().find(|(k, _)| k == "AREA_OR_POINT").map(|(_, v)| v.clone()).or(current_aop.clone());

    // Georeferencing.
    let crs_changed = o.crs.is_some() || o.unset_crs || new_aop != current_aop;
    let transform_changed = o.geo_transform.is_some() || o.bbox.is_some() || !o.gcps.is_empty() || new_aop != current_aop;
    if crs_changed || transform_changed {
        let crs = match (&o.crs, o.unset_crs) {
            (Some(c), _) => c.clone(),
            (None, true) => SpatialRef::default(),
            (None, false) => src.crs(),
        };
        let gt = match (o.bbox, o.geo_transform) {
            (Some(b), _) => [b[0], (b[2] - b[0]) / w, 0.0, b[3], 0.0, -(b[3] - b[1]) / h],
            (None, Some(gt)) => gt,
            (None, None) => src.geo_transform(),
        };
        let gcps = if !o.gcps.is_empty() {
            o.gcps.clone()
        } else if o.bbox.is_some() || o.geo_transform.is_some() {
            Vec::new()
        } else {
            src.header().gcps
        };
        // A raster without a geotransform of its own keeps having none unless one is given: a
        // CRS or AREA_OR_POINT change must not write the unit-pixel fallback.
        let georeferenced = o.bbox.is_some() || o.geo_transform.is_some() || !gcps.is_empty() || src.has_geo_transform();
        let tags = georeferencing(src, &crs, gt, &gcps, new_aop.as_deref(), georeferenced)?;
        let d = &mut dirs[main];
        let replace: Vec<u16> =
            [if transform_changed { &TRANSFORM_TAGS[..] } else { &[] }, if crs_changed { &CRS_TAGS[..] } else { &[] }].concat();
        for tag in &replace {
            d.remove(*tag);
        }
        for (tag, v) in &tags {
            if replace.contains(tag) {
                d.set(Entry::new(*tag, v, big));
            }
        }
        if crs.is_unknown() && crs_changed {
            for tag in CRS_TAGS {
                d.remove(tag);
            }
        }
    }

    // Nodata, on the image and its overviews (as GDAL writes it).
    let nodata = if o.unset_nodata { None } else { o.nodata.or(src.nodata()) };
    if o.nodata.is_some() || o.unset_nodata {
        for d in dirs.iter_mut().filter(|d| !d.is_mask()) {
            match o.nodata {
                Some(nd) => d.set(Entry::new(42113, &Value::Ascii(gw::format_nodata(nd)), big)),
                None => d.remove(42113),
            }
        }
    }

    // Metadata items.
    let metadata_changed =
        o.unset_all_metadata || !o.metadata.is_empty() || !o.unset_metadata.is_empty() || o.stats || o.approx_stats || o.unset_stats;
    if metadata_changed {
        let mut items = dirs[main].get(42112).and_then(Entry::ascii).map(|x| parse_items(&x)).unwrap_or_default();
        if o.unset_all_metadata {
            items.retain(|i| !i.is_dataset());
        }
        items.retain(|i| !(i.is_dataset() && o.unset_metadata.contains(&i.name)));
        for (k, v) in &o.metadata {
            if k != "AREA_OR_POINT" {
                set_item(&mut items, Item::dataset(k, v));
            }
        }
        if o.unset_stats || o.stats || o.approx_stats {
            items.retain(|i| !(i.band_index().is_some() && i.name.starts_with("STATISTICS_")));
        }
        if o.stats || o.approx_stats {
            for b in 0..src.band_count() {
                let s = band_statistics(src, b + 1, nodata, o.approx_stats && !o.stats)?;
                if s.valid > 0 {
                    if s.approximate {
                        set_item(&mut items, Item::band(b, "STATISTICS_APPROXIMATE", "YES"));
                    }
                    for (name, v) in [
                        ("STATISTICS_MINIMUM", s.min),
                        ("STATISTICS_MAXIMUM", s.max),
                        ("STATISTICS_MEAN", s.mean),
                        ("STATISTICS_STDDEV", s.std_dev),
                    ] {
                        set_item(&mut items, Item::band(b, name, &crate::drivers::common::fmt_g(v, 14)));
                    }
                }
                set_item(&mut items, Item::band(b, "STATISTICS_VALID_PERCENT", &s.valid_percent()));
            }
        }
        if items.is_empty() {
            dirs[main].remove(42112);
        } else {
            dirs[main].set(Entry::new(42112, &Value::Ascii(items_xml(&items)), big));
        }
    }
    if dirs == tiff.dirs {
        return Ok(());
    }
    gtiff_update::break_cog_layout(file, tiff.big, o.ignore_cog_layout_break)?;
    Appender::new(file, big)?.finish(&dirs)
}

/// Band statistics as GDAL's `ComputeStatistics` computes them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stats {
    min: f64,
    max: f64,
    mean: f64,
    std_dev: f64,
    valid: u64,
    sampled: u64,
    approximate: bool,
}

impl Stats {
    /// GDAL's `STATISTICS_VALID_PERCENT`.
    fn valid_percent(&self) -> String {
        if self.valid == 0 {
            return "0".into();
        }
        if self.valid == self.sampled {
            return "100".into();
        }
        let v = crate::drivers::common::fmt_g(100.0 * self.valid as f64 / self.sampled as f64, 4);
        if v == "100" { "99.999".into() } else { v }
    }
}

/// Computes a band's statistics as GDAL does (`GDALRasterBand::ComputeStatistics`): every
/// pixel, or with `approx` those of the overview of more than 2500 pixels closest to that size,
/// else one block in every `sqrt(blocks)`. Byte and UInt16 bands without a mask are summed in
/// integers, as GDAL sums them; the others with Welford's method.
fn band_statistics(src: &mut LazyRaster, band: usize, nodata: Option<f64>, approx: bool) -> Result<Stats> {
    let (w, h) = (src.width(), src.height());
    let dt = src.data_type();
    let overviews = src.overviews();
    if approx && let Some(i) = crate::histogram::sample_overview(w, h, &overviews) {
        let (ow, oh) = overviews[i];
        let windows = vec![Window::new(0, 0, ow, oh)];
        let s = statistics_of(src, band, Some(i), &windows, nodata, dt)?;
        return Ok(Stats { approximate: true, ..s });
    }
    let (bw, bh) = src.block_size().unwrap_or((w, h));
    let (bw, bh) = (bw.min(w).max(1), bh.min(h).max(1));
    let (per_row, per_col) = (w.div_ceil(bw), h.div_ceil(bh));
    let mut rate = 1;
    if approx {
        rate = ((per_row as f64 * per_col as f64).sqrt() as usize).max(1);
        if rate == per_row && per_row > 1 {
            rate += 1;
        }
    }
    let windows: Vec<Window> = if rate == 1 {
        // Strips of whole rows of blocks.
        let rows = bh.max(((32 << 20) / (8 * w.max(1))).max(1) / bh * bh);
        (0..h).step_by(rows).map(|y| Window::new(0, y, w, rows.min(h - y))).collect()
    } else {
        (0..per_row * per_col)
            .step_by(rate)
            .map(|i| {
                let (x, y) = ((i % per_row) * bw, (i / per_row) * bh);
                Window::new(x, y, bw.min(w - x), bh.min(h - y))
            })
            .collect()
    };
    let s = statistics_of(src, band, None, &windows, nodata, dt)?;
    Ok(Stats { approximate: rate > 1, ..s })
}

fn statistics_of(
    src: &mut LazyRaster,
    band: usize,
    overview: Option<usize>,
    windows: &[Window],
    nodata: Option<f64>,
    dt: DataType,
) -> Result<Stats> {
    // GDAL takes the mask band only when there is no nodata value.
    let masked = nodata.is_none() && src.has_mask();
    let integer = !masked && matches!(dt, DataType::U8 | DataType::U16);
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut sum, mut sum_sq) = (0u128, 0u128);
    let (mut mean, mut m2) = (0.0f64, 0.0f64);
    let (mut valid, mut sampled) = (0u64, 0u64);
    for &win in windows {
        let data = src.read_window(win, &WindowOptions { bands: vec![band], overview })?.raster.data.to_f64_vec();
        let mask = if masked { src.read_mask(win, overview)? } else { None };
        sampled += data.len() as u64;
        for (i, &v) in data.iter().enumerate() {
            if mask.as_ref().is_some_and(|m| m[i] == 0) || !crate::histogram::is_valid(v, nodata, dt) {
                continue;
            }
            valid += 1;
            min = min.min(v);
            max = max.max(v);
            if integer {
                sum += v as u128;
                sum_sq += (v as u128) * (v as u128);
            } else if min == max {
                if valid == 1 {
                    mean = min;
                }
            } else {
                let d = v - mean;
                mean += d / valid as f64;
                m2 += d * (v - mean);
            }
        }
    }
    let (mean, std_dev) = if valid == 0 {
        (0.0, 0.0)
    } else if integer {
        let n = valid as u128;
        ((sum as f64) / valid as f64, ((sum_sq * n - sum * sum) as f64).sqrt() / valid as f64)
    } else {
        (mean, (m2 / valid as f64).sqrt())
    };
    Ok(Stats { min, max, mean, std_dev, valid, sampled, approximate: false })
}

/// Computes the default histogram of every band of a GeoTIFF (as `gdalinfo -hist` does, from its
/// stored statistics when there are some) and stores them in its `.aux.xml` sidecar.
fn write_histograms(path: &str) -> Result<()> {
    let info = crate::info::info_path(path, &crate::info::InfoOptions { statistics: false, histogram: true })?;
    let crate::info::Info::Raster(r) = info else { return Err(Error::InvalidData("not a raster".into())) };
    let aux = std::path::PathBuf::from(format!("{path}.aux.xml"));
    let mut xml = match std::fs::read_to_string(&aux) {
        Ok(xml) => xml,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "<PAMDataset>\n</PAMDataset>\n".into(),
        Err(e) => return Err(e.into()),
    };
    for b in &r.bands {
        let Some(hist) = &b.histogram else { continue };
        let counts: Vec<String> = hist.buckets.iter().map(u64::to_string).collect();
        let block = format!(
            "<Histograms>\n      <HistItem>\n        <HistMin>{}</HistMin>\n        <HistMax>{}</HistMax>\n        <BucketCount>{}</BucketCount>\n        <IncludeOutOfRange>1</IncludeOutOfRange>\n        <Approximate>0</Approximate>\n        <HistCounts>{}</HistCounts>\n      </HistItem>\n    </Histograms>",
            crate::drivers::common::fmt_g(hist.min, 16),
            crate::drivers::common::fmt_g(hist.max, 16),
            hist.count,
            counts.join("|")
        );
        xml = set_band_histograms(&xml, b.band, &block)?;
    }
    gtiff_update::replace_file(&aux, |f| Ok(f.write_all(xml.as_bytes())?))
}

/// Puts a `<Histograms>` element in the `PAMRasterBand` of a band of a `.aux.xml` document,
/// replacing the band's histograms, or adding the band.
fn set_band_histograms(xml: &str, band: usize, block: &str) -> Result<String> {
    let open = format!("<PAMRasterBand band=\"{band}\"");
    if let Some(start) = xml.find(&open) {
        let end = start + xml[start..].find("</PAMRasterBand>").ok_or_else(|| Error::InvalidData("malformed .aux.xml".into()))?;
        let body = &xml[start..end];
        let new_body = match (body.find("<Histograms>"), body.find("</Histograms>")) {
            (Some(a), Some(b)) => format!("{}{}{}", &body[..a], block, &body[b + "</Histograms>".len()..]),
            _ => format!("{}\n    {block}\n  ", body.trim_end()),
        };
        return Ok(format!("{}{}{}", &xml[..start], new_body, &xml[end..]));
    }
    let end = xml.rfind("</PAMDataset>").ok_or_else(|| Error::InvalidData("malformed .aux.xml".into()))?;
    Ok(format!("{}  <PAMRasterBand band=\"{band}\">\n    {block}\n  </PAMRasterBand>\n{}", &xml[..end], &xml[end..]))
}

/// How [`move_raster`] moves a raster's georeferencing (`gdalmove.py`).
#[derive(Debug, Clone, PartialEq)]
pub struct MoveOptions {
    /// The CRS to move to (`-t_srs`).
    pub t_srs: SpatialRef,
    /// The raster's CRS, overriding its own (`-s_srs`).
    pub s_srs: Option<SpatialRef>,
    /// Update the file only when the new affine georeferencing is within this many pixels of the
    /// transformed corners and centre (`-et`). Without it, the change is only reported.
    pub error_threshold: Option<f64>,
    /// Update a Cloud Optimized GeoTIFF, breaking its layout.
    pub ignore_cog_layout_break: bool,
}

/// What [`move_raster`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveReport {
    /// The geotransform in the target CRS: exact at the upper left, upper right and lower left
    /// corners.
    pub geo_transform: [f64; 6],
    /// The upper left, lower left, upper right and lower right corners and the centre, in the
    /// target CRS.
    pub corners: Vec<(f64, f64)>,
    /// The error of the new geotransform at each of those points, in pixels.
    pub errors: Vec<f64>,
    /// The largest of them.
    pub max_error: f64,
    /// Whether the file was updated.
    pub updated: bool,
}

/// Moves the georeferencing of a local GeoTIFF to another CRS without resampling, as
/// `gdalmove.py` does: the corners are transformed, an affine geotransform is fitted to three of
/// them, and the file gets it and the target CRS when the error at the corners and centre is
/// below `error_threshold` pixels.
pub fn move_raster(path: &str, options: &MoveOptions) -> Result<MoveReport> {
    gtiff_update::writable_local_file(path)?;
    let src = open_tiff(path)?;
    let report = move_report(&src, options)?;
    drop(src);
    if report.updated {
        edit_raster(path, &move_edit(&report, options))?;
    }
    Ok(report)
}

/// [`move_raster`] on a GeoTIFF held in memory: the edited file (unchanged when not updated) and
/// the report.
pub fn move_raster_bytes(tiff: &[u8], options: &MoveOptions) -> Result<(Vec<u8>, MoveReport)> {
    let src = LazyRaster::from_tiff_bytes(tiff.to_vec())?.ok_or_else(|| Error::InvalidData("not a TIFF file".into()))?;
    let report = move_report(&src, options)?;
    if !report.updated {
        return Ok((tiff.to_vec(), report));
    }
    Ok((edit_raster_bytes(tiff, &move_edit(&report, options))?, report))
}

fn move_edit(report: &MoveReport, o: &MoveOptions) -> EditOptions {
    EditOptions {
        crs: Some(o.t_srs.clone()),
        geo_transform: Some(report.geo_transform),
        ignore_cog_layout_break: o.ignore_cog_layout_break,
        ..Default::default()
    }
}

fn move_report(src: &LazyRaster, o: &MoveOptions) -> Result<MoveReport> {
    let s_srs = o.s_srs.clone().unwrap_or_else(|| src.crs());
    if s_srs.is_unknown() {
        return Err(Error::Crs("the raster has no CRS: give the source CRS".into()));
    }
    if !src.has_geo_transform() {
        let what = if src.header().gcps.is_empty() { "has no geotransform" } else { "is georeferenced by ground control points" };
        return Err(Error::Unsupported(format!("the raster {what}: it cannot be moved without resampling")));
    }
    let gt = src.geo_transform();
    let (w, h) = (src.width() as f64, src.height() as f64);
    let pixels = [(0.0, 0.0), (0.0, h), (w, 0.0), (w, h), (w / 2.0, h / 2.0)];
    let t = super::Ct::new(&s_srs, &o.t_srs)?;
    let corners: Vec<(f64, f64)> = pixels
        .iter()
        .map(|&(c, l)| {
            let (x, y) = super::apply_gt(&gt, c, l);
            t.transform(x, y)
        })
        .collect::<Result<_>>()?;
    let (ul, ll, ur) = (corners[0], corners[1], corners[2]);
    let new_gt = [ul.0, (ur.0 - ul.0) / w, (ll.0 - ul.0) / h, ul.1, (ur.1 - ul.1) / w, (ll.1 - ul.1) / h];
    let inv = super::invert_gt(&new_gt).ok_or_else(|| Error::InvalidData("the moved geotransform cannot be inverted".into()))?;
    let errors: Vec<f64> = corners
        .iter()
        .zip(pixels)
        .map(|(&(x, y), (c, l))| {
            let (pc, pl) = super::apply_gt(&inv, x, y);
            ((pc - c).powi(2) + (pl - l).powi(2)).sqrt()
        })
        .collect();
    // A NaN error (a corner the transform cannot place) counts as unbounded.
    let max_error = errors.iter().copied().fold(0.0, |m, e| if e.is_nan() { f64::INFINITY } else { m.max(e) });
    let updated = o.error_threshold.is_some_and(|e| e > max_error);
    Ok(MoveReport { geo_transform: new_gt, corners, errors, max_error, updated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiff(opts: &gw::TiffOptions) -> Vec<u8> {
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 1, data_type: DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data(cfg, vec![1.0; 12]).unwrap();
        gw::encode(&r, opts).unwrap()
    }

    #[test]
    fn move_refuses_rasters_without_their_own_geotransform() {
        let gcps = [(0.0, 0.0), (4.0, 0.0), (0.0, 3.0)]
            .map(|(pixel, line)| Gcp { pixel, line, x: pixel, y: -line, ..Default::default() })
            .to_vec();
        let o = MoveOptions {
            t_srs: SpatialRef::from_epsg(3857).unwrap(),
            s_srs: Some(SpatialRef::from_epsg(3857).unwrap()),
            error_threshold: Some(1.0),
            ignore_cog_layout_break: false,
        };
        let cases = [
            (gw::TiffOptions { gcps, ..Default::default() }, "ground control points"),
            (gw::TiffOptions { georeferenced: false, ..Default::default() }, "no geotransform"),
        ];
        for (opts, reason) in cases {
            let err = move_raster_bytes(&tiff(&opts), &o).unwrap_err();
            assert!(matches!(&err, Error::Unsupported(m) if m.contains(reason)), "{err}");
        }
        assert!(move_raster_bytes(&tiff(&gw::TiffOptions::default()), &o).unwrap().1.updated);
    }

    #[test]
    fn non_finite_or_singular_georeferencing_is_refused() {
        let bytes = tiff(&gw::TiffOptions::default());
        for gt in [[f64::NAN, 1.0, 0.0, 0.0, 0.0, -1.0], [0.0, 0.0, 0.0, 0.0, 0.0, -1.0]] {
            let o = EditOptions { geo_transform: Some(gt), ..Default::default() };
            assert!(matches!(edit_raster_bytes(&bytes, &o), Err(Error::InvalidArgument(_))), "{gt:?}");
        }
        let gcps = vec![Gcp { x: f64::INFINITY, ..Default::default() }];
        assert!(matches!(edit_raster_bytes(&bytes, &EditOptions { gcps, ..Default::default() }), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn crs_and_area_or_point_edits_keep_an_unreferenced_file_unreferenced() {
        let bytes = tiff(&gw::TiffOptions { georeferenced: false, ..Default::default() });
        let o = EditOptions {
            crs: Some(SpatialRef::from_epsg(3857).unwrap()),
            metadata: vec![("AREA_OR_POINT".into(), "Point".into())],
            ..Default::default()
        };
        let edited = LazyRaster::from_tiff_bytes(edit_raster_bytes(&bytes, &o).unwrap()).unwrap().unwrap();
        assert!(!edited.has_geo_transform());
        assert!(!edited.crs().is_unknown());
    }

    #[test]
    fn new_metadata_values_use_gdal_cpl_and_xml_escaping() {
        let value = "A & <B>";
        let xml = items_xml(&[Item::dataset("SOURCE", value)]);
        // GDAL stores a CPL-escaped value inside XML-escaped text.
        assert_eq!(gw::parse_gdal_metadata(&xml)[0].1, value);
        assert!(xml.contains("&amp;amp;"));
    }

    #[test]
    fn histograms_join_existing_sidecars() {
        let xml = "<PAMDataset>\n  <PAMRasterBand band=\"1\">\n    <Description>dem</Description>\n  </PAMRasterBand>\n</PAMDataset>\n";
        let out = set_band_histograms(xml, 1, "<Histograms>A</Histograms>").unwrap();
        assert!(out.contains("<Description>dem</Description>") && out.contains("<Histograms>A</Histograms>"), "{out}");
        let again = set_band_histograms(&out, 1, "<Histograms>B</Histograms>").unwrap();
        assert!(!again.contains("<Histograms>A") && again.contains("<Histograms>B</Histograms>"), "{again}");
        let two = set_band_histograms(&again, 2, "<Histograms>C</Histograms>").unwrap();
        assert!(two.contains("<PAMRasterBand band=\"2\">") && two.ends_with("</PAMDataset>\n"), "{two}");
    }

    #[test]
    fn statistics_are_stored_and_removed_as_band_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.tif").to_string_lossy().into_owned();
        let cfg = wbraster::RasterConfig { cols: 40, rows: 30, data_type: DataType::U8, ..Default::default() };
        let values: Vec<f64> = (0..1200).map(|i| (i % 200) as f64).collect();
        let ds = crate::RasterDataset::new(wbraster::Raster::from_data(cfg, values.clone()).unwrap(), false);
        crate::write_raster_dataset(&ds, &path, &Default::default()).unwrap();
        edit_raster(&path, &EditOptions { nodata: Some(0.0), stats: true, ..Default::default() }).unwrap();
        let items = crate::geotiff::read_geo_tags(std::path::Path::new(&path)).unwrap().band_metadata;
        let get = |k: &str| items.iter().find(|(b, n, _)| *b == 0 && n == k).map(|(_, _, v)| v.clone());
        let valid: Vec<f64> = values.iter().copied().filter(|v| *v != 0.0).collect();
        let mean = valid.iter().sum::<f64>() / valid.len() as f64;
        assert_eq!(get("STATISTICS_MINIMUM").as_deref(), Some("1"));
        assert_eq!(get("STATISTICS_MAXIMUM").as_deref(), Some("199"));
        assert_eq!(get("STATISTICS_MEAN"), Some(crate::drivers::common::fmt_g(mean, 14)));
        assert_eq!(get("STATISTICS_VALID_PERCENT").as_deref(), Some("99.5"));
        assert!(get("STATISTICS_APPROXIMATE").is_none());
        edit_raster(&path, &EditOptions { unset_stats: true, ..Default::default() }).unwrap();
        let items = crate::geotiff::read_geo_tags(std::path::Path::new(&path)).unwrap().band_metadata;
        assert!(!items.iter().any(|(_, n, _)| n.starts_with("STATISTICS_")), "{items:?}");
    }

    #[test]
    fn valid_percents_are_formatted_as_gdal() {
        let s = |valid, sampled| Stats { min: 0.0, max: 0.0, mean: 0.0, std_dev: 0.0, valid, sampled, approximate: false };
        assert_eq!(s(0, 10).valid_percent(), "0");
        assert_eq!(s(10, 10).valid_percent(), "100");
        assert_eq!(s(1102, 10000).valid_percent(), "11.02");
        assert_eq!(s(999_999, 1_000_000).valid_percent(), "99.999");
    }
    #[test]
    fn invalid_utf8_histogram_sidecar_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif").to_string_lossy().into_owned();
        let cfg = wbraster::RasterConfig { cols: 2, rows: 2, data_type: DataType::U8, ..Default::default() };
        let ds = crate::RasterDataset::new(wbraster::Raster::from_data(cfg, vec![1., 2., 3., 4.]).unwrap(), false);
        crate::write_raster_dataset(&ds, &path, &Default::default()).unwrap();
        let aux = format!("{path}.aux.xml");
        std::fs::write(&aux, [0xff]).unwrap();
        assert!(write_histograms(&path).is_err());
        assert_eq!(std::fs::read(aux).unwrap(), [0xff]);
    }
}
