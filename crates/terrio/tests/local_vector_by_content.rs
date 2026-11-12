// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Local GeoJSON and GML documents under names that point to no driver (`data`, `data.txt`, a GML
//! without its extension): read by their content, as a remote response under such a name is
//! (`remote_vector_response.rs`), and only with their driver allowed.

use std::collections::HashMap;

struct ScopeGuard;
impl Drop for ScopeGuard {
    fn drop(&mut self) {
        terrio::config::pop_scope();
    }
}

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
    let _scope = ScopeGuard;
    f()
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
        ("mixed", MIXED_CRS_GML, "GML: mixed coordinate reference systems within a layer"),
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
    let _scope = ScopeGuard;
    let read = terrio::open_vector(&mixed, &Default::default()).unwrap();
    assert_eq!(read.driver.name(), "GML");
    let layers = read.layers;
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
    let cut = (cut.to_str().unwrap().to_string(), "XML: syntax error");
    terrio::config::push_scope(HashMap::from([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))]));
    let _scope = ScopeGuard;
    let errors: Vec<_> = [lines, cut]
        .into_iter()
        .flat_map(|(path, refusal)| {
            [terrio::open_vector(&path, &Default::default()).map(|_| ()), terrio::open(&path).map(|_| ())]
                .map(|read| (read.unwrap_err().to_string(), refusal))
        })
        .collect();
    for (error, refusal) in errors {
        assert!(error.contains(refusal), "{error}");
    }
}

/// A fallback misidentification cannot replace the native reader's more specific error.
#[cfg(not(feature = "gdal"))]
#[test]
fn a_disallowed_fallback_keeps_the_native_readers_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lines");
    let text = std::str::from_utf8(FEATURES_PER_LINE).unwrap().replace("\"id\":0", "\"marker\":\"<kml\",\"id\":0");
    std::fs::write(&path, text).unwrap();
    terrio::config::push_scope(HashMap::from([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))]));
    let _scope = ScopeGuard;
    let error = allowing("GeoJSON", || terrio::open_vector(path.to_str().unwrap(), &Default::default())).unwrap_err();
    assert!(error.to_string().contains("GeoJSON: trailing characters"), "{error}");
}

/// Enabling the fallback still prefers the native GML reader when it accepts the document.
#[test]
fn native_gml_is_preferred_with_the_fallback_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("features");
    std::fs::write(&path, GML).unwrap();
    terrio::config::push_scope(HashMap::from([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))]));
    let _scope = ScopeGuard;
    check_read(path.to_str().unwrap(), "GML");
}

/// A GPX document and a KML document of three points each, point `i` at `(i, 0)`.
#[cfg(not(feature = "gdal"))]
fn gpx_and_kml() -> [(&'static str, String, &'static str); 2] {
    let points = |each: &dyn Fn(usize) -> String| (0..3).map(each).collect::<String>();
    let gpx = r#"<?xml version="1.0"?><gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1">"#.to_string()
        + &points(&|i| format!(r#"<wpt lat="0" lon="{i}"><name>p{i}</name></wpt>"#))
        + "</gpx>";
    let kml = r#"<?xml version="1.0"?><kml xmlns="http://www.opengis.net/kml/2.2"><Document>"#.to_string()
        + &points(&|i| format!("<Placemark><name>k{i}</name><Point><coordinates>{i},0</coordinates></Point></Placemark>"))
        + "</Document></kml>";
    [("points.gpx", gpx, "GPX"), ("places.kml", kml, "KML")]
}

/// The x of the points of the first layer `path` gives with `skip_features` and `max_features`.
#[cfg(not(feature = "gdal"))]
fn xs_in_range(path: &str, skip_features: usize, max_features: Option<usize>) -> (String, Vec<f64>) {
    let options = terrio::VectorOpenOptions { skip_features, max_features, ..Default::default() };
    let read = terrio::open_vector(path, &options).unwrap();
    let xs = read.layers[0]
        .features
        .iter()
        .map(|f| match &f.geometry {
            Some(terrio::Geometry::Point(c)) => c.x,
            other => panic!("{path}: not a point: {other:?}"),
        })
        .collect();
    (read.driver.name().to_string(), xs)
}

/// Without the `gdal` feature, with `TERRIO_WBVECTOR_FALLBACK=YES`, a document only the `wbvector`
/// fallback recognises (GPX or KML under a name of no driver) gives the features `skip_features` and
/// `max_features` ask for, and the driver of its format, as the same document under its own
/// extension does (#726). So do a GML document terrio's reader refuses, which goes on to the
/// fallback, and a GeoPackage under a name of no driver.
#[cfg(not(feature = "gdal"))]
#[test]
fn the_wbvector_fallback_keeps_the_range_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    terrio::config::push_scope(HashMap::from([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))]));
    let _scope = ScopeGuard;
    // A GML document terrio's reader refuses, which goes on to the fallback: its second feature
    // alone.
    let [_, (mixed, _)] = refused_documents(dir.path());
    let features = |options: &terrio::VectorOpenOptions| {
        let read = terrio::open_vector(&mixed, options).unwrap();
        (read.driver.name().to_string(), read.layers[0].features.iter().map(|f| f.attributes.clone()).collect::<Vec<_>>())
    };
    let (driver, all) = features(&Default::default());
    let ranged = features(&terrio::VectorOpenOptions { skip_features: 1, max_features: Some(1), ..Default::default() });
    assert_eq!((driver.as_str(), all.len()), ("GML", 2));
    assert_ne!(all[0], all[1]);
    assert_eq!(ranged, ("GML".to_string(), all[1..].to_vec()));
    // GPX and KML, under their own extensions and under a name of no driver.
    let ranges: [(usize, Option<usize>, &[f64]); 5] =
        [(1, Some(1), &[1.0]), (2, None, &[2.0]), (0, Some(2), &[0.0, 1.0]), (5, None, &[]), (0, None, &[0.0, 1.0, 2.0])];
    let (mut got, mut want) = (Vec::new(), Vec::new());
    for (name, document, driver) in gpx_and_kml() {
        let named = dir.path().join(name);
        let unnamed = named.with_extension("bin");
        for path in [&named, &unnamed] {
            std::fs::write(path, &document).unwrap();
            for (skip, max, xs) in ranges {
                let file = path.file_name().unwrap().to_str().unwrap();
                got.push((file.to_string(), skip, max, xs_in_range(path.to_str().unwrap(), skip, max)));
                want.push((file.to_string(), skip, max, (driver.to_string(), xs.to_vec())));
            }
        }
    }
    // A GeoPackage under a name of no driver, which the fallback finds by its SQLite header: the
    // points of the KML document, written by terrio.
    #[cfg(feature = "sqlite")]
    {
        let places = terrio::open_vector(dir.path().join("places.kml").to_str().unwrap(), &Default::default()).unwrap();
        let gpkg = dir.path().join("rows.gpkg");
        terrio::write_vector(&places.layers, gpkg.to_str().unwrap(), &Default::default()).unwrap();
        let unnamed = gpkg.with_extension("bin");
        std::fs::copy(&gpkg, &unnamed).unwrap();
        for (skip, max, xs) in ranges {
            got.push(("rows.bin".to_string(), skip, max, xs_in_range(unnamed.to_str().unwrap(), skip, max)));
            want.push(("rows.bin".to_string(), skip, max, ("GPKG".to_string(), xs.to_vec())));
        }
    }
    assert_eq!(got, want);
}
