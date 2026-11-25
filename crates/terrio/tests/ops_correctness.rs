// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Regression tests for core operation edge cases and correctness fixes.

use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::calc::{CalcInput, CalcOptions, calc};
use terrio::ops::proximity::{ProximityOptions, proximity};
use terrio::ops::rasterize::{Grid, RasterizeOptions, rasterize};
use terrio::ops::sieve::{SieveOptions, sieve};
use terrio::ops::sql::query;
use terrio::ops::warp::{Cutline, WarpOptions, warp};
use terrio::raster::RasterData;
use terrio::vector::{Coord, Ring};
use terrio::{DataType, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Raster, RasterConfig, RasterDataset, VectorDataset};

#[test]
fn proximity_u64_computes_bottom_up_distances() {
    let config = RasterConfig { cols: 5, rows: 5, bands: 1, data_type: DataType::U8, ..Default::default() };
    let mut values = vec![0u8; 25];
    // Target only at the bottom-right corner: every other pixel is reached only in the bottom-up pass.
    values[24] = 1;
    let mut src = RasterDataset::new(Raster::from_data_native(config, RasterData::U8(values)).unwrap(), false);
    src.set_geo_transform([0.0, 1.0, 0.0, 5.0, 0.0, -1.0]);

    let out = proximity(&src, &ProximityOptions { output_type: DataType::U64, ..Default::default() }).unwrap();
    assert_eq!(out.raster.data_type, DataType::U64);
    assert_eq!(out.raster.get(0, 4, 4), 0.0);
    assert_eq!(out.raster.get(0, 4, 3), 1.0);
    assert_eq!(out.raster.get(0, 0, 0), (32.0f64).sqrt().round());
}

#[test]
fn sieve_preserves_nan_nodata_and_unmerged_float_values() {
    let config = RasterConfig { cols: 3, rows: 2, bands: 1, data_type: DataType::F32, nodata: f64::NAN, ..Default::default() };
    let values = vec![1.5f32, 1.5, f32::NAN, 1.5, 1.5, 1.5];
    let mut src = RasterDataset::new(Raster::from_data_native(config, RasterData::F32(values)).unwrap(), true);
    src.set_geo_transform([0.0, 1.0, 0.0, 2.0, 0.0, -1.0]);

    let out = sieve(&src, &SieveOptions { threshold: 2, ..Default::default() }).unwrap();
    assert!(out.raster.get(0, 0, 2).is_nan(), "NaN nodata pixel must remain NaN, got {}", out.raster.get(0, 0, 2));
    assert_eq!(out.raster.get(0, 0, 0), 1.5, "unmerged float pixels must keep their original values");
}

#[test]
fn calc_round_large_negative_decimals_and_i128_min_do_not_panic() {
    let config = RasterConfig { cols: 2, rows: 1, bands: 1, data_type: DataType::I32, ..Default::default() };
    let mut src = RasterDataset::new(Raster::from_data_native(config, RasterData::I32(vec![123, -456])).unwrap(), false);
    src.set_geo_transform([0.0, 1.0, 0.0, 1.0, 0.0, -1.0]);

    for expr in ["round(A, -128)", "round(A, -39)", "round(A, -38)"] {
        let out =
            calc(&[CalcInput { name: "A".into(), ds: &src, band: 1 }], &CalcOptions { calc: vec![expr.into()], ..Default::default() })
                .unwrap();
        assert_eq!(out.raster.get(0, 0, 0), 0.0, "{expr}");
        assert_eq!(out.raster.get(0, 0, 1), 0.0, "{expr}");
    }

    // i128::MIN = -170141183460469231731687303715884105728; division/mod/negation must return an error, not panic.
    let i128_min = "(-170141183460469231731687303715884105727 - 1)";
    for expr in [format!("{i128_min} // -1 + A * 0"), format!("{i128_min} % -1 + A * 0"), format!("-{i128_min} + A * 0")] {
        let res =
            calc(&[CalcInput { name: "A".into(), ds: &src, band: 1 }], &CalcOptions { calc: vec![expr.clone()], ..Default::default() });
        assert!(res.is_err(), "{expr} should return overflow error rather than panicking");
    }
}

#[test]
fn sql_three_valued_logic_and_order_by_with_nan() {
    let mut layer = Layer::new("t");
    layer.add_field(FieldDef::new("a", FieldType::Integer));
    layer.add_field(FieldDef::new("b", FieldType::Integer));
    layer.add_field(FieldDef::new("val", FieldType::Float));
    layer.add_field(FieldDef::new("grp", FieldType::Text));

    layer.push(Feature {
        fid: 1,
        geometry: None,
        attributes: vec![FieldValue::Null, FieldValue::Integer(1), FieldValue::Float(f64::NAN), FieldValue::Text("x".into())],
    });
    layer.push(Feature {
        fid: 2,
        geometry: None,
        attributes: vec![FieldValue::Integer(0), FieldValue::Integer(1), FieldValue::Float(2.0), FieldValue::Text("y".into())],
    });
    layer.push(Feature {
        fid: 3,
        geometry: None,
        attributes: vec![FieldValue::Integer(1), FieldValue::Integer(1), FieldValue::Float(1.0), FieldValue::Text("z".into())],
    });

    // NOT (NULL AND TRUE) is NULL (not TRUE), so fid 1 must be excluded while fid 2 (NOT (FALSE AND TRUE)) is included.
    let filtered = query(&[layer.clone()], "SELECT a, b FROM t WHERE NOT (a = 1 AND b = 1)").unwrap();
    let fids: Vec<u64> = filtered.features.iter().map(|f| f.fid).collect();
    assert_eq!(fids, vec![2]);

    // TRUE OR NULL is TRUE (1), not NULL.
    let projected = query(&[layer.clone()], "SELECT (b = 1 OR a = 1) AS ok FROM t WHERE b = 1").unwrap();
    assert_eq!(projected.features[0].attributes[0], FieldValue::Integer(1));

    // ORDER BY with many finite floats and NaNs must not panic in driftsort.
    let mut many = Layer::new("m");
    many.add_field(FieldDef::new("grp", FieldType::Text));
    many.add_field(FieldDef::new("val", FieldType::Float));
    for i in 0..40 {
        let v = if i % 5 == 0 {
            f64::NAN
        } else if i % 7 == 0 {
            f64::INFINITY
        } else if i % 11 == 0 {
            f64::NEG_INFINITY
        } else {
            ((i * 17) % 23) as f64 - 10.0
        };
        many.push(Feature { fid: i as u64 + 1, geometry: None, attributes: vec![FieldValue::Text(format!("g{i}")), FieldValue::Float(v)] });
    }
    let ordered = query(&[many.clone()], "SELECT val FROM m ORDER BY val").unwrap();
    assert_eq!(ordered.features.len(), 40);
    let grouped = query(&[many], "SELECT grp, SUM(val) AS s FROM m GROUP BY grp ORDER BY s DESC").unwrap();
    assert_eq!(grouped.features.len(), 40);
}

#[test]
fn warp_cutline_with_empty_polygon_or_empty_ring_does_not_panic() {
    let config = RasterConfig { cols: 4, rows: 4, bands: 1, data_type: DataType::U8, ..Default::default() };
    let mut src = RasterDataset::new(Raster::from_data_native(config, RasterData::U8(vec![1; 16])).unwrap(), false);
    src.set_geo_transform([0.0, 1.0, 0.0, 4.0, 0.0, -1.0]);
    src.raster.crs.epsg = Some(4326);

    // Polygon containing a valid exterior and an empty interior ring.
    let mut layer = Layer::new("cut");
    layer.push(Feature {
        fid: 1,
        geometry: Some(Geometry::Polygon {
            exterior: Ring(vec![Coord::xy(0.0, 0.0), Coord::xy(2.0, 0.0), Coord::xy(2.0, 2.0), Coord::xy(0.0, 2.0), Coord::xy(0.0, 0.0)]),
            interiors: vec![Ring(vec![])],
        }),
        attributes: vec![],
    });
    let cutline = Cutline::from_vector(&VectorDataset::new(vec![layer], VectorDriver::GeoJSON)).unwrap();
    let opts = WarpOptions { cutline: Some(cutline), crop_to_cutline: true, ..Default::default() };
    let warped = warp(&[&src], &opts).unwrap();
    assert_eq!(warped.raster.cols, 2);
    assert_eq!(warped.raster.rows, 2);

    // Completely empty polygon cutline must return an error, not panic.
    let mut empty_layer = Layer::new("empty");
    empty_layer.push(Feature {
        fid: 1,
        geometry: Some(Geometry::Polygon { exterior: Ring(vec![]), interiors: vec![] }),
        attributes: vec![],
    });
    let cutline_res = Cutline::from_vector(&VectorDataset::new(vec![empty_layer], VectorDriver::GeoJSON));
    assert!(
        cutline_res.is_err()
            || warp(&[&src], &WarpOptions { cutline: cutline_res.ok(), crop_to_cutline: true, ..Default::default() }).is_err()
    );
}

#[test]
fn rasterize_zero_dimension_grid_errors_and_long_line_does_not_overflow_or_stall() {
    let mut layer = Layer::new("lines");
    layer.push(Feature {
        fid: 1,
        geometry: Some(Geometry::LineString(vec![Coord::xy(-1_000_000_000.0, 5.5), Coord::xy(1_000_000_000.0, 5.5)])),
        attributes: vec![],
    });

    // Zero-column grid must return InvalidArgument instead of panicking in chunks_mut(0).
    let bad_grid = Grid { geo_transform: [0.0, 1.0, 0.0, 10.0, 0.0, -1.0], cols: 0, rows: 10, crs: SpatialRef::default() };
    let err = rasterize(&[layer.clone()], &RasterizeOptions { burn_values: vec![1.0], grid: Some(bad_grid), ..Default::default() });
    assert!(err.is_err());

    // Long horizontal line spanning [-1e9, 1e9] across a 10x10 raster must finish immediately and burn row 4.
    let grid = Grid { geo_transform: [0.0, 1.0, 0.0, 10.0, 0.0, -1.0], cols: 10, rows: 10, crs: SpatialRef::default() };
    let out = rasterize(
        &[layer],
        &RasterizeOptions { burn_values: vec![7.0], grid: Some(grid), data_type: Some(DataType::U8), ..Default::default() },
    )
    .unwrap();
    for col in 0..10 {
        assert_eq!(out.raster.get(0, 4, col), 7.0);
        assert_eq!(out.raster.get(0, 3, col), 0.0);
    }

    // Also check a diagonal line with endpoints near +/-2e9 (where ix1 - ix would overflow i32).
    let mut diag = Layer::new("diag");
    diag.push(Feature {
        fid: 1,
        geometry: Some(Geometry::LineString(vec![
            Coord::xy(-2_000_000_000.0, -2_000_000_000.0),
            Coord::xy(2_000_000_000.0, 2_000_000_000.0),
        ])),
        attributes: vec![],
    });
    let grid = Grid { geo_transform: [0.0, 1.0, 0.0, 10.0, 0.0, -1.0], cols: 10, rows: 10, crs: SpatialRef::default() };
    let out = rasterize(
        &[diag],
        &RasterizeOptions { burn_values: vec![3.0], grid: Some(grid), data_type: Some(DataType::U8), ..Default::default() },
    )
    .unwrap();
    // y = x in world coords maps to py = 10 - px, hitting (row = 10 - col, col) for col in 1..10.
    for col in 1..10 {
        assert_eq!(out.raster.get(0, 10 - col, col), 3.0);
    }
}
