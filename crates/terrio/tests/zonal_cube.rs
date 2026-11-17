// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Zonal statistics as a cube indexed by the zones' geometries: cells equal the wide output's
//! columns, dimensions come from the inputs' times and labels.

#![cfg(not(target_family = "wasm"))]

use terrio::LazyRaster;
use terrio::ops::zonal::{Statistic, ZonalBand, ZonalOptions, ZonalRaster, ZonalSource, zonal_stats};
use terrio::ops::zonal_cube::{CubeValues, zonal_cube};
use terrio::vector::Coord;
use terrio::{DataType, Feature, FieldValue, Geometry, Layer, Raster, RasterConfig, RasterDataset};

/// Two zones on a 4 x 4 grid of 1 m pixels from (0, 0): the left half and the top-right quarter.
fn zones() -> Layer {
    let square = |x0: f64, y0: f64, x1: f64, y1: f64| {
        Geometry::polygon(vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)], vec![])
    };
    let mut zones = Layer::new("zones");
    zones.features.push(Feature { fid: 0, geometry: Some(square(0.0, 0.0, 2.0, 4.0)), attributes: vec![] });
    zones.features.push(Feature { fid: 1, geometry: Some(square(2.0, 2.0, 4.0, 4.0)), attributes: vec![] });
    zones
}

/// A 4 x 4 single-band raster of `scale * (1..=16)` over the zones.
fn raster(scale: f64) -> ZonalRaster {
    let cfg = RasterConfig { cols: 4, rows: 4, bands: 2, data_type: DataType::F64, ..Default::default() };
    let values: Vec<f64> = (1..=32).map(|v| scale * v as f64).collect();
    let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
    ds.set_geo_transform([0.0, 1.0, 0.0, 4.0, 0.0, -1.0]);
    ZonalRaster::Open(LazyRaster::from(ds))
}

fn labelled(scale: f64, time: Option<&str>, label: Option<&str>, prefix: &str) -> ZonalSource {
    let band = ZonalBand { band: 1, prefix: prefix.into(), time: time.map(Into::into), label: label.map(Into::into) };
    ZonalSource { raster: raster(scale), bands: vec![band] }
}

fn options() -> ZonalOptions {
    ZonalOptions { stats: vec![Statistic::Mean, Statistic::Count], ..Default::default() }
}

fn f64s(values: &CubeValues) -> Vec<f64> {
    match values {
        CubeValues::F64(v) => v.clone(),
        CubeValues::F32(v) => v.iter().map(|&x| x as f64).collect(),
    }
}

fn number(value: &FieldValue) -> f64 {
    match value {
        FieldValue::Float(x) => *x,
        FieldValue::Integer(i) => *i as f64,
        other => panic!("not a number: {other:?}"),
    }
}

#[test]
fn cube_cells_equal_wide_columns() {
    let sources = || {
        vec![
            labelled(1.0, Some("2024-07-01"), Some("B04"), "2024_07_01_B04"),
            labelled(2.0, Some("2024-07-01"), Some("B08"), "2024_07_01_B08"),
            labelled(3.0, Some("2024-07-11"), Some("B04"), "2024_07_11_B04"),
        ]
    };
    let cube = zonal_cube(&zones(), &mut sources(), &options(), "ndvi").unwrap();
    assert_eq!(cube.statistics, vec![Statistic::Mean, Statistic::Count]);
    let var = &cube.variables[0];
    assert_eq!(var.name, "ndvi");
    let dims: Vec<_> = var.dims.iter().map(|d| (d.name.as_str(), d.labels.clone())).collect();
    assert_eq!(dims, vec![("time", vec!["2024-07-01".to_string(), "2024-07-11".into()]), ("band", vec!["B04".to_string(), "B08".into()])]);
    assert_eq!(cube.shape(0), vec![2, 2, 2, 2]);
    let values = f64s(&var.values);
    let wide = zonal_stats(&zones(), &mut sources(), &options()).unwrap();
    let column = |name: &str| wide.schema.fields().iter().position(|f| f.name == name).unwrap();
    for (t, time) in ["2024_07_01", "2024_07_11"].iter().enumerate() {
        for (b, band) in ["B04", "B08"].iter().enumerate() {
            for (s, stat) in ["mean", "count"].iter().enumerate() {
                for f in 0..2 {
                    let got = values[((f * 2 + t) * 2 + b) * 2 + s];
                    if (*time, *band) == ("2024_07_11", "B08") {
                        assert!(got.is_nan(), "missing cell {time} {band} {stat} {f}: {got}");
                    } else {
                        let expected = number(&wide.features[f].attributes[column(&format!("{time}_{band}_{stat}"))]);
                        assert_eq!(got, expected, "{time} {band} {stat} feature {f}");
                    }
                }
            }
        }
    }
}

#[test]
fn no_labels_no_time_gives_geometry_statistic() {
    let mut sources = vec![ZonalSource { raster: raster(1.0), bands: vec![ZonalBand::new(1, "dem_b1")] }];
    let cube = zonal_cube(&zones(), &mut sources, &options(), "dem").unwrap();
    assert!(cube.variables[0].dims.is_empty());
    assert_eq!(cube.shape(0), vec![2, 2]);
}

#[test]
fn several_unlabelled_bands_give_a_band_dim() {
    let bands = vec![ZonalBand::new(1, "img_b1"), ZonalBand::new(2, "img_b2")];
    let mut sources = vec![ZonalSource { raster: raster(1.0), bands }];
    let cube = zonal_cube(&zones(), &mut sources, &options(), "img").unwrap();
    let dim = &cube.variables[0].dims[0];
    assert_eq!((dim.name.as_str(), dim.labels.clone()), ("band", vec!["img_b1".to_string(), "img_b2".into()]));
    let values = f64s(&cube.variables[0].values);
    assert_ne!(values[0], values[2], "band 2 differs from band 1"); // feature 0, mean of b1 vs b2
}

#[test]
fn time_on_some_inputs_only_is_an_error() {
    let mut sources = vec![labelled(1.0, Some("2024-07-01"), Some("B04"), "dated"), labelled(1.0, None, Some("B04"), "undated")];
    let err = zonal_cube(&zones(), &mut sources, &options(), "x").unwrap_err().to_string();
    assert!(err.contains("undated"), "{err}");
}

#[test]
fn same_cell_twice_is_an_error() {
    let mut sources =
        vec![labelled(1.0, Some("2024-07-01"), Some("B04"), "first"), labelled(2.0, Some("2024-07-01"), Some("B04"), "second")];
    let err = zonal_cube(&zones(), &mut sources, &options(), "x").unwrap_err().to_string();
    assert!(err.contains("first") && err.contains("second"), "{err}");
}

#[test]
fn weighted_and_output_options_are_refused() {
    let mut sources = vec![labelled(1.0, None, None, "a")];
    let rounded = ZonalOptions { decimal_places: Some(2), ..options() };
    assert!(zonal_cube(&zones(), &mut sources, &rounded, "x").is_err());
}
