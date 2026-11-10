// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Local GeoJSON and GML documents under names that point to no driver (`data`, `data.txt`, a GML
//! without its extension): read by their content, as a remote response under such a name is
//! (`remote_vector_response.rs`), and only with their driver allowed.

use std::collections::HashMap;

const GEOJSON: &[u8] = br#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"EPSG:25832"}},"features":[{"type":"Feature","properties":{"name":"road"},"geometry":{"type":"LineString","coordinates":[[700000,5100000],[700001,5100001]]}}]}"#;
const GML: &[u8] = br#"<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:app="urn:test"><wfs:member><app:road gml:id="road.1"><app:name>road</app:name><app:geom><gml:LineString srsName="urn:ogc:def:crs:EPSG::25832"><gml:posList srsDimension="2">700000 5100000 700001 5100001</gml:posList></gml:LineString></app:geom></app:road></wfs:member></wfs:FeatureCollection>"#;

/// GeoJSON features one per line, without the RS separators of a text sequence: terrio's GeoJSON
/// reader refuses the document.
const FEATURES_PER_LINE: &[u8] = b"{\"type\":\"Feature\",\"properties\":{\"id\":0},\"geometry\":{\"type\":\"Point\",\"coordinates\":[10,46]}}\n{\"type\":\"Feature\",\"properties\":{\"id\":1},\"geometry\":{\"type\":\"Point\",\"coordinates\":[11,46]}}\n";
/// GML whose two features use two CRSs: terrio's GML reader refuses the document.
const MIXED_CRS_GML: &[u8] = br#"<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:app="urn:test"><wfs:member><app:road gml:id="road.1"><app:name>a</app:name><app:geom><gml:LineString srsName="urn:ogc:def:crs:EPSG::25832"><gml:posList srsDimension="2">700000 5100000 700001 5100001</gml:posList></gml:LineString></app:geom></app:road></wfs:member><wfs:member><app:road gml:id="road.2"><app:name>b</app:name><app:geom><gml:LineString srsName="urn:ogc:def:crs:EPSG::4326"><gml:posList srsDimension="2">46 10 46.001 10.001</gml:posList></gml:LineString></app:geom></app:road></wfs:member></wfs:FeatureCollection>"#;

/// `f` with `TERRIO_ALLOWED_DRIVERS=drivers` on the calling thread.
fn allowing<T>(drivers: &str, f: impl FnOnce() -> T) -> T {
    terrio::config::push_scope(HashMap::from([("TERRIO_ALLOWED_DRIVERS".to_string(), Some(drivers.to_string()))]));
    let out = f();
    terrio::config::pop_scope();
    out
}

/// Every entry point reads `path` as one feature of `driver`, in EPSG:25832.
fn check_read(path: &str, driver: &str) {
    let ds = terrio::open_vector(path, &Default::default()).unwrap();
    assert_eq!(ds.driver.name(), driver, "{path}");
    assert_eq!(ds.layers[0].features.len(), 1, "{path}");
    assert_eq!(ds.layers[0].crs_epsg(), Some(25832), "{path}");
    assert!(matches!(terrio::open(path), Ok(terrio::Dataset::Vector(_))), "{path}");
    let terrio::info::Info::Vector(info) = terrio::info::info_path(path, &Default::default()).unwrap() else {
        panic!("{path}: not vector")
    };
    assert_eq!(info.layers[0].feature_count, 1, "{path}");
    assert_eq!(terrio::list_layers(path).unwrap().len(), 1, "{path}");
}

#[test]
fn local_documents_under_names_of_no_driver_are_read_by_content() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, body: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        path.to_str().unwrap().to_string()
    };
    let geojson = [file("data", GEOJSON), file("data.txt", GEOJSON)];
    let gml = file("features", GML);
    for path in &geojson {
        check_read(path, "GeoJSON");
        allowing("GeoJSON", || check_read(path, "GeoJSON"));
        let refused = allowing("CSV", || terrio::open_vector(path, &Default::default())).unwrap_err().to_string();
        assert!(refused.ends_with("holds GEOJSON data, and the GEOJSON driver is not allowed (allowed: CSV)"), "{refused}");
    }
    check_read(&gml, "GML");
    allowing("GML", || check_read(&gml, "GML"));
    let refused = allowing("GeoJSON", || terrio::open_vector(&gml, &Default::default())).unwrap_err().to_string();
    assert!(refused.ends_with("holds GML data, and the GML driver is not allowed (allowed: GEOJSON)"), "{refused}");
}

#[test]
fn a_json_document_that_is_not_geojson_is_still_not_recognized() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["notgeo", "notgeo.txt"] {
        let path = dir.path().join(name);
        std::fs::write(&path, br#"{"name":"x","values":[1,2,3]}"#).unwrap();
        let path = path.to_str().unwrap();
        for error in [terrio::open_vector(path, &Default::default()).map(|_| ()).unwrap_err(), terrio::open(path).map(|_| ()).unwrap_err()]
        {
            assert!(matches!(error, terrio::Error::Unsupported(_)), "{error:?}");
            assert!(error.to_string().contains("format not recognized"), "{error}");
        }
    }
}

/// The documents above under names that point to no driver.
fn refused_documents(dir: &std::path::Path) -> [(String, &'static str); 2] {
    [
        ("lines", FEATURES_PER_LINE, "GeoJSON: trailing characters at line 2 column 1"),
        ("mixed", MIXED_CRS_GML, "GML: mixed coordinate axis conventions within a layer"),
    ]
    .map(|(name, body, refusal)| {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        (path.to_str().unwrap().to_string(), refusal)
    })
}

/// With the `gdal` feature, the GDAL backend reads a GeoJSON or GML document under a name of no
/// driver that terrio's reader refuses, through every entry point.
#[cfg(feature = "gdal")]
#[test]
fn gdal_reads_documents_terrios_readers_refuse() {
    let dir = tempfile::tempdir().unwrap();
    for (path, _) in refused_documents(dir.path()) {
        let ds = terrio::open_vector(&path, &Default::default()).unwrap();
        assert_eq!(ds.driver.name(), "GDAL", "{path}");
        assert_eq!(ds.layers.iter().map(|l| l.features.len()).sum::<usize>(), 2, "{path}");
        assert!(matches!(terrio::open(&path), Ok(terrio::Dataset::Vector(_))), "{path}");
        assert!(matches!(terrio::info::info_path(&path, &Default::default()), Ok(terrio::info::Info::Vector(_))), "{path}");
        assert_eq!(terrio::list_layers(&path).unwrap().len(), ds.layers.len(), "{path}");
    }
}

/// Without the `gdal` feature, terrio's reader refuses them with its own error.
#[cfg(not(feature = "gdal"))]
#[test]
fn terrios_readers_refuse_documents_they_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    for (path, refusal) in refused_documents(dir.path()) {
        let error = terrio::open_vector(&path, &Default::default()).map(|_| ()).unwrap_err();
        assert!(error.to_string().ends_with(refusal), "{error}");
    }
}

/// Without the `gdal` feature, with `TERRIO_WBVECTOR_FALLBACK=YES`, a document terrio's GML reader
/// refuses under a name of no driver goes on to the `wbvector` fallback, which reads it.
#[cfg(not(feature = "gdal"))]
#[test]
fn the_wbvector_fallback_reads_a_document_terrios_reader_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let [_, (mixed, _)] = refused_documents(dir.path());
    terrio::config::push_scope(HashMap::from([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))]));
    let read = terrio::open_vector(&mixed, &Default::default());
    terrio::config::pop_scope();
    let layers = read.unwrap().layers;
    assert_eq!(layers.iter().map(|l| (l.name.as_str(), l.features.len())).collect::<Vec<_>>(), [("mixed", 2)]);
}

/// Without the `gdal` feature, with `TERRIO_WBVECTOR_FALLBACK=YES`, a document that the fallback
/// cannot read either keeps the error of terrio's reader: GeoJSON the fallback does not recognize
/// under a name of no driver, and GML cut short, which it recognizes but cannot read.
#[cfg(not(feature = "gdal"))]
#[test]
fn a_document_the_wbvector_fallback_cannot_read_keeps_its_readers_error() {
    let dir = tempfile::tempdir().unwrap();
    let [lines, _] = refused_documents(dir.path());
    let cut = dir.path().join("cut");
    std::fs::write(&cut, &MIXED_CRS_GML[..MIXED_CRS_GML.len() / 2]).unwrap();
    let cut = (cut.to_str().unwrap().to_string(), "XML: syntax error: tag not closed: `>` not found before end of input");
    terrio::config::push_scope(HashMap::from([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))]));
    let errors: Vec<_> = [lines, cut]
        .into_iter()
        .flat_map(|(path, refusal)| {
            [terrio::open_vector(&path, &Default::default()).map(|_| ()), terrio::open(&path).map(|_| ())]
                .map(|read| (read.unwrap_err().to_string(), refusal))
        })
        .collect();
    terrio::config::pop_scope();
    for (error, refusal) in errors {
        assert!(error.ends_with(refusal), "{error}");
    }
}
