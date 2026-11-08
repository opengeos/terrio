// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Operations use the selected band's nodata, as GDAL 3.13.3 does.
use terrio::ops::{
    calc::{CalcInput, CalcNodata, CalcOptions, calc},
    dem::{ColorMap, DemOptions, DemProduct, color_relief, dem},
    polygonize::{PolygonizeOptions, polygonize},
    sample::{SampleInput, sample},
    sieve::{SieveOptions, sieve},
};
use terrio::raster::RasterData;
use terrio::{DataType, Feature, FieldValue, Geometry, Layer, Raster, RasterConfig, RasterDataset};

fn source() -> RasterDataset {
    let config = RasterConfig { cols: 5, rows: 5, bands: 2, data_type: DataType::F64, nodata: -1.0, ..Default::default() };
    let mut values = vec![100.0; 25];
    values.extend([10.0; 25]);
    values[25] = -2.0;
    values[26] = -1.0; // Valid in band 2 despite being band 1's nodata.
    values[37] = 20.0;
    let mut ds = RasterDataset::new(Raster::from_data_native(config, RasterData::F64(values)).unwrap(), true);
    ds.band_nodata = vec![Some(-1.0), Some(-2.0)];
    ds.set_geo_transform([0.0, 1.0, 0.0, 5.0, 0.0, -1.0]);
    ds
}

#[test]
fn calc_and_sample_use_selected_band_nodata() {
    let ds = source();
    let output = calc(
        &[CalcInput { name: "A".into(), ds: &ds, band: 2 }],
        &CalcOptions { calc: vec!["A".into()], nodata: CalcNodata::Value(-9999.0), ..Default::default() },
    )
    .unwrap();
    assert_eq!(output.raster.data.get_f64(0), -9999.0);
    assert_eq!(output.raster.data.get_f64(1), -1.0);
    assert_eq!(output.band_nodata(1), Some(-9999.0));
    let mut points = Layer::new("points");
    for i in 0..2 {
        points.push(Feature { fid: i, geometry: Some(Geometry::point(i as f64 + 0.5, 4.5)), attributes: vec![] });
    }
    let output = sample(&points, &[SampleInput { field_name: "value".into(), raster: &ds, band: 2 }]).unwrap();
    assert_eq!(output.features[0].attributes, vec![FieldValue::Null]);
    assert_eq!(output.features[1].attributes, vec![FieldValue::Float(-1.0)]);
}

#[test]
fn sieve_and_polygonize_use_selected_band_nodata() {
    let ds = source();
    let output = sieve(&ds, &SieveOptions { band: 2, threshold: 2, ..Default::default() }).unwrap();
    assert_eq!(output.band_nodata(1), Some(-2.0));
    assert_eq!(output.raster.data.get_f64(0), -2.0);
    assert_eq!(output.raster.data.get_f64(12), 10.0);
    let output = polygonize(&ds, &PolygonizeOptions { band: 1, ..Default::default() }).unwrap();
    let values: Vec<_> = output.features.iter().map(|f| f.attributes[0].as_f64().unwrap()).collect();
    assert!(!values.contains(&-2.0));
    assert!(values.contains(&-1.0));
}

#[test]
fn terrain_and_color_percentages_use_selected_band_nodata() {
    let mut ds = source();
    let colors = ColorMap::parse("0% 10 20 30\n100% 40 50 60\nnv 1 2 3", &ds, 2, None).unwrap();
    assert!(colors.entries.iter().any(|e| e.r == 10 && e.value == -1.0));
    assert!(colors.entries.iter().any(|e| e.r == 40 && e.value == 20.0));
    assert!(colors.entries.iter().any(|e| e.r == 1 && e.value == -2.0));
    let output = color_relief(&ds, &colors, &DemOptions { band: 2, ..Default::default() }).unwrap();
    assert_eq!(output.raster.data.get_f64(0), 1.0);
    assert_eq!(output.raster.data.get_f64(1), 10.0);
    if let RasterData::F64(values) = &mut ds.raster.data {
        values[37] = -2.0;
    }
    let output = dem(&ds, DemProduct::Roughness, &DemOptions { band: 2, ..Default::default() }).unwrap();
    assert_eq!(output.raster.data.get_f64(12), output.band_nodata(1).unwrap());
}
