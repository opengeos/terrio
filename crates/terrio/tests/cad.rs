//! CAD interchange regressions, exercising both acadrust-generated documents and
//! Terrio's public file/byte APIs. External GDAL comparisons live in scripts/.
#![cfg(feature = "cad")]

use acadrust::entities::{Circle, EntityType, Insert, Line, LwPolyline, LwVertex, Point, Text};
use acadrust::tables::BlockRecord;
use acadrust::types::{Vector2, Vector3};
use acadrust::{CadDocument, DwgWriter, DxfWriter};
use terrio::formats::VectorDriver;
use terrio::memory::{open_vector_bytes, write_vector_bytes};
use terrio::vector::{Coord, Ring};
use terrio::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};

fn read(bytes: Vec<u8>, ext: &str) -> terrio::VectorDataset {
    open_vector_bytes(bytes, &format!("drawing.{ext}"), &Default::default()).unwrap()
}

fn property<'a>(layer: &'a Layer, i: usize, name: &str) -> &'a FieldValue {
    &layer.features[i].attributes[layer.schema.field_index(name).unwrap()]
}

fn fixture() -> Layer {
    let mut layer = Layer::new("survey");
    layer.add_field(FieldDef::new("Text", FieldType::Text));
    layer.add_field(FieldDef::new("PaperSpace", FieldType::Integer));
    layer.add_field(FieldDef::new("Linetype", FieldType::Text));
    let geoms = [
        Geometry::Point(Coord::xyz(12.5, -7.25, 18.0)),
        Geometry::LineString(vec![Coord::xyz(1.0, 2.0, 3.0), Coord::xyz(4.0, 8.0, 12.0)]),
        Geometry::LineString(vec![Coord::xyz(0.0, 1.0, 2.0), Coord::xyz(3.0, 4.0, 5.0), Coord::xyz(6.0, 7.0, 8.0)]),
        Geometry::Polygon {
            exterior: Ring::new(vec![
                Coord::xyz(0.0, 0.0, 2.0),
                Coord::xyz(10.0, 0.0, 2.0),
                Coord::xyz(10.0, 10.0, 2.0),
                Coord::xyz(0.0, 10.0, 2.0),
            ]),
            interiors: vec![Ring::new(vec![
                Coord::xyz(2.0, 2.0, 2.0),
                Coord::xyz(4.0, 2.0, 2.0),
                Coord::xyz(4.0, 4.0, 2.0),
                Coord::xyz(2.0, 4.0, 2.0),
            ])],
        },
    ];
    for (i, geometry) in geoms.into_iter().enumerate() {
        layer.features.push(Feature {
            fid: i as u64,
            geometry: Some(geometry),
            attributes: vec![
                if i == 0 { FieldValue::Text("Elevation 日本語".into()) } else { FieldValue::Null },
                if i == 0 { FieldValue::Integer(1) } else { FieldValue::Null },
                if i == 1 { FieldValue::Text("DASHED".into()) } else { FieldValue::Null },
            ],
        });
    }
    layer
}

#[test]
fn cad_roundtrip_bytes_files_and_cross_format() {
    for ext in ["dxf", "dwg"] {
        let original = fixture();
        let bytes = write_vector_bytes(std::slice::from_ref(&original), &format!("drawing.{ext}"), &Default::default()).unwrap();
        if ext == "dwg" {
            assert!(bytes.starts_with(b"AC1032"));
        }
        let ds = read(bytes.clone(), ext);
        assert_eq!(ds.driver, if ext == "dwg" { VectorDriver::DWG } else { VectorDriver::DXF });
        let layer = &ds.layers[0];
        assert_eq!(layer.name, "entities");
        assert_eq!(layer.crs_epsg(), None);
        assert_eq!(layer.features.len(), 4);
        // Paper-space text is still represented, with the original position and UTF-8 label.
        let text_index = layer.features.iter().position(|f| f.geometry == original.features[0].geometry).unwrap();
        assert_eq!(property(layer, text_index, "Text"), &FieldValue::Text("Elevation 日本語".into()));
        assert_eq!(property(layer, text_index, "PaperSpace"), &FieldValue::Integer(1));
        for feature in &original.features {
            assert!(layer.features.iter().any(|f| f.geometry == feature.geometry), "{ext}: missing {:?}", feature.geometry);
        }
        assert!((0..4).all(|i| property(layer, i, "Layer") == &FieldValue::Text("survey".into())));
        let line_index = layer.features.iter().position(|f| f.geometry == original.features[1].geometry).unwrap();
        assert_eq!(property(layer, line_index, "Linetype"), &FieldValue::Text("DASHED".into()));
        assert!(matches!(property(layer, line_index, "EntityHandle"), FieldValue::Text(h) if !h.starts_with("0x")));
        let other = if ext == "dwg" { "dxf" } else { "dwg" };
        let converted = write_vector_bytes(&ds.layers, &format!("cross.{other}"), &Default::default()).unwrap();
        assert_eq!(read(converted, other).layers[0].features.len(), 4);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("drawing.{ext}"));
        terrio::write_vector(&[original], path.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers[0].features.len(), 4);
    }
}

#[test]
fn cad_reads_ascii_binary_dxf_and_dwg_curves_and_ocs() {
    let mut doc = CadDocument::new();
    let mut p = LwPolyline::new();
    p.elevation = 5.0;
    p.normal = Vector3::new(0.0, 1.0, 0.0);
    p.add_vertex(LwVertex::with_bulge(Vector2::new(0.0, 0.0), 1.0));
    p.add_vertex(LwVertex::from_coords(2.0, 0.0));
    doc.add_entity(EntityType::LwPolyline(p)).unwrap();
    doc.add_entity(EntityType::Circle(Circle::from_coords(4.0, 5.0, 6.0, 2.0))).unwrap();
    let encodings = [
        (DxfWriter::new(&doc).write_to_vec().unwrap(), "dxf"),
        (DxfWriter::new_binary(&doc).write_to_vec().unwrap(), "dxf"),
        (DwgWriter::write_to_vec(&doc).unwrap(), "dwg"),
    ];
    for (bytes, ext) in encodings {
        let ds = read(bytes, ext);
        assert_eq!(ds.layers[0].features.len(), 2);
        let Geometry::LineString(cs) = ds.layers[0].features[0].geometry.as_ref().unwrap() else { panic!() };
        assert_eq!(cs.first().unwrap(), &Coord::xyz(0.0, 5.0, 0.0));
        assert_eq!(cs.last().unwrap(), &Coord::xyz(-2.0, 5.0, 0.0));
        assert!(cs.len() >= 46);
        assert!(cs.iter().all(|c| (c.y - 5.0).abs() < 1e-12));
        assert!(cs.iter().map(|c| c.z.unwrap()).fold(0.0, f64::min) < -0.99);
        let Geometry::LineString(cs) = ds.layers[0].features[1].geometry.as_ref().unwrap() else { panic!() };
        assert_eq!(cs.len(), 91);
        assert_eq!(cs.first(), cs.last());
    }
}

#[test]
fn cad_multiple_layers_multipart_and_empty_drawing() {
    let mut a = Layer::new("first");
    a.features.push(Feature::with_geometry(0, Geometry::MultiPoint(vec![Coord::xyz(1.0, 2.0, 3.0), Coord::xyz(4.0, 5.0, 6.0)]), 0));
    let mut b = Layer::new("second");
    b.features.push(Feature::with_geometry(0, Geometry::Point(Coord::xyz(7.0, 8.0, 9.0)), 0));
    for ext in ["dxf", "dwg"] {
        let ds = read(write_vector_bytes(&[a.clone(), b.clone()], &format!("x.{ext}"), &Default::default()).unwrap(), ext);
        assert_eq!(ds.layers[0].features.len(), 3);
        assert_eq!(property(&ds.layers[0], 2, "Layer"), &FieldValue::Text("second".into()));
        let ds = read(write_vector_bytes(&[Layer::new("empty")], &format!("x.{ext}"), &Default::default()).unwrap(), ext);
        assert_eq!(ds.layers.len(), 1);
        assert!(ds.layers[0].features.is_empty());
    }
}

#[test]
fn cad_rejects_unsupported_or_invalid_data_without_writing_output() {
    assert!(open_vector_bytes(b"not CAD".to_vec(), "broken.dwg", &Default::default()).is_err());
    assert!(open_vector_bytes(b"not CAD".to_vec(), "broken.dxf", &Default::default()).is_err());
    let mut doc = CadDocument::new();
    doc.add_entity(EntityType::Spline(Default::default())).unwrap();
    let err = open_vector_bytes(DxfWriter::new(&doc).write_to_vec().unwrap(), "spline.dxf", &Default::default()).unwrap_err();
    assert!(err.to_string().contains("unsupported SPLINE"), "{err}");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.dxf");
    let mut layer = fixture();
    layer.features[3].geometry = Some(Geometry::Polygon {
        exterior: Ring::new(vec![Coord::xyz(0.0, 0.0, 1.0), Coord::xyz(1.0, 0.0, 2.0), Coord::xyz(1.0, 1.0, 3.0)]),
        interiors: vec![],
    });
    let error = terrio::write_vector(&[layer], path.to_str().unwrap(), &Default::default()).unwrap_err();
    assert!(error.to_string().contains("Z elevation"));
    assert!(!path.exists());
}

#[test]
fn cad_inserts_expand_without_exporting_unused_block_definitions() {
    let mut doc = CadDocument::new();
    let mut block = BlockRecord::new("test");
    block.handle = doc.allocate_handle();
    let owner = block.handle;
    doc.block_records.add(block).unwrap();
    let mut line = Line::from_coords(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
    line.common.owner_handle = owner;
    doc.add_entity(EntityType::Line(line)).unwrap();
    doc.add_entity(EntityType::Insert(Insert::new("test", Vector3::new(10.0, 20.0, 30.0)))).unwrap();
    doc.add_entity(EntityType::Point(Point::from_coords(0.0, 0.0, 0.0))).unwrap();
    doc.add_entity(EntityType::Text(Text::with_value("hello", Vector3::ZERO))).unwrap();
    for (bytes, ext) in [(DxfWriter::new(&doc).write_to_vec().unwrap(), "dxf"), (DwgWriter::write_to_vec(&doc).unwrap(), "dwg")] {
        let ds = read(bytes, ext);
        assert_eq!(ds.layers[0].features.len(), 3);
        assert!(
            ds.layers[0]
                .features
                .iter()
                .any(|f| f.geometry == Some(Geometry::LineString(vec![Coord::xyz(11.0, 22.0, 33.0), Coord::xyz(14.0, 25.0, 36.0)])))
        );
    }
}

#[test]
fn cad_hatch_external_shells_and_tilted_holes() {
    use acadrust::entities::hatch::{BoundaryEdge, BoundaryPath, Hatch, PolylineEdge};
    let mut h = Hatch::solid().with_elevation(5.0).with_normal(Vector3::new(0.0, 1.0, 0.0));
    for (external, min, max) in [(true, 0.0, 10.0), (true, 2.0, 4.0), (false, 6.0, 8.0)] {
        let mut path = if external { BoundaryPath::external() } else { BoundaryPath::new() };
        path.flags.set_polyline(true);
        path.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(
            vec![Vector2::new(min, min), Vector2::new(max, min), Vector2::new(max, max), Vector2::new(min, max)],
            true,
        )));
        h.add_path(path);
    }
    let mut doc = CadDocument::new();
    doc.add_entity(EntityType::Hatch(Box::new(h))).unwrap();
    for (bytes, ext) in [(DxfWriter::new(&doc).write_to_vec().unwrap(), "dxf"), (DwgWriter::write_to_vec(&doc).unwrap(), "dwg")] {
        let ds = read(bytes, ext);
        let Geometry::MultiPolygon(ps) = ds.layers[0].features[0].geometry.as_ref().unwrap() else { panic!() };
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].1.len(), 1);
        assert!(ps[1].1.is_empty());
        assert!(ps.iter().flat_map(|(e, hs)| std::iter::once(e).chain(hs)).flat_map(|r| &r.0).all(|c| (c.y - 5.0).abs() < 1e-12));
    }
}
