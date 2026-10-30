//! The adapter on the engine's frozen synthetic meshes (see tests/data/hecras/README.md): what is
//! translated, and that every engine refusal reaches the caller as an error.
#![cfg(feature = "hecras")]

use std::path::PathBuf;
use terrio::hecras::{GeometryPolicy, HecRasHdf, MESH_LAYERS, MeshOptions, OmissionKind};
use terrio::{FieldValue, Geometry};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/hecras").join(name)
}

fn open(name: &str) -> HecRasHdf {
    HecRasHdf::open_with_threads(fixture(name), 1).unwrap()
}

#[test]
fn engine_contract_is_api_10_or_later() {
    assert!(terrio::hecras::engine_api_version() >= terrio::hecras::REQUIRED_ENGINE_API);
}

#[test]
fn good_mesh_becomes_polygons_with_identities_and_omissions() {
    let hdf = open("coverage_mesh.h5");
    // No `mesh_perimeters`: not every mesh of the fixture stores a Perimeter.
    assert_eq!(hdf.layer_names().unwrap(), MESH_LAYERS[..4].iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let cells = hdf.mesh_cells(&MeshOptions::default().with_meshes(["Good"])).unwrap();
    let layer = &cells.layer;
    assert_eq!(layer.name, "mesh_cells");
    let fields: Vec<_> = layer.schema.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fields, ["mesh_name", "cell_id"]);
    assert_eq!(layer.len(), 2);
    let ids: Vec<_> = layer.features.iter().map(|f| f.attributes.clone()).collect();
    assert_eq!(ids[0], vec![FieldValue::Text("Good".into()), FieldValue::Integer(0)]);
    assert_eq!(ids[1], vec![FieldValue::Text("Good".into()), FieldValue::Integer(1)]);
    // The engine's API 10 ring for cell 0 is [[1,0],[0,0],[0,1],[1,1],[1,0]] (clockwise from
    // the first listed face); terrio rings drop the closing vertex.
    let Some(Geometry::Polygon { exterior, interiors }) = &layer.features[0].geometry else { panic!("not a polygon") };
    let xy: Vec<_> = exterior.coords().iter().map(|c| (c.x, c.y)).collect();
    assert_eq!(xy, [(1., 0.), (0., 0.), (0., 1.), (1., 1.)]);
    assert!(exterior.signed_area() < 0.0, "clockwise as the engine emits it");
    assert!(interiors.is_empty());
    // The one-face (ghost-like) cell is reported, not drawn.
    assert_eq!(cells.omitted.len(), 1);
    assert_eq!((cells.omitted[0].mesh_name.as_str(), cells.omitted[0].cell_id), ("Good", 2));
    assert_eq!(cells.omitted[0].kind, OmissionKind::InsufficientFaces);
    // The fixture stores no Projection: no CRS, and none guessed.
    assert_eq!(hdf.crs().unwrap(), None);
    assert!(layer.crs.is_none());
}

#[test]
fn cell_ids_repeat_across_meshes_and_stay_paired_with_mesh_names() {
    let hdf = open("coverage_mesh.h5");
    let cells = hdf.mesh_cells(&MeshOptions::default().with_meshes(["Good", "F32Coords"])).unwrap();
    let keys: Vec<_> = cells.layer.features.iter().map(|f| (f.attributes[0].clone(), f.attributes[1].clone())).collect();
    assert_eq!(keys.len(), 4);
    assert_eq!(keys[0].1, keys[2].1, "cell 0 in both meshes");
    assert_ne!(keys[0].0, keys[2].0);
    let fids: Vec<_> = cells.layer.features.iter().map(|f| f.fid).collect();
    assert_eq!(fids, [0, 1, 2, 3]);
}

#[test]
fn corrupt_meshes_fail_the_read_instead_of_giving_fewer_cells() {
    let hdf = open("coverage_mesh.h5");
    // Reading every mesh meets the fixture's broken ones: the whole read fails.
    assert!(matches!(hdf.mesh_cells(&MeshOptions::default()), Err(terrio::Error::InvalidData(_))));
    for (mesh, needle) in [("NaNCoord", "nonfinite"), ("FaceIndexOutOfRange", "out of range")] {
        let e = hdf.mesh_cells(&MeshOptions::default().with_meshes([mesh])).unwrap_err();
        assert!(
            matches!(&e, terrio::Error::InvalidData(m) if m.starts_with("HEC-RAS structural_topology") && m.contains(needle)),
            "{mesh}: {e}"
        );
    }
    // Integer coordinates are refused by the engine, never coerced.
    let e = hdf.mesh_cells(&MeshOptions::default().with_meshes(["IntCoords"])).unwrap_err();
    assert!(matches!(&e, terrio::Error::InvalidData(m) if m.contains("coordinates must be floats")), "{e}");
}

#[test]
fn missing_and_unsupported_layouts_are_errors() {
    let hdf = open("coverage_mesh.h5");
    let e = hdf.mesh_cells(&MeshOptions::default().with_meshes(["NoSuchMesh"])).unwrap_err();
    assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.starts_with("HEC-RAS not_found")), "{e}");
    let e = hdf.mesh_cells(&MeshOptions::default().with_meshes(["MissingValues"])).unwrap_err();
    assert!(matches!(e, terrio::Error::InvalidArgument(_)), "a mesh missing a dataset is not an empty mesh");

    // A legacy names-only layout is Unsupported, not an empty file: its mesh layers are listed
    // (the file stores 2D flow areas), and reading any of them is the engine's refusal.
    let legacy = open("coverage_mesh_nogroups.h5");
    assert!(matches!(legacy.mesh_names(), Err(terrio::Error::Unsupported(_))));
    let listed = legacy.layer_names().unwrap();
    assert!(listed.iter().any(|l| l == "mesh_cells"), "{listed:?}");
    for layer in &listed {
        assert!(matches!(legacy.read_layer(layer), Err(terrio::Error::Unsupported(_))), "{layer}");
    }
    assert!(matches!(legacy.mesh_cells(&MeshOptions::default()), Err(terrio::Error::Unsupported(_))));

    // No 2D Flow Areas group: no mesh layer is listed, and asking for it is an error.
    let none = open("coverage_mesh_nogeometry.h5");
    assert!(none.layer_names().unwrap().is_empty());
    assert!(matches!(none.mesh_cells(&MeshOptions::default()), Err(terrio::Error::InvalidArgument(_))));
}

#[test]
fn byte_budgets_are_refusals() {
    let hdf = open("coverage_mesh.h5");
    let mut options = MeshOptions::default().with_meshes(["Good"]);
    options.max_bytes = 16;
    let e = hdf.mesh_cells(&options).unwrap_err();
    assert!(matches!(&e, terrio::Error::Unsupported(m) if m.contains("resource_limit")), "{e}");
}

#[test]
fn quarantine_is_explicit() {
    let hdf = open("coverage_mesh.h5");
    let strict = hdf.mesh_cells(&MeshOptions::default().with_meshes(["Good"])).unwrap();
    let quarantine = hdf.mesh_cells(&MeshOptions::default().with_meshes(["Good"]).with_policy(GeometryPolicy::QuarantineInvalid)).unwrap();
    let parts = |l: &terrio::Layer| l.features.iter().map(|f| (f.geometry.clone(), f.attributes.clone())).collect::<Vec<_>>();
    assert_eq!(parts(&strict.layer), parts(&quarantine.layer));
    assert_eq!(strict.omitted, quarantine.omitted);
}

#[test]
fn summaries_from_a_file_without_results_are_errors() {
    let hdf = open("coverage_mesh.h5");
    let e = hdf.mesh_cells(&MeshOptions::default().with_meshes(["Good"]).with_summary("Maximum Water Surface")).unwrap_err();
    assert!(matches!(e, terrio::Error::InvalidArgument(_) | terrio::Error::Unsupported(_)), "{e}");
}

#[test]
fn missing_file_is_an_io_not_found_error() {
    let Err(e) = HecRasHdf::open(fixture("does_not_exist.hdf")) else { panic!("opened a missing file") };
    assert!(matches!(&e, terrio::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound), "{e}");
}

#[test]
fn the_layer_round_trips_through_terrio_drivers() {
    let hdf = open("coverage_mesh.h5");
    let cells = hdf.mesh_cells(&MeshOptions::default().with_meshes(["Good"])).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.geojson");
    let path = path.to_str().unwrap();
    terrio::write_vector(std::slice::from_ref(&cells.layer), path, &Default::default()).unwrap();
    let back = terrio::open_vector(path, &Default::default()).unwrap();
    let layer = &back.layers[0];
    assert_eq!(layer.len(), 2);
    let names: Vec<_> = layer.schema.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["mesh_name", "cell_id"]);
    assert_eq!(layer.features[1].attributes, vec![FieldValue::Text("Good".into()), FieldValue::Integer(1)]);
}

#[test]
fn faces_are_the_engine_polylines_with_mesh_and_face_ids() {
    let hdf = open("coverage_mesh.h5");
    let faces = hdf.mesh_faces(&MeshOptions::default().with_meshes(["Good"])).unwrap();
    let layer = &faces.layer;
    assert_eq!(layer.name, "mesh_faces");
    let fields: Vec<_> = layer.schema.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fields, ["mesh_name", "face_id"]);
    assert!(!layer.is_empty());
    for (i, f) in layer.features.iter().enumerate() {
        assert_eq!(f.attributes, vec![FieldValue::Text("Good".into()), FieldValue::Integer(i as i64)]);
        let Some(Geometry::LineString(line)) = &f.geometry else { panic!("not a line") };
        assert!(line.len() >= 2);
    }
    // Cell polygons and face lines of one mesh share their vertices.
    let cells = hdf.mesh_cells(&MeshOptions::default().with_meshes(["Good"])).unwrap();
    let Some(Geometry::Polygon { exterior, .. }) = &cells.layer.features[0].geometry else { panic!() };
    let vertex = (exterior.coords()[0].x, exterior.coords()[0].y);
    assert!(layer.features.iter().any(|f| matches!(&f.geometry, Some(Geometry::LineString(l)) if l.iter().any(|c| (c.x, c.y) == vertex))));
}

#[test]
fn absent_perimeters_and_unprovable_ghost_flags_are_errors() {
    let hdf = open("coverage_mesh.h5");
    // The synthetic meshes store no Perimeter dataset.
    let e = hdf.mesh_perimeters(&MeshOptions::default().with_meshes(["Good"])).unwrap_err();
    assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.starts_with("HEC-RAS not_found")), "{e}");
    // Ghost flags need the physical cell count of a 2D Flow Areas Attributes table.
    let e = hdf.mesh_cell_points(&MeshOptions::default().with_meshes(["Good"])).unwrap_err();
    assert!(!matches!(e, terrio::Error::Io(_)), "{e}");
}

#[test]
fn family_layers_absent_from_a_file_are_errors_and_not_listed() {
    let hdf = open("coverage_mesh.h5");
    let names = hdf.layer_names().unwrap();
    for family in terrio::hecras::family_layer_names() {
        assert!(!names.iter().any(|n| n == family), "{family}");
        let e = hdf.read_layer(family).unwrap_err();
        assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.starts_with("HEC-RAS unavailable")), "{family}: {e}");
    }
    assert!(matches!(hdf.read_layer("no_such_layer"), Err(terrio::Error::InvalidArgument(_))));
    assert_eq!(hdf.read_layer("mesh_faces").map(|l| l.name).ok(), None, "every mesh is read, and the fixture has broken ones");
}

#[test]
fn summaries_are_refused_on_perimeters() {
    let hdf = open("coverage_mesh.h5");
    let e = hdf.mesh_perimeters(&MeshOptions::default().with_meshes(["Good"]).with_summary("Maximum Water Surface")).unwrap_err();
    assert!(matches!(e, terrio::Error::InvalidArgument(_)));
}
