// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Streamed ranges follow the eager reader's live-feature order, after spatial filtering.
//! GDAL 3.13.3 is the reference for the feature-range ordering contract.

use terrio::{FieldValue, Layer, VectorOpenOptions};

fn points(name: &str, start: i32, count: i32) -> Layer {
    let features: Vec<_> = (0..count)
        .map(|i| {
            serde_json::json!({
                "type": "Feature", "properties": {"sequence": start + i},
                "geometry": {"type": "Point", "coordinates": [10 + i, 46]}
            })
        })
        .collect();
    let document = serde_json::json!({"type": "FeatureCollection", "features": features});
    terrio::memory::parse_geojson_layer(document.to_string().as_bytes(), name).unwrap()
}

fn sequences(layer: &Layer) -> Vec<String> {
    let index = layer.schema.field_index("sequence").unwrap();
    layer
        .features
        .iter()
        .map(|feature| match &feature.attributes[index] {
            FieldValue::Integer(value) => value.to_string(),
            FieldValue::Float(value) => value.to_string(),
            FieldValue::Text(value) => value.clone(),
            value => panic!("unexpected sequence value: {value:?}"),
        })
        .collect()
}

#[test]
fn stac_assets_forward_layer_range_and_spatial_options() {
    let directory = tempfile::tempdir().unwrap();
    let mut formats = vec!["geojson"];
    if cfg!(feature = "sqlite") {
        formats.push("gpkg");
    }
    for extension in formats {
        let asset = directory.path().join(format!("points.{extension}"));
        let mut layers = vec![points("points", 0, 7)];
        if extension == "gpkg" {
            layers.push(points("second", 100, 4));
        }
        terrio::write_vector(&layers, asset.to_str().unwrap(), &Default::default()).unwrap();
        let item = directory.path().join(format!("item-{extension}.json"));
        let document = serde_json::json!({
            "type": "Feature", "stac_version": "1.0.0", "id": "ranges",
            "geometry": {"type": "Point", "coordinates": [10, 46]}, "bbox": [10, 46, 17, 46],
            "properties": {"datetime": "2024-01-01T00:00:00Z"}, "links": [],
            "assets": {"data": {"href": asset.file_name().unwrap().to_str().unwrap(), "roles": ["data"]}}
        });
        std::fs::write(&item, document.to_string()).unwrap();
        let name = format!("STAC:{}:data", item.display());
        for selection in [vec![], vec![layers.last().unwrap().name.clone()]] {
            for spatial_filter in [None, Some((11.5, 45.5, 14.5, 46.5))] {
                for (skip_features, max_features) in [(0, Some(0)), (1, Some(2)), (2, None), (usize::MAX, Some(1))] {
                    let options =
                        VectorOpenOptions { layers: selection.clone(), spatial_filter, skip_features, max_features, ..Default::default() };
                    let expected = terrio::open_vector(asset.to_str().unwrap(), &options).unwrap();
                    let actual = terrio::open_vector(&name, &options).unwrap();
                    assert_eq!(actual.layers.len(), expected.layers.len(), "{extension}");
                    for (actual, expected) in actual.layers.iter().zip(&expected.layers) {
                        assert_eq!(actual.name, expected.name, "{extension}");
                        assert_eq!(sequences(actual), sequences(expected), "{extension}");
                        assert_eq!(actual.crs_epsg(), expected.crs_epsg(), "{extension}");
                    }
                }
            }
        }
        let options = VectorOpenOptions { layers: vec!["missing".into()], ..Default::default() };
        assert!(terrio::open_vector(&name, &options).unwrap_err().to_string().contains("no layer named missing"));
    }
}

#[test]
#[cfg(feature = "tiles")]
fn mvt_directories_and_tiles_keep_each_selected_layers_range() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tiles");
    let mut options = terrio::VectorWriteOptions { driver: Some(terrio::formats::VectorDriver::MVT), ..Default::default() };
    options.tiles.max_zoom = Some(1);
    terrio::write_vector(&[points("five", 0, 5), points("three", 100, 3)], path.to_str().unwrap(), &options).unwrap();
    let tile = path.join("0/0/0.pbf");
    assert!(tile.is_file());
    for path in [&path, &tile] {
        let full = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(full.layers.len(), 2);
        for selection in [vec![], vec!["three".into()]] {
            for (skip_features, max_features) in [(0, Some(0)), (1, Some(1)), (2, None), (usize::MAX, Some(1))] {
                let options = VectorOpenOptions { layers: selection.clone(), skip_features, max_features, ..Default::default() };
                let actual = terrio::open_vector(path.to_str().unwrap(), &options).unwrap();
                let expected = full.layers.iter().filter(|l| selection.is_empty() || selection.contains(&l.name)).collect::<Vec<_>>();
                assert_eq!(actual.layers.len(), expected.len());
                for (actual, expected) in actual.layers.iter().zip(expected) {
                    let expected =
                        sequences(expected).into_iter().skip(skip_features).take(max_features.unwrap_or(usize::MAX)).collect::<Vec<_>>();
                    assert_eq!(sequences(actual), expected, "{}", path.display());
                }
            }
        }
    }
}

/// An inverted spatial filter (its corners swapped) reads the features of its corners' extent, as
/// the filter itself takes it: eager and streamed reads of every format, indexed ones included.
#[test]
fn an_inverted_spatial_filter_reads_its_corners_extent() {
    let directory = tempfile::tempdir().unwrap();
    let mut extensions = vec!["geojson", "csv", "shp", "kml", "gml"];
    if cfg!(feature = "sqlite") {
        extensions.push("gpkg");
    }
    if cfg!(feature = "tiles") {
        extensions.push("pmtiles");
    }
    if cfg!(feature = "flatgeobuf") {
        extensions.push("fgb");
    }
    if cfg!(feature = "parquet") {
        extensions.push("parquet");
    }
    for extension in extensions {
        let path = directory.path().join(format!("points.{extension}"));
        let path = path.to_str().unwrap();
        terrio::write_vector(&[points("points", 0, 7)], path, &Default::default()).unwrap();
        // PMTiles filters in EPSG:3857, the other formats in the points' EPSG:4326.
        let rect = if extension == "pmtiles" { (1_280_000.0, 5_700_000.0, 1_620_000.0, 5_860_000.0) } else { (11.5, 45.5, 14.5, 46.5) };
        let options = |rect| VectorOpenOptions { spatial_filter: Some(rect), ..Default::default() };
        let inverted = (rect.2, rect.3, rect.0, rect.1);
        let expected = terrio::open_vector(path, &options(rect)).unwrap();
        assert!(expected.layers.iter().any(|l| !l.features.is_empty()), "{extension}: the rectangle holds features");
        let eager = terrio::open_vector(path, &options(inverted)).unwrap();
        assert_eq!(eager.layers.len(), expected.layers.len(), "{extension}");
        for (a, b) in eager.layers.iter().zip(&expected.layers) {
            assert_eq!(sequences(a), sequences(b), "{extension}, eager");
        }
        if extension != "pmtiles" {
            let streamed = terrio::stream::open_layers(path, &options(inverted), 3).unwrap();
            assert_eq!(streamed.len(), expected.layers.len(), "{extension}, streamed");
            for (reader, b) in streamed.into_iter().zip(&expected.layers) {
                assert_eq!(sequences(&reader.read_all().unwrap()), sequences(b), "{extension}, streamed");
            }
        }
    }
}

#[test]
fn streamed_ranges_match_eager_readers_after_spatial_filtering() {
    let directory = tempfile::tempdir().unwrap();
    let mut extensions = vec!["geojson", "geojsonl", "csv", "shp", "kml", "gml"];
    if cfg!(feature = "sqlite") {
        extensions.push("gpkg");
    }
    if cfg!(feature = "flatgeobuf") {
        extensions.push("fgb");
    }
    if cfg!(feature = "parquet") {
        extensions.push("parquet");
    }
    for extension in extensions {
        let path = directory.path().join(format!("points.{extension}"));
        let path = path.to_str().unwrap();
        terrio::write_vector(&[points("points", 0, 7)], path, &Default::default()).unwrap();
        for spatial_filter in [None, Some((11.5, 45.5, 14.5, 46.5))] {
            for (skip_features, max_features) in
                [(0, None), (0, Some(0)), (1, Some(2)), (2, Some(1)), (5, None), (2, Some(0)), (usize::MAX, Some(3)), (2, Some(usize::MAX))]
            {
                let options = VectorOpenOptions { skip_features, max_features, spatial_filter, ..Default::default() };
                let expected = terrio::open_vector(path, &options).unwrap();
                for batch in [1, 2, 17] {
                    let actual = terrio::stream::open_layers(path, &options, batch)
                        .unwrap()
                        .into_iter()
                        .map(|reader| reader.read_all().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(actual.len(), expected.layers.len(), "{extension}");
                    for (actual, expected) in actual.iter().zip(&expected.layers) {
                        assert_eq!(
                            sequences(actual),
                            sequences(expected),
                            "{extension}, skip={skip_features}, max={max_features:?}, bbox={spatial_filter:?}, batch={batch}"
                        );
                        assert_eq!(actual.crs_epsg(), expected.crs_epsg(), "{extension}");
                        for (actual, expected) in actual.features.iter().zip(&expected.features) {
                            assert_eq!(actual.geometry, expected.geometry, "{extension}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
#[cfg(feature = "flatgeobuf")]
fn unindexed_flatgeobuf_bbox_reads_keep_the_intersecting_features_before_ranges() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unindexed.fgb");
    let mut layer = points("points", 0, 5);
    // A geometry without a bbox makes the writer omit the spatial index.
    layer.features[1].geometry = None;
    terrio::write_vector(&[layer], path.to_str().unwrap(), &Default::default()).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let header = flatgeobuf::size_prefixed_root_as_header(&bytes[8..]).unwrap();
    assert_eq!(header.index_node_size(), 0);
    // Points at x = 10..14: the bbox holds 11 (without a geometry, so left out), 12 and 13.
    for ((skip_features, max_features), expected) in
        [((0, None), &["2", "3"][..]), ((0, Some(0)), &[][..]), ((1, Some(2)), &["3"][..]), ((3, Some(1)), &[][..])]
    {
        let options =
            VectorOpenOptions { spatial_filter: Some((10.5, 45.5, 13.5, 46.5)), skip_features, max_features, ..Default::default() };
        let eager = terrio::open_vector(path.to_str().unwrap(), &options).unwrap();
        assert_eq!(sequences(&eager.layers[0]), expected, "skip={skip_features}, max={max_features:?}");
        for batch in [1, 3] {
            let streamed = terrio::stream::open_layers(path.to_str().unwrap(), &options, batch).unwrap().remove(0).read_all().unwrap();
            assert_eq!(sequences(&streamed), expected, "skip={skip_features}, max={max_features:?}, batch={batch}");
            for (streamed, eager) in streamed.features.iter().zip(&eager.layers[0].features) {
                assert_eq!(streamed.geometry, eager.geometry);
                assert_eq!(streamed.fid, eager.fid);
            }
        }
    }
}

#[test]
#[cfg(feature = "flatgeobuf")]
fn flatgeobuf_bbox_reads_keep_the_same_features_with_or_without_an_index() {
    let directory = tempfile::tempdir().unwrap();
    // A grid of points, and the same points with one geometry left out, which drops the index.
    let grid = || {
        let features: Vec<_> = (0..400)
            .map(|i| serde_json::json!({"type": "Feature", "properties": {"sequence": i}, "geometry": {"type": "Point", "coordinates": [i % 20, i / 20]}}))
            .collect();
        let document = serde_json::json!({"type": "FeatureCollection", "features": features});
        terrio::memory::parse_geojson_layer(document.to_string().as_bytes(), "grid").unwrap()
    };
    let indexed = directory.path().join("indexed.fgb");
    terrio::write_vector(&[grid()], indexed.to_str().unwrap(), &Default::default()).unwrap();
    let unindexed = directory.path().join("unindexed.fgb");
    let mut layer = grid();
    layer.features[399].geometry = None;
    terrio::write_vector(&[layer], unindexed.to_str().unwrap(), &Default::default()).unwrap();
    // x in 3..=6 and y in 4..=5: in the file's order (the indexed file sorts its features).
    let mut expected: Vec<String> = (4..=5).flat_map(|y| (3..=6).map(move |x| (y * 20 + x).to_string())).collect();
    expected.sort();
    // The same rectangle given by its other two corners selects the same features.
    for (path, bbox) in [&indexed, &unindexed].into_iter().flat_map(|p| [(p, (2.5, 3.5, 6.5, 5.5)), (p, (6.5, 5.5, 2.5, 3.5))]) {
        let options = VectorOpenOptions { spatial_filter: Some(bbox), ..Default::default() };
        let path = path.to_str().unwrap();
        let eager = terrio::open_vector(path, &options).unwrap();
        let streamed = terrio::stream::open_layers(path, &options, 3).unwrap().remove(0).read_all().unwrap();
        for layer in [&eager.layers[0], &streamed] {
            let mut got = sequences(layer);
            got.sort();
            assert_eq!(got, expected, "{path}");
        }
        assert_eq!(sequences(&streamed), sequences(&eager.layers[0]), "{path}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn bounded_csv_ranges_keep_one_file_and_stop_after_its_prefetched_prefix() {
    fn file_position(path: &std::path::Path) -> u64 {
        let handles = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| std::fs::read_link(entry.path()).is_ok_and(|target| target == path))
            .collect::<Vec<_>>();
        assert_eq!(handles.len(), 1, "one CSV input handle must remain open");
        let info = std::fs::read_to_string(std::path::Path::new("/proc/self/fdinfo").join(handles[0].file_name())).unwrap();
        info.lines().find_map(|line| line.strip_prefix("pos:\t")).unwrap().parse().unwrap()
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("many.csv");
    let mut csv = "sequence,WKT\n".to_string();
    for index in 0..100_000 {
        csv.push_str(&format!("{index},\"POINT ({} 46)\"\n", 10 + index % 100));
    }
    std::fs::write(&path, csv).unwrap();
    let path = path.canonicalize().unwrap();
    let length = std::fs::metadata(&path).unwrap().len();
    for max_features in [Some(0), Some(1), None] {
        let options = VectorOpenOptions { max_features, ..Default::default() };
        let mut reader = terrio::stream::open_layers(path.to_str().unwrap(), &options, 1).unwrap().remove(0);
        let before = file_position(&path);
        let mut count = 0;
        while let Some(chunk) = reader.read_chunk(16_384).unwrap() {
            count += chunk.features.len();
        }
        let after = file_position(&path);
        assert_eq!(count, max_features.unwrap_or(100_000));
        match max_features {
            Some(0) => assert_eq!(after, before),
            Some(1) => assert!(after < length),
            None => assert_eq!(after, length),
            _ => unreachable!(),
        }
        eprintln!("CSV range {max_features:?}: one file open, cursor {before} -> {after}, file {length} bytes");
    }
}

#[test]
#[cfg(feature = "sqlite")]
fn streamed_ranges_restart_for_each_selected_layer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("two.gpkg");
    let path = path.to_str().unwrap();
    terrio::write_vector(&[points("first", 0, 5), points("second", 100, 4)], path, &Default::default()).unwrap();
    for layers in [vec![], vec!["second".to_string()]] {
        let options = VectorOpenOptions { layers, skip_features: 1, max_features: Some(2), ..Default::default() };
        let expected = terrio::open_vector(path, &options).unwrap();
        let actual = terrio::stream::open_layers(path, &options, 2)
            .unwrap()
            .into_iter()
            .map(|reader| reader.read_all().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual.len(), expected.layers.len());
        for (actual, expected) in actual.iter().zip(&expected.layers) {
            assert_eq!(actual.name, expected.name);
            assert_eq!(sequences(actual), sequences(expected));
        }
    }
}

#[test]
fn streamed_shapefile_range_counts_live_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("deleted.shp");
    terrio::write_vector(&[points("deleted", 0, 6)], path.to_str().unwrap(), &Default::default()).unwrap();
    let dbf = path.with_extension("dbf");
    let mut bytes = std::fs::read(&dbf).unwrap();
    let header = u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as usize;
    let record = u16::from_le_bytes(bytes[10..12].try_into().unwrap()) as usize;
    bytes[header + record] = b'*';
    std::fs::write(dbf, bytes).unwrap();
    let options = VectorOpenOptions { skip_features: 1, max_features: Some(2), ..Default::default() };
    let expected = terrio::open_vector(path.to_str().unwrap(), &options).unwrap();
    assert_eq!(sequences(&expected.layers[0]), ["2", "3"]);
    for batch in [1, 2, 17] {
        let actual = terrio::stream::open_layers(path.to_str().unwrap(), &options, batch).unwrap().remove(0).read_all().unwrap();
        assert_eq!(sequences(&actual), ["2", "3"], "batch={batch}");
    }
}
