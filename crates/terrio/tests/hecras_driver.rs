//! The HECRAS vector driver through terrio's generic API: detection by content, coexistence with
//! the HDF5 raster driver, layer selection and the open options. Uses the synthetic plan
//! `tests/data/hecras/results_mesh_xs.h5` (see `scripts/make_hecras_fixture.py`).
#![cfg(feature = "hecras")]

use std::path::PathBuf;
use terrio::formats::VectorDriver;
use terrio::{Dataset, FieldValue, HecRasOpenOptions, Layer, VectorOpenOptions};

fn data(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/hecras").join(name).to_string_lossy().into_owned()
}

fn options(pairs: &[(&str, &str)], layers: &[&str]) -> VectorOpenOptions {
    let mut hecras = HecRasOpenOptions::default();
    for (k, v) in pairs {
        assert!(hecras.set(k, v).unwrap(), "{k}");
    }
    VectorOpenOptions { layers: layers.iter().map(|s| s.to_string()).collect(), hecras, ..Default::default() }
}

fn field<'a>(layer: &'a Layer, name: &str) -> Vec<&'a FieldValue> {
    let i = layer.schema.fields().iter().position(|f| f.name == name).unwrap_or_else(|| panic!("no field {name}"));
    layer.features.iter().map(|f| &f.attributes[i]).collect()
}

#[test]
fn hec_ras_plans_open_as_vectors_by_content() {
    let path = data("results_mesh_xs.h5");
    let ds = terrio::open_vector(&path, &VectorOpenOptions::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::HECRAS);
    let names: Vec<_> = ds.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "mesh_cells",
            "mesh_cell_points",
            "mesh_faces",
            "mesh_face_points",
            // No `mesh_perimeters`: the synthetic mesh stores no Perimeter dataset.
            "cross_sections",
            "bc_lines",
            "reference_lines",
            "reference_points",
            "river_junctions"
        ]
    );
    // terrio::open (and so `terrio info`) gives the same layers.
    assert!(matches!(terrio::open(&path).unwrap(), Dataset::Vector(v) if v.driver == VectorDriver::HECRAS));
    // The stored projection text, no EPSG code.
    let crs = ds.layers[0].crs.as_ref().unwrap();
    assert!(crs.wkt.as_deref().unwrap().starts_with("PROJCS[\"Synthetic\""));
    assert_eq!(crs.epsg, None);
}

#[test]
fn raster_opens_keep_reading_hdf5() {
    // A raster open of a HEC-RAS file still goes to the HDF5 raster driver, as before.
    let r = terrio::open_raster(&data("results_mesh_xs.h5"));
    assert!(!matches!(r, Err(terrio::Error::Unsupported(ref m)) if m.contains("HECRAS")), "{r:?}");
    // An HDF5 file without a HEC-RAS File Type is not claimed by the vector driver.
    let other = terrio::open(&data("coverage_mesh.h5"));
    assert!(!matches!(other, Ok(Dataset::Vector(ref v)) if v.driver == VectorDriver::HECRAS));
}

#[test]
fn layer_selection_and_joins_by_open_options() {
    let o = options(
        &[
            ("CELL_SUMMARIES", "Maximum Water Surface"),
            ("CELL_ATTRIBUTES", "MINIMUM_ELEVATION"),
            ("CELL_SERIES", "Water Surface"),
            ("CELL_MAXIMUM", "Water Surface"),
            ("FACE_SERIES", "Face Velocity"),
            ("TIME_STEP", "1"),
        ],
        &["mesh_cells", "mesh_faces"],
    );
    let ds = terrio::open_vector(&data("results_mesh_xs.h5"), &o).unwrap();
    assert_eq!(ds.layers.len(), 2);
    let cells = &ds.layers[0];
    let fields: Vec<_> = cells.schema.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        fields,
        [
            "mesh_name",
            "cell_id",
            "maximum_water_surface",
            "maximum_water_surface_time",
            "minimum_elevation",
            "water_surface",
            "water_surface_max",
            "water_surface_max_row"
        ]
    );
    assert_eq!(field(cells, "water_surface"), [&FieldValue::Float(12.75), &FieldValue::Float(12.25)]);
    assert!(ds.layers[1].schema.fields().iter().any(|f| f.name == "face_velocity"));
    // A layer named twice is read once, in the order first named, as other drivers select.
    let o = options(&[], &["river_junctions", "mesh_cells", "river_junctions"]);
    let names: Vec<_> = terrio::open_vector(&data("results_mesh_xs.h5"), &o).unwrap().layers.into_iter().map(|l| l.name).collect();
    assert_eq!(names, ["river_junctions", "mesh_cells"]);
}

#[test]
fn feature_layers_join_at_the_time_step() {
    let o = options(
        &[("XS_SUMMARIES", "Maximum Water Surface"), ("XS_RESULTS", "Water Surface"), ("BC_SERIES", "YES"), ("TIME_STEP", "2")],
        &["cross_sections", "bc_lines"],
    );
    let ds = terrio::open_vector(&data("results_mesh_xs.h5"), &o).unwrap();
    assert_eq!(field(&ds.layers[0], "water_surface"), [&FieldValue::Float(21.0), &FieldValue::Float(19.0), &FieldValue::Float(15.5)]);
    assert!(ds.layers[0].schema.fields().iter().any(|f| f.name == "maximum_water_surface_flow_at_max_wsel"));
    assert_eq!(field(&ds.layers[1], "stage"), [&FieldValue::Float(12.25), &FieldValue::Float(11.25)]);
}

#[test]
fn misuse_is_an_error() {
    let path = data("results_mesh_xs.h5");
    // A series without TIME_STEP.
    assert!(terrio::open_vector(&path, &options(&[("CELL_SERIES", "Water Surface")], &["mesh_cells"])).is_err());
    // A value the file does not store (no depth in this plan): an error, never computed.
    assert!(terrio::open_vector(&path, &options(&[("CELL_SERIES", "Depth"), ("TIME_STEP", "0")], &["mesh_cells"])).is_err());
    // A layer the file does not store.
    let e = terrio::open_vector(&path, &options(&[], &["structures"])).unwrap_err();
    assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.contains("layers:")), "{e}");
    // ... and one this file stores no perimeter for, though it stores meshes.
    let e = terrio::open_vector(&path, &options(&[], &["mesh_cells", "mesh_perimeters"])).unwrap_err();
    assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.starts_with("no layer named mesh_perimeters")), "{e}");
    // Options for layers not read are ignored.
    assert!(terrio::open_vector(&path, &options(&[("FACE_SUMMARIES", "No Such Variable")], &["mesh_cells"])).is_ok());
}

#[test]
fn the_driver_is_listed_by_name() {
    assert_eq!(VectorDriver::from_name("HECRAS"), Some(VectorDriver::HECRAS));
    assert_eq!(VectorDriver::from_name("hec-ras"), Some(VectorDriver::HECRAS));
    let info = VectorDriver::HECRAS.info();
    assert!(info.read && !info.write);
    assert_eq!(VectorDriver::for_extension("plan.p01.hdf"), None, "no extension claim: content decides");
}

#[test]
fn hec_ras_files_open_from_bytes() {
    let bytes = std::fs::read(data("results_mesh_xs.h5")).unwrap();
    // The engine session from bytes reads what the session from the path reads.
    let from_bytes = terrio::hecras::HecRasHdf::from_bytes(bytes.clone(), "plan.p01.hdf").unwrap();
    let from_path = terrio::hecras::HecRasHdf::open(data("results_mesh_xs.h5")).unwrap();
    assert_eq!(from_bytes.layer_names().unwrap(), from_path.layer_names().unwrap());
    let (a, b) = (from_bytes.read_layer("cross_sections").unwrap(), from_path.read_layer("cross_sections").unwrap());
    assert_eq!(a.len(), b.len());
    for (fa, fb) in a.features.iter().zip(&b.features) {
        assert_eq!((&fa.geometry, &fa.attributes), (&fb.geometry, &fb.attributes));
    }
    // Through terrio's generic in-memory API, with open options.
    let o = options(&[("CELL_SERIES", "Water Surface"), ("TIME_STEP", "1")], &["mesh_cells"]);
    let ds = terrio::memory::open_vector_bytes(bytes.clone(), "plan.p01.hdf", &o).unwrap();
    assert_eq!(ds.driver, VectorDriver::HECRAS);
    assert_eq!(field(&ds.layers[0], "water_surface"), [&FieldValue::Float(12.75), &FieldValue::Float(12.25)]);
    assert!(matches!(terrio::memory::open_bytes(bytes.clone(), "plan.p01.hdf").unwrap(), Dataset::Vector(_)));
    // A raster open from bytes still goes to the HDF5 raster driver.
    let r = terrio::memory::open_raster_bytes(bytes, "plan.p01.hdf");
    assert!(!matches!(r, Err(terrio::Error::Unsupported(ref m)) if m.contains("vector")), "{r:?}");
    // Bytes that are not HDF5 are refused by the engine with the given name.
    let Err(e) = terrio::hecras::HecRasHdf::from_bytes(b"not HDF5".to_vec(), "x.hdf") else { panic!("opened") };
    assert!(matches!(e, terrio::Error::InvalidData(_)), "{e}");
}

#[test]
fn other_hdf5_and_netcdf_files_are_not_claimed() {
    let tests = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    // An HDF5 file without a root File Type, another HDF5 table, a NetCDF-4 file (HDF5 inside),
    // and an HDF5 file whose File Type is HEC's but not a geometry or plan file (`HEC Terrain`).
    for path in [
        tests.join("hecras/coverage_mesh.h5"),
        tests.join("hdf5_v1_compound_table.h5"),
        tests.join("netcdf_bands/axes.nc"),
        tests.join("hecras/terrain.h5"),
    ] {
        let path = path.to_string_lossy().into_owned();
        let claimed = |d: &terrio::Result<Dataset>| matches!(d, Ok(Dataset::Vector(v)) if v.driver == VectorDriver::HECRAS);
        assert!(!claimed(&terrio::open(&path)), "{path}: claimed by HECRAS");
        let vector = terrio::open_vector(&path, &VectorOpenOptions::default());
        assert!(!matches!(&vector, Ok(v) if v.driver == VectorDriver::HECRAS), "{path}: open_vector gave HECRAS");
        let bytes = std::fs::read(&path).unwrap();
        let name = std::path::Path::new(&path).file_name().unwrap().to_string_lossy().into_owned();
        assert!(!claimed(&terrio::memory::open_bytes(bytes.clone(), &name)), "{path}: claimed from bytes");
        let vector = terrio::memory::open_vector_bytes(bytes, &name, &VectorOpenOptions::default());
        assert!(!matches!(&vector, Ok(v) if v.driver == VectorDriver::HECRAS), "{path}: open_vector_bytes gave HECRAS");
    }
    // The NetCDF file keeps opening as before (a raster through the NetCDF driver).
    assert!(matches!(terrio::open(&tests.join("netcdf_bands/axes.nc").to_string_lossy()), Ok(Dataset::Raster(_))));
    // A HEC-RAS geometry file (`File Type` = `HEC-RAS Geometry`) is claimed, from a path and from bytes.
    let geometry = data("pipes.h5");
    assert!(matches!(terrio::open(&geometry), Ok(Dataset::Vector(v)) if v.driver == VectorDriver::HECRAS));
    let bytes = std::fs::read(&geometry).unwrap();
    assert!(matches!(terrio::memory::open_bytes(bytes, "pipes.g01.hdf"), Ok(Dataset::Vector(v)) if v.driver == VectorDriver::HECRAS));
}

#[test]
fn pipe_networks_are_line_and_point_layers() {
    let ds = terrio::open_vector(&data("pipes.h5"), &VectorOpenOptions::default()).unwrap();
    let names: Vec<_> = ds.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["pipe_conduits", "pipe_nodes"]);
    let conduits = &ds.layers[0];
    assert_eq!(conduits.geom_type, Some(terrio::GeometryType::LineString));
    let fields: Vec<_> = conduits.schema.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        fields,
        ["source_row", "name", "system_name", "us_node", "ds_node", "conduit_length", "rise", "manning_s_n", "ds_flap_gate"]
    );
    assert_eq!(field(conduits, "name"), [&FieldValue::Text("C1".into()), &FieldValue::Text("C2".into())]);
    assert_eq!(field(conduits, "conduit_length"), [&FieldValue::Float(120.5), &FieldValue::Float(80.25)]);
    // float32 widened exactly; uint8 as an integer.
    assert_eq!(field(conduits, "manning_s_n"), [&FieldValue::Float(f64::from(0.013f32)), &FieldValue::Float(f64::from(0.015f32))]);
    assert_eq!(field(conduits, "ds_flap_gate"), [&FieldValue::Integer(1), &FieldValue::Integer(0)]);
    let Some(terrio::Geometry::LineString(line)) = &conduits.features[1].geometry else { panic!() };
    assert_eq!(line.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), [(10.0, 0.0), (15.0, 5.0), (20.0, 5.0)]);
    let nodes = &ds.layers[1];
    assert_eq!(nodes.geom_type, Some(terrio::GeometryType::Point));
    assert_eq!(field(nodes, "node_type").len(), 3);
    assert_eq!(field(nodes, "invert_elevation")[2], &FieldValue::Float(34.25));
    let Some(terrio::Geometry::Point(p)) = &nodes.features[2].geometry else { panic!() };
    assert_eq!((p.x, p.y), (20.0, 5.0));
    // Geometry files store no CRS here: no CRS is invented.
    assert!(conduits.crs.is_none());
}

#[test]
fn byte_budgets_bound_every_layer_and_the_listing() {
    let path = data("results_mesh_xs.h5");
    let tiny = [("MAX_BYTES", "1")];
    // Each layer, named, is refused within a one-byte array budget: family layers too, not only
    // the mesh.
    for layer in ["mesh_cells", "mesh_face_points", "cross_sections", "bc_lines", "reference_lines", "reference_points", "river_junctions"]
    {
        let e = terrio::open_vector(&path, &options(&tiny, &[layer])).unwrap_err();
        assert!(matches!(e, terrio::Error::Unsupported(_)), "{layer}: {e}");
    }
    // Either limit alone bounds the family datasets (attribute tables and coordinates); the
    // metadata limit bounds the root attributes first.
    for limit in ["MAX_BYTES", "METADATA_MAX_BYTES"] {
        let e = terrio::open_vector(&path, &options(&[(limit, "1")], &["cross_sections"])).unwrap_err();
        assert!(matches!(e, terrio::Error::Unsupported(_)), "{limit}: {e}");
    }
    // Listing reads within the budget too, and lists what it cannot read, so a default open
    // refuses every layer rather than reading any above the budget.
    let _ = terrio::warnings::take();
    let r = terrio::open_vector(&path, &options(&tiny, &[]));
    let warnings = terrio::warnings::take();
    assert!(matches!(r, Err(terrio::Error::Unsupported(ref m)) if m.contains("refused every")), "{r:?}");
    assert_eq!(warnings.len(), 9, "{warnings:?}");
    // A budget that holds the data reads it.
    let ok = options(&[("MAX_BYTES", "1048576"), ("METADATA_MAX_BYTES", "1048576")], &[]);
    assert_eq!(terrio::open_vector(&path, &ok).unwrap().layers.len(), 9);
}

#[test]
fn root_attributes_keep_to_the_metadata_budget() {
    // A 16,028-byte Projection, and an unrelated 16,384-byte root attribute next to a short
    // Projection (scripts/make_hecras_root_fixtures.py): both above a 4 KiB metadata budget.
    let budget = [("METADATA_MAX_BYTES", "4096"), ("MAX_BYTES", "4096")];
    for file in ["large_projection.h5", "large_root_attribute.h5"] {
        let path = data(file);
        let refused = |r: terrio::Result<terrio::VectorDataset>| {
            let e = r.err().unwrap_or_else(|| panic!("{file}: read within a 4 KiB metadata budget"));
            assert!(matches!(&e, terrio::Error::Unsupported(m) if m.contains("root attributes")), "{file}: {e}");
        };
        // From a file and from memory, with or without layers named.
        refused(terrio::open_vector(&path, &options(&budget, &[])));
        refused(terrio::open_vector(&path, &options(&budget, &["pipe_nodes"])));
        let bytes = std::fs::read(&path).unwrap();
        refused(terrio::memory::open_vector_bytes(bytes.clone(), file, &options(&budget, &[])));
        // Detection decodes only File Type: the files are still HEC-RAS files, and read with the
        // default budgets.
        let ds = terrio::open_vector(&path, &VectorOpenOptions::default()).unwrap();
        assert_eq!(ds.driver, VectorDriver::HECRAS);
        let wkt = ds.layers[0].crs.as_ref().and_then(|c| c.wkt.clone()).unwrap();
        assert_eq!(wkt.len(), if file == "large_projection.h5" { 16028 } else { 429 }, "{file}");
        assert_eq!(terrio::memory::open_vector_bytes(bytes, file, &VectorOpenOptions::default()).unwrap().layers.len(), 2);
    }
    // The typed API's crs() reads within the engine's default metadata budget (64 MiB).
    let hdf = terrio::hecras::HecRasHdf::open(data("large_projection.h5")).unwrap();
    assert_eq!(hdf.crs().unwrap().unwrap().len(), 16028);
}

#[test]
fn options_for_layers_the_file_does_not_store_are_errors() {
    // With no layer named, an option whose layers the file does not store is an error naming
    // the option, never a dataset without them.
    // (file, options, the option the error names)
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
    let cases: [Case; 5] = [
        ("pipes.h5", &[("XS_SUMMARIES", "Maximum Water Surface")], "XS_SUMMARIES"),
        ("pipes.h5", &[("FACE_MINIMUM", "Face Velocity")], "FACE_MINIMUM"),
        ("pipes.h5", &[("CELL_SERIES", "Water Surface"), ("TIME_STEP", "0")], "CELL_SERIES"),
        ("pipes.h5", &[("MESHES", "Good")], "MESHES"),
        ("results_mesh_xs.h5", &[("STRUCTURE_SERIES", "Structure Variables"), ("TIME_STEP", "0")], "STRUCTURE_SERIES"),
    ];
    for (file, pairs, option) in cases {
        let path = data(file);
        let check = |r: terrio::Result<terrio::VectorDataset>| {
            let e = r.err().unwrap_or_else(|| panic!("{file} {option}: opened"));
            assert!(
                matches!(&e, terrio::Error::InvalidArgument(m) if m.starts_with(option) && m.contains("does not store")),
                "{file}: {e}"
            );
        };
        check(terrio::open_vector(&path, &options(pairs, &[])));
        check(terrio::memory::open_vector_bytes(std::fs::read(&path).unwrap(), file, &options(pairs, &[])));
    }
    // Options for layers that are not read are still not looked up when layers are named.
    let o = options(&[("XS_SUMMARIES", "Maximum Water Surface")], &["pipe_nodes"]);
    assert_eq!(terrio::open_vector(&data("pipes.h5"), &o).unwrap().layers.len(), 1);
    // An option applies when any of its layers is stored: MESHES on a file without perimeters.
    assert!(terrio::open_vector(&data("results_mesh_xs.h5"), &options(&[("MESHES", "Good")], &[])).is_ok());
}

#[test]
fn invalid_requests_fail_a_default_open() {
    let path = data("results_mesh_xs.h5");
    // With no layer named, an invalid request is an error, never a dataset without those layers.
    for pairs in [
        &[("CELL_SERIES", "Water Surface")][..],
        &[("FACE_SERIES", "Face Velocity")],
        &[("XS_RESULTS", "Water Surface")],
        &[("BC_SERIES", "YES")],
        &[("STRUCTURE_SERIES", "Structure Variables")],
        &[("REFERENCE_LINE_SERIES", "Flow")],
        &[("REFERENCE_POINT_SERIES", "Water Surface")],
    ] {
        let e = terrio::open_vector(&path, &options(pairs, &[])).unwrap_err();
        assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.contains("TIME_STEP")), "{pairs:?}: {e}");
    }
    // A join the file does not store fails the layer it targets, which is not left out.
    for pairs in [
        &[("CELL_SUMMARIES", "No Such Variable")][..],
        &[("CELL_SERIES", "Depth"), ("TIME_STEP", "0")],
        &[("XS_SUMMARIES", "No Such Variable")],
        &[("MESHES", "No Such Mesh")],
    ] {
        let r = terrio::open_vector(&path, &options(pairs, &[]));
        assert!(r.is_err(), "{pairs:?}: {:?}", r.map(|d| d.layers.len()));
    }
    // A row option nothing reads is an error up front, never a dataset without the mesh layers:
    // TIME_STEP without a series, XS_PROFILE without XS_RESULTS, and both rows for XS_RESULTS.
    for (pairs, layers) in [
        (&[("TIME_STEP", "1")][..], &[][..]),
        (&[("TIME_STEP", "1")], &["river_junctions"]),
        (&[("XS_PROFILE", "0")], &[]),
        (&[("XS_RESULTS", "Water Surface"), ("XS_PROFILE", "0"), ("TIME_STEP", "1")], &[]),
    ] {
        let e = terrio::open_vector(&path, &options(pairs, layers)).unwrap_err();
        assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.contains("TIME_STEP") || m.contains("XS_PROFILE")), "{pairs:?}: {e}");
    }
    // A valid request opens every layer.
    let ds = terrio::open_vector(&path, &options(&[("CELL_SERIES", "Water Surface"), ("TIME_STEP", "1")], &[])).unwrap();
    assert_eq!(ds.layers.len(), 9);
    assert!(ds.layers[0].schema.fields().iter().any(|f| f.name == "water_surface"));
}
