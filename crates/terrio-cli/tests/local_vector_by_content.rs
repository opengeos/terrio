// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! `terrio info` and `terrio convert` read a local GeoJSON or GML document whose name points to no
//! driver by its content.

use std::process::Command;

const GEOJSON: &[u8] = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"road"},"geometry":{"type":"LineString","coordinates":[[10,46],[11,47]]}}]}"#;
const GML: &[u8] = br#"<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:app="urn:test"><wfs:member><app:road gml:id="road.1"><app:name>road</app:name><app:geom><gml:LineString srsName="urn:ogc:def:crs:EPSG::25832"><gml:posList srsDimension="2">700000 5100000 700001 5100001</gml:posList></gml:LineString></app:geom></app:road></wfs:member></wfs:FeatureCollection>"#;

#[test]
fn info_and_convert_read_documents_under_names_of_no_driver() {
    let dir = tempfile::tempdir().unwrap();
    for (name, body, driver) in [("data", GEOJSON, "GeoJSON"), ("data.txt", GEOJSON, "GeoJSON"), ("features", GML, "GML")] {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        let terrio = || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_terrio"));
            command.env_remove("TERRIO_ALLOWED_DRIVERS");
            command
        };
        let info = terrio().args(["info", path.to_str().unwrap()]).output().unwrap();
        let stdout = String::from_utf8_lossy(&info.stdout);
        assert!(info.status.success(), "{name}: {}", String::from_utf8_lossy(&info.stderr));
        assert!(stdout.contains(&format!("Driver: {driver}")), "{name}: {stdout}");
        let out = dir.path().join(format!("{name}-out.geojson"));
        let convert = terrio().args(["convert", path.to_str().unwrap(), out.to_str().unwrap()]).output().unwrap();
        assert!(convert.status.success(), "{name}: {}", String::from_utf8_lossy(&convert.stderr));
        assert_eq!(terrio::open_vector(out.to_str().unwrap(), &Default::default()).unwrap().layers[0].features.len(), 1, "{name}");
    }
}
