//! Per-polygon statistics across raster bands. Zones are evaluated independently, so overlapping
//! zones can include the same pixel. Membership uses the GDAL-compatible rasterizer in pixel
//! coordinates, with a cropped window per zone and one mask shared by bands of the same raster.

use super::rasterize::{Grid, RasterizeOptions, rasterize};
use crate::crs::{SpatialRef, Transformer};
use crate::{DataType, Error, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, RasterDataset, Result};

/// A statistic for [`zonal_stats`]. Standard deviation is the population standard deviation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statistic {
    /// Number of finite, non-nodata pixels. Zero when none contribute.
    Count,
    /// Minimum pixel value.
    Min,
    /// Maximum pixel value.
    Max,
    /// Arithmetic mean.
    Mean,
    /// Sum of pixel values.
    Sum,
    /// Population standard deviation.
    StdDev,
    /// Median; the mean of the two middle values for an even count.
    Median,
}

impl Statistic {
    /// Parses `count`, `min`, `max`, `mean`, `sum`, `stddev` (`std`, `stdev`) or `median`.
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name.to_ascii_lowercase().as_str() {
            "count" => Self::Count,
            "min" => Self::Min,
            "max" => Self::Max,
            "mean" => Self::Mean,
            "sum" => Self::Sum,
            "stddev" | "std" | "stdev" => Self::StdDev,
            "median" => Self::Median,
            _ => return Err(Error::InvalidArgument(format!("unknown zonal statistic '{name}'"))),
        })
    }

    /// The suffix used in output field names.
    pub fn name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Min => "min",
            Self::Max => "max",
            Self::Mean => "mean",
            Self::Sum => "sum",
            Self::StdDev => "stddev",
            Self::Median => "median",
        }
    }
}

/// One raster band to summarize, and its output field prefix.
pub struct ZonalInput<'a> {
    /// Output fields are `prefix_STATISTIC`. Name collisions get `_2`, `_3`, etc.
    pub prefix: String,
    /// Raster held in memory by the caller.
    pub raster: &'a RasterDataset,
    /// Band, from 1.
    pub band: usize,
}

/// Pixel inclusion and statistics for [`zonal_stats`].
#[derive(Debug, Clone)]
pub struct ZonalOptions {
    /// Statistics to calculate, in output order. Default: count, min, max, mean, sum, stddev.
    pub stats: Vec<Statistic>,
    /// Include every pixel touched by the polygon, instead of pixels whose centres are inside.
    pub all_touched: bool,
}

impl Default for ZonalOptions {
    fn default() -> Self {
        Self {
            stats: vec![Statistic::Count, Statistic::Min, Statistic::Max, Statistic::Mean, Statistic::Sum, Statistic::StdDev],
            all_touched: false,
        }
    }
}

/// Adds statistics for each zone and raster band, preserving source geometry, Z/M, attributes,
/// feature IDs and CRS. Zones must be polygons or multipolygons; null/empty geometries and zones
/// with no valid contributing pixels have count 0 and null values for the other statistics.
/// Nodata and non-finite pixels are excluded. Holes are excluded; overlapping zones are evaluated
/// independently. Statistics use f64 values; integers above 2^53 can lose precision.
///
/// Raster grids may be rotated or south-up and use different CRSs. Each boundary is reprojected
/// into each raster's CRS before rasterization; a boundary transformation failure is an error.
/// Missing CRSs are accepted only when both zone and raster CRS are missing. Curved edges are
/// not densified during reprojection. Complex rasters and geolocation arrays are unsupported.
///
/// ```no_run
/// use terrio::ops::zonal::{zonal_stats, ZonalInput};
/// let zones = terrio::open_vector("counties.gpkg", &Default::default())?;
/// let dem = terrio::open_raster("dem.tif")?;
/// let inputs = [ZonalInput { prefix: "elevation".into(), raster: &dem, band: 1 }];
/// let stats = zonal_stats(&zones.layers[0], &inputs, &Default::default())?;
/// terrio::write_vector(&[stats], "statistics.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn zonal_stats(zones: &Layer, inputs: &[ZonalInput<'_>], options: &ZonalOptions) -> Result<Layer> {
    if inputs.is_empty() || options.stats.is_empty() {
        return Err(Error::InvalidArgument("zonal statistics needs at least one raster band and one statistic".into()));
    }
    for (i, stat) in options.stats.iter().enumerate() {
        if options.stats[..i].contains(stat) {
            return Err(Error::InvalidArgument(format!("duplicate statistic '{}'", stat.name())));
        }
    }
    for f in &zones.features {
        if let Some(g) = &f.geometry {
            if !matches!(g, Geometry::Polygon { .. } | Geometry::MultiPolygon(_)) {
                return Err(Error::InvalidArgument(format!("zone {} is not a polygon or multipolygon", f.fid)));
            }
            if g.all_coords().iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
                return Err(Error::InvalidArgument(format!("zone {} has non-finite XY coordinates", f.fid)));
            }
        }
    }
    let src = SpatialRef::from_layer(zones);
    let mut groups: Vec<Group<'_>> = Vec::new();
    let mut out = zones.clone();
    let mut columns = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        super::sample::check_raster(input.raster)?;
        super::sample::check_band(input.raster, input.band)?;
        if input.prefix.trim().is_empty() {
            return Err(Error::InvalidArgument("zonal field prefixes cannot be empty".into()));
        }
        if let Some(g) = groups.iter_mut().find(|g| std::ptr::eq(g.raster, input.raster)) {
            g.entries.push((i, input.band - 1));
        } else {
            let dst = input.raster.crs();
            let transform = match (src.is_unknown(), dst.is_unknown()) {
                (true, true) => None,
                (true, false) | (false, true) => {
                    return Err(Error::Crs("zones and raster must both declare a CRS, or both have no CRS".into()));
                }
                _ if src.same_as(&dst) => None,
                _ => Some(Transformer::new(&src, &dst)?),
            };
            groups.push(Group { raster: input.raster, transform, entries: vec![(i, input.band - 1)] });
        }
        let mut indices = Vec::new();
        for &stat in &options.stats {
            let name = super::geometry::unique_field(&out, &format!("{}_{}", input.prefix, stat.name()));
            indices.push(out.schema.len());
            out.schema.add_field(FieldDef::new(name, if stat == Statistic::Count { FieldType::Integer } else { FieldType::Float }));
        }
        columns.push(indices);
    }
    let median = options.stats.contains(&Statistic::Median);
    for f in &mut out.features {
        f.attributes.resize(out.schema.len(), FieldValue::Null);
        for group in &groups {
            let mask = match &f.geometry {
                Some(g) if !g.is_empty() => {
                    zone_mask(g, group, options.all_touched).map_err(|e| Error::InvalidArgument(format!("zone {}: {e}", f.fid)))?
                }
                _ => None,
            };
            for &(input, band) in &group.entries {
                let mut values = Summary::default();
                let mut ordered = Vec::new();
                if let Some(mask) = &mask {
                    let r = &group.raster.raster;
                    for row in 0..mask.rows {
                        for col in 0..mask.cols {
                            if mask.pixels[row * mask.cols + col] == 0 {
                                continue;
                            }
                            let index = band * r.cols * r.rows + (row + mask.row) * r.cols + col + mask.col;
                            let v = r.data.get_f64(index);
                            if v.is_finite() && crate::histogram::is_valid(v, group.raster.nodata(), r.data_type) {
                                values.add(v);
                                if median {
                                    ordered.push(v);
                                }
                            }
                        }
                    }
                }
                if median {
                    ordered.sort_by(f64::total_cmp);
                }
                for (&column, &stat) in columns[input].iter().zip(&options.stats) {
                    f.attributes[column] = values.stat(stat, &ordered);
                }
            }
        }
    }
    Ok(out)
}

struct Group<'a> {
    raster: &'a RasterDataset,
    transform: Option<Transformer>,
    entries: Vec<(usize, usize)>,
}

struct Mask {
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    pixels: Vec<u8>,
}

fn zone_mask(geometry: &Geometry, group: &Group<'_>, all_touched: bool) -> Result<Option<Mask>> {
    let inv =
        super::invert_gt(&group.raster.geo_transform()).ok_or_else(|| Error::InvalidArgument("cannot invert the raster grid".into()))?;
    let mut geometry = geometry.clone();
    crate::vector::map_coords(&mut geometry, &mut |c| {
        let (x, y) = match &group.transform {
            Some(t) => t.transform(c.x, c.y)?,
            None => (c.x, c.y),
        };
        let (col, row) = super::apply_gt(&inv, x, y);
        if !col.is_finite() || !row.is_finite() {
            return Err(Error::Crs("zone boundary could not be projected into the raster grid".into()));
        }
        c.x = col;
        c.y = -row;
        Ok(())
    })?;
    let Some(bbox) = geometry.bbox() else { return Ok(None) };
    let r = &group.raster.raster;
    let col = (bbox.min_x.floor() - 1.0).clamp(0.0, r.cols as f64) as usize;
    let row = ((-bbox.max_y).floor() - 1.0).clamp(0.0, r.rows as f64) as usize;
    let end_col = (bbox.max_x.ceil() + 1.0).clamp(0.0, r.cols as f64) as usize;
    let end_row = ((-bbox.min_y).ceil() + 1.0).clamp(0.0, r.rows as f64) as usize;
    if col >= end_col || row >= end_row {
        return Ok(None);
    }
    let (cols, rows) = (end_col - col, end_row - row);
    let mut layer = Layer::new("zone");
    layer.features.push(Feature { fid: 1, geometry: Some(geometry), attributes: Vec::new() });
    let options = RasterizeOptions {
        burn_values: vec![1.0],
        data_type: Some(DataType::U8),
        all_touched,
        grid: Some(Grid { cols, rows, geo_transform: [col as f64, 1.0, 0.0, -(row as f64), 0.0, -1.0], crs: SpatialRef::default() }),
        ..Default::default()
    };
    let mask = rasterize(&[layer], &options)?;
    let crate::raster::RasterData::U8(pixels) = mask.raster.data else { unreachable!() };
    Ok(Some(Mask { col, row, cols, rows, pixels }))
}

#[derive(Default)]
struct Summary {
    count: usize,
    sum: f64,
    mean: f64,
    m2: f64,
    min: f64,
    max: f64,
}

impl Summary {
    fn add(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        }
        self.count += 1;
        self.sum += value;
        let delta = value - self.mean;
        self.mean += delta / self.count as f64;
        self.m2 += delta * (value - self.mean);
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    fn stat(&self, stat: Statistic, sorted: &[f64]) -> FieldValue {
        if stat == Statistic::Count {
            return FieldValue::Integer(self.count as i64);
        }
        if self.count == 0 {
            return FieldValue::Null;
        }
        FieldValue::Float(match stat {
            Statistic::Min => self.min,
            Statistic::Max => self.max,
            Statistic::Mean => self.mean,
            Statistic::Sum => self.sum,
            Statistic::StdDev => (self.m2 / self.count as f64).max(0.0).sqrt(),
            Statistic::Median => {
                let mid = sorted.len() / 2;
                if sorted.len() % 2 == 1 { sorted[mid] } else { sorted[mid - 1] / 2.0 + sorted[mid] / 2.0 }
            }
            Statistic::Count => unreachable!(),
        })
    }
}
