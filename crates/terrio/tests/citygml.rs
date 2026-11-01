// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityGML encoding regressions. GDAL 3.13.3 is an informational cross-reader.
use terrio::{VectorDataset, formats::VectorDriver};
use wbvector::{FieldValue, Geometry};

fn model(version: &str, body: &str, crs: &str) -> String {
    let gml = if version == "2.0" { "http://www.opengis.net/gml" } else { "http://www.opengis.net/gml/3.2" };
    format!(
        r#"<c:CityModel xmlns:c="http://www.opengis.net/citygml/{version}" xmlns:g="{gml}" xmlns:b="http://www.opengis.net/citygml/building/{version}" xmlns:l="http://www.w3.org/1999/xlink">
    <g:boundedBy><g:Envelope srsName="{crs}" srsDimension="3"><g:lowerCorner>0 0 0</g:lowerCorner><g:upperCorner>1000 1000 1000</g:upperCorner></g:Envelope></g:boundedBy>
    <c:cityObjectMember><b:Building g:id="house">{body}</b:Building></c:cityObjectMember></c:CityModel>"#
    )
}
fn read(text: &str, name: &str) -> terrio::Result<VectorDataset> {
    terrio::memory::open_vector_bytes(text.as_bytes().to_vec(), name, &Default::default())
}
const FACE: &str = r#"<g:Polygon g:id="wall"><g:exterior><g:LinearRing><g:posList>10 0 0 10 10 0 10 10 10 10 0 10 10 0 0</g:posList></g:LinearRing></g:exterior><g:interior><g:LinearRing><g:posList>10 2 2 10 3 2 10 3 3 10 2 2</g:posList></g:LinearRing></g:interior></g:Polygon>"#;

#[test]
fn local_links_and_thematic_boundaries_share_one_vertical_face() {
    for version in ["2.0", "3.0"] {
        let body = format!(
            r##"<g:name>A &amp; B</g:name><b:lod2Solid><g:Solid><g:exterior><g:CompositeSurface><g:surfaceMember l:href="#wall"/></g:CompositeSurface></g:exterior></g:Solid></b:lod2Solid><b:boundedBy><b:WallSurface g:id="surface"><b:lod2MultiSurface><g:MultiSurface><g:surfaceMember>{FACE}</g:surfaceMember></g:MultiSurface></b:lod2MultiSurface></b:WallSurface></b:boundedBy>"##
        );
        for name in ["house.gml", "house.citygml", "house.xml"] {
            let ds = read(&model(version, &body, "EPSG:32632"), name).unwrap();
            assert_eq!(ds.driver, VectorDriver::CityGML);
            let layer = &ds.layers[0];
            assert_eq!(layer.features.len(), 1);
            assert_eq!(layer.crs_epsg(), Some(32632));
            let f = &layer.features[0];
            assert_eq!(f.attributes[0], FieldValue::Text("house".into()));
            assert_eq!(f.attributes[2], FieldValue::Text("2".into()));
            assert_eq!(f.attributes[7], FieldValue::Text("{\"type\":\"WallSurface\"}".into()));
            let FieldValue::Text(attrs) = &f.attributes[4] else { panic!() };
            assert_eq!(serde_json::from_str::<serde_json::Value>(attrs).unwrap()["name"], "A & B");
            let Geometry::Polygon { exterior, interiors } = f.geometry.as_ref().unwrap() else { panic!() };
            assert_eq!(exterior.len(), 4);
            assert_eq!(interiors.len(), 1);
            assert_eq!(exterior.coords()[3].z, Some(10.0));
        }
    }
}

#[test]
fn implicit_matrix_is_applied_before_reference_point() {
    let body = r#"<b:lod1ImplicitRepresentation><c:ImplicitGeometry><c:transformationMatrix>2 0 0 10 0 3 0 20 0 0 4 30 0 0 0 1</c:transformationMatrix><c:referencePoint><g:Point><g:pos>100 200 300</g:pos></g:Point></c:referencePoint><c:relativeGeometry><g:MultiPoint><g:pointMember><g:Point><g:pos>1 2 3</g:pos></g:Point></g:pointMember></g:MultiPoint></c:relativeGeometry></c:ImplicitGeometry></b:lod1ImplicitRepresentation>"#;
    let ds = read(&model("3.0", body, "EPSG:32632"), "tree.gml").unwrap();
    let Geometry::Point(c) = ds.layers[0].features[0].geometry.as_ref().unwrap() else { panic!() };
    assert_eq!((c.x, c.y, c.z), (112.0, 226.0, Some(342.0)));
}

#[test]
fn nested_city_objects_retain_parent_and_nonspatial_container() {
    let body = r#"<b:consistsOfBuildingPart><b:BuildingPart g:id="part"><b:lod1MultiSurface><g:MultiSurface><g:surfaceMember><g:Polygon><g:exterior><g:LinearRing><g:posList>1 2 3 1 3 3 1 3 4 1 2 3</g:posList></g:LinearRing></g:exterior></g:Polygon></g:surfaceMember></g:MultiSurface></b:lod1MultiSurface></b:BuildingPart></b:consistsOfBuildingPart>"#;
    let ds = read(&model("2.0", body, "EPSG:32632"), "parts.gml").unwrap();
    let features = &ds.layers[0].features;
    assert_eq!(features.len(), 2);
    let part = features.iter().find(|f| f.geometry.is_some()).unwrap();
    assert_eq!(part.attributes[0], FieldValue::Text("part".into()));
    assert_eq!(part.attributes[3], FieldValue::Text("[\"house\"]".into()));
    assert!(features.iter().any(|f| f.geometry.is_none() && f.attributes[0] == FieldValue::Text("house".into())));
}

#[test]
fn geographic_authority_axes_are_normalized_and_compound_crs_is_retained() {
    let body = "<b:lod1Geometry><g:Point><g:pos>52 4 30</g:pos></g:Point></b:lod1Geometry>";
    let ds = read(&model("3.0", body, "urn:ogc:def:crs:EPSG::4326"), "point.gml").unwrap();
    let Geometry::Point(c) = ds.layers[0].features[0].geometry.as_ref().unwrap() else { panic!() };
    assert_eq!((c.x, c.y, c.z), (4.0, 52.0, Some(30.0)));
    let ds = read(&model("2.0", body, "urn:ogc:def:crs,crs:EPSG:6.12:3068,crs:EPSG:6.12:5783"), "point.gml").unwrap();
    let wkt = ds.layers[0].crs_wkt().unwrap();
    assert!(wkt.contains("5783"), "{wkt}");
}

#[test]
fn malformed_xml_links_coordinates_and_namespaces_fail_explicitly() {
    let base = model("3.0", &format!("<b:lod2MultiSurface>{FACE}</b:lod2MultiSurface>"), "EPSG:32632");
    for text in [
        base.replace("10 0 0 10 10 0", "NaN 0 0 10 10 0"),
        base.replace("</c:CityModel>", ""),
        base.replace("</b:Building>", "</b:BuildingPart>"),
        base.replace("http://www.opengis.net/citygml/3.0", "urn:fake"),
        model("3.0", r##"<b:lod2MultiSurface l:href="#missing"/>"##, "EPSG:32632"),
        model("3.0", r#"<b:lod2MultiSurface l:href="https://example.invalid/model.gml#face"/>"#, "EPSG:32632"),
        model("3.0", &format!("<b:lod2Geometry>{FACE}{FACE}</b:lod2Geometry>"), "EPSG:32632"),
    ] {
        assert!(read(&text, "broken.gml").is_err(), "accepted {}", &text[..text.len().min(200)]);
    }
}

#[test]
fn real_citygml_examples_open_without_flattening_away_heights() {
    let Ok(root) = std::env::var("TERRIO_TEST_DATA") else { return };
    for name in ["lod3_building_v2.gml", "lod2_buildings_v3.gml"] {
        let path = std::path::Path::new(&root).join("citymodels").join(name);
        let ds = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(ds.driver, VectorDriver::CityGML);
        assert!(ds.layers[0].features.len() > 500);
        assert!(
            ds.layers[0]
                .features
                .iter()
                .filter_map(|f| f.geometry.as_ref())
                .any(|g| matches!(g,Geometry::Polygon{exterior,..} if exterior.coords().iter().any(|c|c.z.is_some_and(|z|z!=0.0))))
        );
    }
}

#[test]
fn openings_do_not_inherit_their_wall_class_and_curves_are_not_dropped() {
    let window = FACE.replace("wall", "window-face");
    let body = format!(
        "<b:boundedBy><b:WallSurface><b:lod2MultiSurface>{FACE}</b:lod2MultiSurface><b:opening><b:Window g:id=\"window\"><b:lod2MultiSurface>{window}</b:lod2MultiSurface></b:Window></b:opening></b:WallSurface></b:boundedBy>"
    );
    let ds = read(&model("2.0", &body, "EPSG:32632"), "house.gml").unwrap();
    let opening = ds.layers[0].features.iter().find(|f| f.attributes[0] == FieldValue::Text("window".into())).unwrap();
    assert_eq!(opening.attributes[7], FieldValue::Null);
    assert_eq!(opening.attributes[3], FieldValue::Text("[\"house\"]".into()));
    let body = "<b:lod2MultiCurve><g:Curve><g:segments><g:GeodesicString><g:posList>1 2 3 4 5 6</g:posList></g:GeodesicString></g:segments></g:Curve></b:lod2MultiCurve>";
    assert!(read(&model("3.0", body, "EPSG:32632"), "curve.gml").unwrap_err().to_string().contains("nonlinear"));
}

#[test]
fn nonspatial_models_keep_crs_and_generate_distinct_ids() {
    let body = "<b:consistsOfBuildingPart><b:BuildingPart/></b:consistsOfBuildingPart><b:consistsOfBuildingPart><b:BuildingPart/></b:consistsOfBuildingPart>";
    let ds = read(&model("3.0", body, "EPSG:32632"), "empty.gml").unwrap();
    let layer = &ds.layers[0];
    assert_eq!(layer.crs_epsg(), Some(32632));
    assert_eq!(layer.features.len(), 3);
    let ids: std::collections::HashSet<_> = layer
        .features
        .iter()
        .map(|f| match &f.attributes[0] {
            FieldValue::Text(s) => s,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(layer.features.iter().all(|f| f.geometry.is_none()));
}

#[test]
fn mixed_unknown_and_known_crs_are_rejected_in_both_orders() {
    let unknown = "<b:lod1Geometry><g:Point><g:pos>1 2 3</g:pos></g:Point></b:lod1Geometry>";
    let known = "<b:lod1Geometry><g:Point srsName=\"EPSG:32631\"><g:pos>1 2 3</g:pos></g:Point></b:lod1Geometry>";
    for body in [format!("{unknown}{known}"), format!("{known}{unknown}")] {
        let text = model("3.0", &body, "EPSG:32632").replace("srsName=\"EPSG:32632\"", "");
        assert!(read(&text, "mixed.gml").unwrap_err().to_string().contains("without CRS mixed"));
    }
}
