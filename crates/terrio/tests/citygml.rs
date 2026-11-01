// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityGML encoding regressions. GDAL 3.13.3 is an informational cross-reader.
use terrio::{VectorDataset, formats::VectorDriver};
use wbvector::{FieldValue, Geometry};

#[path = "support/citygml.rs"]
mod data;
use data::{FACE, model};

fn read(text: &str, name: &str) -> terrio::Result<VectorDataset> {
    terrio::memory::open_vector_bytes(text.as_bytes().to_vec(), name, &Default::default())
}

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
fn real_citygml_examples_open_without_flattening_away_heights() {
    let Ok(root) = std::env::var("TERRIO_TEST_DATA") else { return };
    for name in ["lod3_building_v2.gml", "lod2_buildings_v3.gml"] {
        let path = std::path::Path::new(&root).join("citymodels").join(name);
        if !path.is_file() {
            eprintln!("optional CityGML fixture is absent: {}", path.display());
            continue;
        }
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
fn openings_do_not_inherit_their_wall_class() {
    let window = FACE.replace("wall", "window-face");
    let body = format!(
        "<b:boundedBy><b:WallSurface><b:lod2MultiSurface>{FACE}</b:lod2MultiSurface><b:opening><b:Window g:id=\"window\"><b:lod2MultiSurface>{window}</b:lod2MultiSurface></b:Window></b:opening></b:WallSurface></b:boundedBy>"
    );
    let ds = read(&model("2.0", &body, "EPSG:32632"), "house.gml").unwrap();
    let opening = ds.layers[0].features.iter().find(|f| f.attributes[0] == FieldValue::Text("window".into())).unwrap();
    assert_eq!(opening.attributes[7], FieldValue::Null);
    assert_eq!(opening.attributes[3], FieldValue::Text("[\"house\"]".into()));
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
fn repeated_object_properties_keep_every_value_and_code_space() {
    let body = r#"<b:function codeSpace="codes">office</b:function><b:function codeSpace="codes">retail</b:function><b:usage>work</b:usage><b:usage>shop</b:usage>"#;
    let ds = read(&model("3.0", body, "EPSG:32632"), "functions.gml").unwrap();
    let FieldValue::Text(attrs) = &ds.layers[0].features[0].attributes[4] else { panic!() };
    let attrs: serde_json::Value = serde_json::from_str(attrs).unwrap();
    assert_eq!(attrs["function"], serde_json::json!([{"value":"office","codeSpace":"codes"},{"value":"retail","codeSpace":"codes"}]));
    assert_eq!(attrs["usage"], serde_json::json!(["work", "shop"]));
}

#[test]
fn linked_objects_share_records_and_accumulate_all_parents() {
    let xml = r##"<c:CityModel xmlns:c="http://www.opengis.net/citygml/3.0" xmlns:g="http://www.opengis.net/gml/3.2" xmlns:b="http://www.opengis.net/citygml/building/3.0" xmlns:l="http://www.w3.org/1999/xlink"><c:cityObjectMember><b:Building g:id="a"><b:consistsOfBuildingPart l:href="#part"/></b:Building></c:cityObjectMember><c:cityObjectMember><b:Building g:id="b"><b:consistsOfBuildingPart l:href="#part"/></b:Building></c:cityObjectMember><c:cityObjectMember><b:BuildingPart g:id="part"/></c:cityObjectMember></c:CityModel>"##;
    for geometry in [String::new(), format!("<b:lod2MultiSurface>{FACE}</b:lod2MultiSurface>")] {
        let xml = xml.replace("<b:BuildingPart g:id=\"part\"/>", &format!("<b:BuildingPart g:id=\"part\">{geometry}</b:BuildingPart>"));
        let ds = read(&xml, "references.gml").unwrap();
        let parts: Vec<_> = ds.layers[0].features.iter().filter(|f| f.attributes[0] == FieldValue::Text("part".into())).collect();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].attributes[3], FieldValue::Text("[\"a\",\"b\"]".into()));
        assert_eq!(ds.layers[0].features.len(), 3);
    }
}

#[test]
fn geometry_axis_spellings_are_normalized_before_combining() {
    let body = r#"<b:lod1Geometry><g:MultiPoint><g:pointMember><g:Point srsName="urn:ogc:def:crs:EPSG::4326"><g:pos>52 4 30</g:pos></g:Point></g:pointMember><g:pointMember><g:Point srsName="EPSG:4326"><g:pos>4 52 30</g:pos></g:Point></g:pointMember></g:MultiPoint></b:lod1Geometry>"#;
    let ds = read(&model("3.0", body, "EPSG:4326"), "axes.gml").unwrap();
    for f in &ds.layers[0].features {
        let Geometry::Point(c) = f.geometry.as_ref().unwrap() else { panic!() };
        assert_eq!((c.x, c.y, c.z), (4.0, 52.0, Some(30.0)));
    }
}

#[test]
fn constructive_elements_and_building_subdivisions_keep_their_own_identity() {
    for class in ["BuildingConstructiveElement", "BuildingUnit", "Storey"] {
        let body = format!(
            r#"<b:buildingSubdivision><b:{class} g:id="child"><g:name>member</g:name><b:lod2MultiSurface>{FACE}</b:lod2MultiSurface></b:{class}></b:buildingSubdivision>"#
        );
        let ds = read(&model("3.0", &body, "EPSG:32632"), "members.gml").unwrap();
        let child = ds.layers[0].features.iter().find(|f| f.attributes[0] == FieldValue::Text("child".into())).unwrap();
        assert_eq!(child.attributes[1], FieldValue::Text(class.into()));
        assert_eq!(child.attributes[3], FieldValue::Text("[\"house\"]".into()));
        assert!(child.geometry.is_some());
    }
}

#[test]
fn utf16_and_latin1_documents_keep_unicode_attributes() {
    let text = model("3.0", "<g:name>Montréal</g:name>", "EPSG:32632");
    for little in [true, false] {
        let mut bytes = if little { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };
        for value in text.encode_utf16() {
            bytes.extend(if little { value.to_le_bytes() } else { value.to_be_bytes() });
        }
        let ds = terrio::memory::open_vector_bytes(bytes.clone(), "model.gml", &Default::default()).unwrap();
        assert_eq!(ds.driver, VectorDriver::CityGML);
        assert_eq!(terrio::inspection::identify_bytes(&bytes, "model.gml").unwrap().driver, "CityGML");
        let FieldValue::Text(attrs) = &ds.layers[0].features[0].attributes[4] else { panic!() };
        assert_eq!(serde_json::from_str::<serde_json::Value>(attrs).unwrap()["name"], "Montréal");
    }
    let bytes: Vec<u8> = text.chars().map(|c| u8::try_from(c as u32).unwrap()).collect();
    let ds = terrio::memory::open_vector_bytes(bytes, "model.gml", &Default::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::CityGML);
    let FieldValue::Text(attrs) = &ds.layers[0].features[0].attributes[4] else { panic!() };
    assert_eq!(serde_json::from_str::<serde_json::Value>(attrs).unwrap()["name"], "Montréal");
}

#[test]
fn reverse_surface_references_keep_orientation_and_remove_adjacent_duplicates() {
    let face = FACE.replace("10 0 0 10 10 0", "10 0 0 10 0 0 10 10 0");
    let body = format!(
        r##"<b:lod2MultiSurface><g:MultiSurface><g:surfaceMember>{face}</g:surfaceMember><g:surfaceMember><g:OrientableSurface orientation="-"><g:baseSurface l:href="#wall"/></g:OrientableSurface></g:surfaceMember></g:MultiSurface></b:lod2MultiSurface>"##
    );
    let ds = read(&model("3.0", &body, "EPSG:32632"), "reversed.gml").unwrap();
    assert_eq!(ds.layers[0].features.len(), 2);
    let rings: Vec<_> = ds.layers[0]
        .features
        .iter()
        .map(|f| match f.geometry.as_ref().unwrap() {
            Geometry::Polygon { exterior, .. } => exterior,
            _ => panic!(),
        })
        .collect();
    assert_eq!(rings[0].len(), 4);
    assert_eq!(rings[1].0, rings[0].0.iter().rev().cloned().collect::<Vec<_>>());
}
