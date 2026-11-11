// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Filesystem-free range and option validation, compared with GDAL 3.13.3's range contract.

use super::*;

fn fixture() -> Layer {
    parse_geojson_layer(
        br#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"sequence":0},"geometry":{"type":"Point","coordinates":[10,46]}},
            {"type":"Feature","properties":{"sequence":1},"geometry":{"type":"Point","coordinates":[11,46]}},
            {"type":"Feature","properties":{"sequence":2},"geometry":{"type":"Point","coordinates":[12,46]}},
            {"type":"Feature","properties":{"sequence":3},"geometry":{"type":"Point","coordinates":[13,46]}}
        ]}"#,
        "points",
    )
    .unwrap()
}

fn encoded() -> Vec<(String, Vec<u8>)> {
    let mut drivers = vec![
        VectorDriver::GeoJSON,
        VectorDriver::GeoJSONSeq,
        VectorDriver::CSV,
        VectorDriver::GML,
        VectorDriver::GPX,
        VectorDriver::KML,
        VectorDriver::KMZ,
        VectorDriver::TopoJSON,
    ];
    if cfg!(feature = "flatgeobuf") {
        drivers.push(VectorDriver::FlatGeobuf);
    }
    if cfg!(feature = "parquet") {
        drivers.push(VectorDriver::Parquet);
    }
    if cfg!(feature = "sqlite") {
        drivers.push(VectorDriver::GPKG);
    }
    let mut formats = drivers
        .into_iter()
        .map(|driver| {
            let name = format!("points.{}", driver.info().extensions[0]);
            let bytes = codec::write_vector(&[fixture()], driver, &Default::default()).unwrap();
            (name, bytes)
        })
        .collect::<Vec<_>>();
    formats.push(("points.shp.zip".into(), write_shapefile_zip_bytes(&fixture(), "points").unwrap()));
    #[cfg(feature = "tiles")]
    {
        use crate::tiles::mvt::{MvtFeature, MvtLayer, MvtValue, encode, encode_points};
        let layers = ["first", "second"]
            .into_iter()
            .map(|name| MvtLayer {
                name: name.into(),
                extent: 4096,
                features: (0..4)
                    .map(|index| MvtFeature {
                        id: Some(index),
                        geom_type: crate::tiles::mvt::POINT,
                        geometry: encode_points(&[(100 + index as i32, 200)]),
                        tags: vec![("sequence".into(), MvtValue::UInt(index))],
                        ..Default::default()
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        formats.push(("0/0/0.pbf".into(), encode(&layers)));
    }
    formats
}

#[test]
fn filesystem_free_ranges_slice_each_selected_layer_once() {
    for (name, bytes) in encoded() {
        let Dataset::Vector(full) = codec::open(bytes.clone(), &name, &Default::default()).unwrap() else {
            panic!("{name}: expected vector dataset");
        };
        for (skip_features, max_features) in [(0, Some(0)), (1, Some(2)), (2, None), (usize::MAX, Some(1)), (1, Some(usize::MAX))] {
            let options = VectorOpenOptions { skip_features, max_features, ..Default::default() };
            let Dataset::Vector(actual) = codec::open(bytes.clone(), &name, &options).unwrap() else {
                panic!("{name}: expected vector dataset");
            };
            assert_eq!(actual.layers.len(), full.layers.len(), "{name}");
            for (actual, source) in actual.layers.iter().zip(&full.layers) {
                let mut expected = source.clone();
                expected.features = source.features.iter().skip(skip_features).take(max_features.unwrap_or(usize::MAX)).cloned().collect();
                assert_eq!(actual.features.len(), expected.features.len(), "{name}: skip {skip_features}, max {max_features:?}");
                for (actual, expected) in actual.features.iter().zip(&expected.features) {
                    assert_eq!(actual.attributes, expected.attributes, "{name}");
                    assert_eq!(actual.geometry, expected.geometry, "{name}");
                    assert_eq!(actual.fid, expected.fid, "{name}");
                }
                assert_eq!(actual.name, source.name, "{name}");
                assert_eq!(format!("{:?}", actual.schema.fields()), format!("{:?}", source.schema.fields()), "{name}");
                assert_eq!(actual.crs_epsg(), source.crs_epsg(), "{name}");
            }
        }
    }
}

#[test]
fn filesystem_free_readers_refuse_service_options_before_dispatch() {
    for (name, bytes) in encoded() {
        let mut options = VectorOpenOptions::default();
        options.oapif.datetime = Some("2024-01-01".into());
        for label in [&name, "OAPIF:https://example.invalid/collections/points"] {
            let error = codec::open(bytes.clone(), label, &options).unwrap_err();
            assert!(error.to_string().contains("OAPIF"), "{name}: {error}");
        }
    }
}
