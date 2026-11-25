// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Operations use the selected band's nodata, as GDAL 3.13.3 does.
use terrio::ops::{
    calc::{CalcInput, CalcNodata, CalcOptions, calc},
    contour::{ContourOptions, contour},
    dem::{ColorMap, DemOptions, DemProduct, color_relief, dem},
    fill_nodata::{FillNodataOptions, fill_nodata},
    geolocation::GeolocationTransformer,
    polygonize::{PolygonizeOptions, polygonize},
    proximity::{ProximityOptions, proximity},
    sample::{SampleInput, sample},
    sieve::{SieveOptions, sieve},
};
use terrio::raster::RasterData;
use terrio::{DataType, Feature, FieldValue, Geolocation, Geometry, Layer, Raster, RasterConfig, RasterDataset};

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

#[test]
fn fill_nodata_contour_proximity_and_geolocation_use_selected_band_nodata() {
    let ds = source();

    // fill_nodata: pixel 0 in band 2 is -2.0 (band 2's nodata) and must be filled;
    // pixel 1 is -1.0 (band 1's nodata, valid in band 2) and must stay -1.0.
    let filled = fill_nodata(&ds, &FillNodataOptions { band: 2, ..Default::default() }).unwrap();
    assert_eq!(filled.band_nodata(1), Some(-2.0));
    assert_ne!(filled.raster.data.get_f64(0), -2.0);
    assert_eq!(filled.raster.data.get_f64(1), -1.0);

    // contour: band 2's valid range includes -1.0 (not nodata) and excludes -2.0 (nodata).
    let contours = contour(&ds, &ContourOptions { band: 2, levels: vec![-1.5], ..Default::default() }).unwrap();
    assert!(contours.features.is_empty(), "nodata -2.0 in band 2 must not generate a -1.5 contour");
    let valid_contours = contour(&ds, &ContourOptions { band: 2, levels: vec![0.0], ..Default::default() }).unwrap();
    assert!(!valid_contours.features.is_empty(), "valid -1.0 in band 2 must participate in the 0.0 contour");

    // proximity: with use_input_nodata, band 2's -2.0 pixel is skipped while -1.0 receives distance.
    let prox =
        proximity(&ds, &ProximityOptions { band: 2, values: vec![20], use_input_nodata: true, nodata: Some(-99.0), ..Default::default() })
            .unwrap();
    assert_eq!(prox.raster.data.get_f64(0), -99.0);
    assert!(prox.raster.data.get_f64(1) > 0.0);

    // geolocation: band 2's -2.0 marks an invalid node, while -1.0 is a valid coordinate.
    let mut lon = source();
    let mut lat = source();
    if let RasterData::F64(v) = &mut lon.raster.data {
        for r in 0..5 {
            for c in 0..5 {
                v[25 + r * 5 + c] = c as f64 - 1.0; // pixel (0,0) = -1.0 (valid in band 2!)
            }
        }
        v[25 + 24] = -2.0; // pixel (4,4) = -2.0 (band 2's nodata)
    }
    if let RasterData::F64(v) = &mut lat.raster.data {
        for r in 0..5 {
            for c in 0..5 {
                v[25 + r * 5 + c] = 10.0 + r as f64;
            }
        }
    }
    let meta = Geolocation {
        srs: "EPSG:4326".into(),
        x_dataset: String::new(),
        y_dataset: String::new(),
        x_band: 2,
        y_band: 2,
        pixel_offset: 0.0,
        line_offset: 0.0,
        pixel_step: 1.0,
        line_step: 1.0,
        pixel_center: false,
    };
    let t = GeolocationTransformer::new(&meta, &lon, &lat, 5, 5).unwrap();
    // (0.5, 0.5) is near (-1.0, 10.0), which is valid in band 2.
    assert!(t.to_geo(0.5, 0.5).is_some(), "valid -1.0 in band 2 must not be masked by band 1 nodata");
    // (4.5, 4.5) is at the -2.0 nodata cell in band 2.
    assert!(t.to_geo(4.5, 4.5).is_none(), "band 2 nodata -2.0 must be masked");
}
