// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Zonal statistics as a cube indexed by the zones' geometries: `(geometry, ..., statistic)`.
//!
//! [`zonal_cube`] places each band of [`zonal_stats_with`] at the cell its time and label name,
//! so that a series of dated images gives `(geometry, time, band, statistic)` rather than one
//! column per date, band and statistic.

use std::collections::HashMap;
use std::sync::Mutex;

use super::zonal::{BandStats, Statistic, ZonalOptions, ZonalSource, zonal_stats_with};
use crate::{Error, FieldValue, Layer, Result};

/// Zonal statistics of the features of `zones`, as variables of dims
/// `(geometry, dims..., statistic)`.
#[derive(Debug, Clone)]
pub struct ZonalCube {
    /// The zones, in their own CRS: one entry of the `geometry` dimension per feature.
    pub zones: Layer,
    /// The `statistic` dimension, last.
    pub statistics: Vec<Statistic>,
    /// The variables, each over the zones and the statistics.
    pub variables: Vec<CubeVariable>,
}

/// One variable of a [`ZonalCube`].
#[derive(Debug, Clone, PartialEq)]
pub struct CubeVariable {
    /// Its name.
    pub name: String,
    /// The dimensions between `geometry` and `statistic` (such as `time` and `band`), in order.
    pub dims: Vec<CubeDim>,
    /// The values in C order over `(geometry, dims..., statistic)`; NaN where there is none.
    pub values: CubeValues,
}

/// A dimension of a [`CubeVariable`] and the label of each of its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeDim {
    /// Its name.
    pub name: String,
    /// One label per entry.
    pub labels: Vec<String>,
}

/// The values of a [`CubeVariable`].
#[derive(Debug, Clone, PartialEq)]
pub enum CubeValues {
    /// Single precision, for single-precision data.
    F32(Vec<f32>),
    /// Double precision.
    F64(Vec<f64>),
}

impl ZonalCube {
    /// The shape of variable `variable`: `[features, dims..., statistics]`.
    pub fn shape(&self, variable: usize) -> Vec<usize> {
        let mut shape = vec![self.zones.features.len()];
        shape.extend(self.variables[variable].dims.iter().map(|d| d.labels.len()));
        shape.push(self.statistics.len());
        shape
    }

    /// Writes the cube to the new Zarr v3 store `path`, readable by xarray and CF tools:
    /// - each variable over `(geometry, dims..., statistic)`, NaN where there is no value;
    /// - the zones as CF-1.8 polygon geometries (`geometry_container`; exterior rings
    ///   anticlockwise, holes clockwise) with a CF grid mapping (`spatial_ref`) of their CRS, and
    ///   their fields and FIDs (`feature_id`) as coordinates along `geometry`;
    /// - `time` labels that all are ISO 8601 dates or date-times as CF times, other labels and
    ///   the statistics' names as strings;
    /// - the zones' CRS and extent as `proj:` and `spatial:` group attributes.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] when `path` exists, or a zone has no geometry, an empty one or
    /// one that is not a polygon (CF cannot store it), naming it; I/O errors.
    pub fn write_zarr(&self, path: &str) -> Result<()> {
        #[cfg(feature = "zarr")]
        {
            crate::drivers::zarr::write_cube(self, std::path::Path::new(path))
        }
        #[cfg(not(feature = "zarr"))]
        {
            let _ = path;
            Err(Error::disabled("zarr"))
        }
    }
}

/// The zonal statistics of `sources` over `zones`, as [`zonal_stats`](super::zonal::zonal_stats)
/// computes them, as one variable named `name` of a cube.
///
/// A band's cell is given by its [`time`](super::zonal::ZonalBand::time) and its key, its
/// [`label`](super::zonal::ZonalBand::label) or else its prefix:
/// - a `time` dimension when a band has a time; every band must then have one;
/// - a `band` dimension when a band has a label or bands have different keys;
/// - entries in the order they first appear; cells no band fills are NaN.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for weighted statistics, `dst_srs` or `decimal_places` (a cube keeps
/// the zones' CRS and full precision), a band without a time when another has one, or two bands
/// on the same cell, naming their prefixes; and the errors of [`zonal_stats_with`].
pub fn zonal_cube(zones: &Layer, sources: &mut [ZonalSource], options: &ZonalOptions, name: &str) -> Result<ZonalCube> {
    if options.weights.is_some() || options.stats.iter().any(|s| s.uses_weights()) {
        return Err(Error::InvalidArgument("weighted statistics are not supported in zonal cubes".into()));
    }
    if options.dst_srs.is_some() || options.decimal_places.is_some() {
        return Err(Error::InvalidArgument(
            "zonal cubes keep the zones' CRS and full precision: dst_srs and decimal_places are not supported".into(),
        ));
    }
    let bands: Vec<_> = sources.iter().flat_map(|s| &s.bands).collect();
    let timed = bands.iter().any(|b| b.time.is_some());
    if timed && let Some(b) = bands.iter().find(|b| b.time.is_none()) {
        return Err(Error::InvalidArgument(format!("input '{}' has no time while other inputs have one", b.prefix)));
    }
    let key = |b: &super::zonal::ZonalBand| b.label.clone().unwrap_or_else(|| b.prefix.clone());
    let mut times: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for b in &bands {
        if let Some(t) = &b.time
            && !times.contains(t)
        {
            times.push(t.clone());
        }
        let k = key(b);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    let banded = bands.iter().any(|b| b.label.is_some()) || keys.len() > 1;
    let key_count = if banded { keys.len() } else { 1 };
    let cells = times.len().max(1) * key_count;
    // The cell of each band of each source; two bands on one cell are an error.
    let mut taken: HashMap<usize, &str> = HashMap::new();
    let mut cell_of: Vec<Vec<usize>> = Vec::with_capacity(sources.len());
    for source in sources.iter() {
        let mut row = Vec::with_capacity(source.bands.len());
        for b in &source.bands {
            let t = b.time.as_ref().map_or(0, |t| times.iter().position(|x| x == t).unwrap_or(0));
            let k = if banded { keys.iter().position(|x| *x == key(b)).unwrap_or(0) } else { 0 };
            let cell = t * key_count + k;
            if let Some(first) = taken.insert(cell, &b.prefix) {
                return Err(Error::InvalidArgument(format!("inputs '{first}' and '{}' fall on the same time and band", b.prefix)));
            }
            row.push(cell);
        }
        cell_of.push(row);
    }
    let mut dims = Vec::new();
    if timed {
        dims.push(CubeDim { name: "time".into(), labels: times });
    }
    if banded {
        dims.push(CubeDim { name: "band".into(), labels: keys });
    }
    let stat_count = options.stats.len();
    let values = Mutex::new(vec![f64::NAN; zones.features.len() * cells * stat_count]);
    zonal_stats_with(zones, sources, options, &|stats: BandStats| {
        let cell = cell_of[stats.source][stats.band];
        let mut values = values.lock().map_err(|_| Error::InvalidArgument("zonal cube lock poisoned".into()))?;
        for (s, column) in stats.values.iter().enumerate() {
            for (f, value) in column.iter().enumerate() {
                values[(f * cells + cell) * stat_count + s] = match value {
                    FieldValue::Float(x) => *x,
                    FieldValue::Integer(i) => *i as f64,
                    _ => f64::NAN,
                };
            }
        }
        Ok(())
    })?;
    let values = values.into_inner().map_err(|_| Error::InvalidArgument("zonal cube lock poisoned".into()))?;
    Ok(ZonalCube {
        zones: zones.clone(),
        statistics: options.stats.clone(),
        variables: vec![CubeVariable { name: name.into(), dims, values: CubeValues::F64(values) }],
    })
}
