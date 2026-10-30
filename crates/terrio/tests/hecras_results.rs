//! Joined values on the synthetic plan fixture `results_mesh_xs.h5` (see
//! `scripts/make_results_fixture.py`): static mesh arrays, summaries, time-step values, series
//! extremes and 1D cross-section results, with the exact stored numbers, and every misuse or
//! absence as an error.
#![cfg(feature = "hecras")]

use std::path::PathBuf;
use terrio::hecras::{CrossSectionOptions, Extreme, FeatureSeriesOptions, HecRasHdf, MeshAttribute, MeshOptions};
use terrio::{FieldValue, Layer};

fn open() -> HecRasHdf {
    HecRasHdf::open_with_threads(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/hecras/results_mesh_xs.h5"), 1).unwrap()
}

fn column(layer: &Layer, field: &str) -> Vec<FieldValue> {
    let i = layer.schema.fields().iter().position(|f| f.name == field).unwrap_or_else(|| panic!("no field {field}"));
    layer.features.iter().map(|f| f.attributes[i].clone()).collect()
}

fn floats(values: &[f64]) -> Vec<FieldValue> {
    values.iter().map(|&v| FieldValue::Float(v)).collect()
}

fn ints(values: &[i64]) -> Vec<FieldValue> {
    values.iter().map(|&v| FieldValue::Integer(v)).collect()
}

fn fields(layer: &Layer) -> Vec<&str> {
    layer.schema.fields().iter().map(|f| f.name.as_str()).collect()
}

#[test]
fn static_cell_arrays_join_by_cell_id() {
    let hdf = open();
    let options = MeshOptions::default().with_attribute(MeshAttribute::MinimumElevation).with_attribute(MeshAttribute::ManningsN);
    let cells = hdf.mesh_cells(&options).unwrap();
    assert_eq!(fields(&cells.layer), ["mesh_name", "cell_id", "minimum_elevation", "mannings_n"]);
    assert_eq!(column(&cells.layer, "cell_id"), ints(&[0, 1]));
    assert_eq!(column(&cells.layer, "minimum_elevation"), floats(&[10.25, 11.5]));
    assert_eq!(column(&cells.layer, "mannings_n"), floats(&[f64::from(0.035f32), f64::from(0.06f32)]));
    assert_eq!(cells.joined[0].source, "Cells Minimum Elevation");
    assert_eq!(cells.joined[0].datasets, ["Geometry/2D Flow Areas/Good/Cells Minimum Elevation"]);
    assert_eq!(cells.joined[1].fields, ["mannings_n"]);
    // Cell points carry every stored row, the ghost cell's NaN included.
    let points = hdf.mesh_cell_points(&MeshOptions::default().with_attribute(MeshAttribute::MinimumElevation)).unwrap();
    assert_eq!(column(&points.layer, "is_ghost"), [false, false, true].map(FieldValue::Boolean));
    assert!(matches!(column(&points.layer, "minimum_elevation")[2], FieldValue::Float(v) if v.is_nan()));
}

#[test]
fn static_face_arrays_join_by_face_id() {
    let hdf = open();
    let options = MeshOptions::default()
        .with_attribute(MeshAttribute::MinimumElevation)
        .with_attribute(MeshAttribute::FaceNormalAndLength)
        .with_attribute(MeshAttribute::FaceCells);
    let faces = hdf.mesh_faces(&options).unwrap();
    assert_eq!(fields(&faces.layer), ["mesh_name", "face_id", "minimum_elevation", "normal_x", "normal_y", "length", "cell_a", "cell_b"]);
    let n = faces.layer.len();
    assert_eq!(column(&faces.layer, "minimum_elevation"), floats(&(0..n).map(|i| 9.0 + i as f64 / 4.0).collect::<Vec<_>>()));
    assert_eq!(column(&faces.layer, "cell_a"), ints(&(0..n as i64).map(|i| i % 3).collect::<Vec<_>>()));
    assert_eq!(column(&faces.layer, "cell_b"), ints(&(0..n as i64).map(|i| (i + 1) % 3).collect::<Vec<_>>()));
    for value in column(&faces.layer, "length") {
        assert!(matches!(value, FieldValue::Float(v) if v > 0.0));
    }
}

#[test]
fn attributes_on_the_wrong_element_are_errors() {
    let hdf = open();
    let e = hdf.mesh_faces(&MeshOptions::default().with_attribute(MeshAttribute::ManningsN)).unwrap_err();
    assert!(matches!(e, terrio::Error::InvalidArgument(_)), "{e}");
    let e = hdf.mesh_cells(&MeshOptions::default().with_attribute(MeshAttribute::FaceCells)).unwrap_err();
    assert!(matches!(e, terrio::Error::InvalidArgument(_)), "{e}");
    let e = hdf.mesh_perimeters(&MeshOptions::default().with_attribute(MeshAttribute::MinimumElevation)).unwrap_err();
    assert!(matches!(e, terrio::Error::InvalidArgument(_)), "{e}");
}

#[test]
fn summary_value_and_time_rows() {
    let hdf = open();
    let cells = hdf.mesh_cells(&MeshOptions::default().with_summary("Maximum Water Surface")).unwrap();
    assert_eq!(column(&cells.layer, "maximum_water_surface"), floats(&[12.75, 13.0]));
    assert_eq!(column(&cells.layer, "maximum_water_surface_time"), floats(&[0.5, 1.0]));
    assert_eq!(cells.summaries[0].time_field.as_deref(), Some("maximum_water_surface_time"));
    assert!(cells.joined.is_empty());
    // Not stored for faces: an error, not an empty field.
    let e = hdf.mesh_faces(&MeshOptions::default().with_summary("Maximum Face Velocity")).unwrap_err();
    assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.starts_with("HEC-RAS not_found")), "{e}");
}

#[test]
fn time_step_values_with_their_stored_time() {
    let hdf = open();
    let cells = hdf.mesh_cells(&MeshOptions::default().with_series("Water Surface").at_time_step(1)).unwrap();
    assert_eq!(column(&cells.layer, "water_surface"), floats(&[12.75, 12.25]));
    let step = cells.time_step.unwrap();
    assert_eq!((step.row, step.days, step.date_stamp.as_deref()), (1, Some(0.5), Some("01JAN2000 12:00:00")));
    assert_eq!(cells.joined[0].units.as_deref(), Some("ft"));
    let faces = hdf.mesh_faces(&MeshOptions::default().with_series("Face Velocity").at_time_step(2)).unwrap();
    let n = faces.layer.len();
    let want: Vec<f64> = (0..n).map(|i| f64::from((((2 * n + i) % 5) as f32 - 2.0) / 4.0)).collect();
    assert_eq!(column(&faces.layer, "face_velocity"), floats(&want));
}

#[test]
fn time_step_misuse_is_an_error() {
    let hdf = open();
    for options in [
        MeshOptions::default().with_series("Water Surface"),
        MeshOptions::default().at_time_step(0),
        MeshOptions::default().with_series("Water Surface").at_time_step(3),
        MeshOptions::default().with_series("No Such Variable").at_time_step(0),
    ] {
        let e = hdf.mesh_cells(&options).unwrap_err();
        assert!(matches!(e, terrio::Error::InvalidArgument(_)), "{options:?}: {e}");
    }
    // A cell variable on the face layer.
    let e = hdf.mesh_faces(&MeshOptions::default().with_series("Water Surface").at_time_step(0)).unwrap_err();
    assert!(matches!(e, terrio::Error::InvalidArgument(_)), "{e}");
}

#[test]
fn series_extremes_with_their_rows() {
    let hdf = open();
    let options = MeshOptions::default().with_extreme("Water Surface", Extreme::Maximum).with_extreme("Water Surface", Extreme::Minimum);
    let cells = hdf.mesh_cells(&options).unwrap();
    assert_eq!(column(&cells.layer, "water_surface_max"), floats(&[12.75, 13.0]));
    assert_eq!(column(&cells.layer, "water_surface_max_row"), ints(&[1, 2]));
    assert_eq!(column(&cells.layer, "water_surface_min"), floats(&[12.0, 12.25]));
    assert_eq!(column(&cells.layer, "water_surface_min_row"), ints(&[0, 1]));
    // The same reduction twice gives the same fields twice: an error.
    let twice = MeshOptions::default().with_extreme("Water Surface", Extreme::Maximum).with_extreme("Water Surface", Extreme::Maximum);
    assert!(matches!(hdf.mesh_cells(&twice).unwrap_err(), terrio::Error::InvalidArgument(_)));
}

#[test]
fn cross_section_summaries_join_by_section_identity() {
    let hdf = open();
    let x = hdf.cross_sections(&CrossSectionOptions::default().with_summary("Maximum Water Surface")).unwrap();
    let f = &x.fields[0];
    assert_eq!(f.fields, ["maximum_water_surface", "maximum_water_surface_flow_at_max_wsel", "maximum_water_surface_time"]);
    assert_eq!(f.labels, [Some("Max WSEL".into()), Some("Flow at Max WSEL".into()), Some("Time at Max WSEL".into())]);
    assert_eq!(f.units, [Some("ft".into()), Some("cfs".into()), Some("days".into())]);
    assert_eq!(column(&x.layer, "rs"), ["300", "200", "100"].map(|s| FieldValue::Text(s.into())));
    assert_eq!(column(&x.layer, "maximum_water_surface"), floats(&[21.5, 19.0, 15.5]));
    assert_eq!(column(&x.layer, "maximum_water_surface_flow_at_max_wsel"), floats(&[100.0, 150.0, 175.0]));
    assert_eq!(column(&x.layer, "maximum_water_surface_time"), floats(&[0.5, 1.0, 1.0]));
    assert_eq!(x.layer.crs.as_ref().and_then(|c| c.epsg), None);
    assert!(x.layer.crs.as_ref().and_then(|c| c.wkt.as_deref()).is_some_and(|w| w.starts_with("PROJCS[\"Synthetic\"")));
}

#[test]
fn cross_section_results_at_one_row() {
    let hdf = open();
    let x = hdf.cross_sections(&CrossSectionOptions::default().with_unsteady("Water Surface", 2)).unwrap();
    assert_eq!(column(&x.layer, "water_surface"), floats(&[21.0, 19.0, 15.5]));
    assert_eq!(x.row_label.as_deref(), Some("02JAN2000 00:00:00"));
    for options in [
        CrossSectionOptions::default().with_summary("No Such Variable"),
        CrossSectionOptions::default().with_unsteady("Water Surface", 3),
        CrossSectionOptions::default().with_steady("Water Surface", 0),
    ] {
        assert!(hdf.cross_sections(&options).is_err(), "{options:?}");
    }
}

#[test]
fn river_junctions_are_points_with_their_stored_columns() {
    let hdf = open();
    let names = hdf.layer_names().unwrap();
    assert!(names.iter().any(|n| n == "river_junctions"), "{names:?}");
    assert!(!names.iter().any(|n| n == "pump_stations"), "not stored, not listed: {names:?}");
    let j = hdf.read_layer("river_junctions").unwrap();
    assert_eq!(fields(&j), ["source_row", "name", "description"]);
    assert_eq!(
        j.features[0].attributes,
        [FieldValue::Integer(0), FieldValue::Text("Confluence".into()), FieldValue::Text("synthetic".into())]
    );
    assert_eq!(j.features[0].geometry, Some(terrio::Geometry::Point(terrio::vector::Coord::xy(4.0, 1.5))));
    assert!(matches!(hdf.read_layer("pump_stations").unwrap_err(), terrio::Error::InvalidArgument(_)));
}

#[test]
fn bc_line_values_join_by_name_and_area() {
    let hdf = open();
    let x = hdf.bc_lines_at(&FeatureSeriesOptions::at_row(1)).unwrap();
    assert_eq!(x.fields[0].fields, ["stage", "flow"]);
    assert_eq!(x.fields[0].units, [Some("ft".into()), Some("cfs".into())]);
    assert_eq!(column(&x.layer, "name"), ["Inflow", "Outflow"].map(|s| FieldValue::Text(s.into())));
    assert_eq!(column(&x.layer, "stage"), floats(&[12.5, 11.5]));
    assert_eq!(column(&x.layer, "flow"), floats(&[7.5, 7.5]));
    assert_eq!(x.row_label.as_deref(), Some("01JAN2000 12:00:00"));
    assert!(hdf.bc_lines_at(&FeatureSeriesOptions::at_row(3)).is_err());
    // No structures stored: an error, not an empty join.
    assert!(hdf.structures_at(&FeatureSeriesOptions::at_row(0).with_variable("Structure Variables")).is_err());
}

#[test]
fn an_area_without_boundary_series_leaves_the_others_joined() {
    // `results_two_areas.h5`: the same plan with a second 2D flow area, `Dry`, that stores no
    // boundary series (see scripts/make_hecras_area_fixture.py).
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/hecras/results_two_areas.h5");
    let hdf = HecRasHdf::open_with_threads(path, 1).unwrap();
    assert_eq!(hdf.mesh_names().unwrap(), ["Dry", "Good"]);
    let x = hdf.bc_lines_at(&FeatureSeriesOptions::at_row(1)).unwrap();
    assert_eq!(x.fields[0].fields, ["stage", "flow"]);
    assert_eq!(column(&x.layer, "stage"), floats(&[12.5, 11.5]));
    assert_eq!(column(&x.layer, "flow"), floats(&[7.5, 7.5]));
}

#[test]
fn reference_point_values_join_by_the_engine_link() {
    let hdf = open();
    let x = hdf.reference_points_at(&FeatureSeriesOptions::at_row(1).with_variable("Water Surface")).unwrap();
    // Results are stored for Gauge B then Gauge A; the engine links them back to rows 1 and 0.
    assert_eq!(column(&x.layer, "name"), ["Gauge A", "Gauge B"].map(|s| FieldValue::Text(s.into())));
    assert_eq!(column(&x.layer, "water_surface"), floats(&[12.25, 12.75]));
    assert_eq!(x.fields[0].units, [Some("ft".into())]);
    assert!(hdf.reference_points_at(&FeatureSeriesOptions::at_row(0).with_variable("Velocity")).is_err());
}

#[test]
fn reference_lines_join_only_when_the_engine_links_them() {
    let hdf = open();
    let r = hdf.reference_lines_at(&FeatureSeriesOptions::at_row(1).with_variable("Flow"));
    match r {
        // An engine that links reference-line columns to geometry rows.
        Ok(x) => assert_eq!(column(&x.layer, "flow"), floats(&[2.5])),
        // The pinned engine does not link them: refused, never guessed by column order.
        Err(e) => assert!(matches!(&e, terrio::Error::Unsupported(m) if m.starts_with("HEC-RAS unsupported(identity_layout)")), "{e}"),
    }
}

#[test]
fn face_points_carry_the_stored_perimeter_flag_and_elevation() {
    let hdf = open();
    assert!(hdf.layer_names().unwrap().contains(&"mesh_face_points".to_string()));
    let l = hdf.mesh_face_points(&MeshOptions::default()).unwrap().layer;
    assert_eq!(fields(&l), ["mesh_name", "face_point_id", "is_perimeter", "elevation"]);
    let n = l.len();
    assert!(n > 0);
    assert_eq!(column(&l, "face_point_id"), ints(&(0..n as i64).collect::<Vec<_>>()));
    // The stored integers, unconverted (the fixture alternates -1 and 0).
    assert_eq!(column(&l, "is_perimeter"), ints(&(0..n).map(|k| if k % 2 == 0 { -1 } else { 0 }).collect::<Vec<_>>()));
    assert_eq!(column(&l, "elevation"), floats(&(0..n).map(|k| f64::from(10.0f32 + 0.25 * k as f32)).collect::<Vec<_>>()));
    assert!(l.crs.as_ref().and_then(|c| c.wkt.as_deref()).is_some_and(|w| w.starts_with("PROJCS[\"Synthetic\"")));
    // Values join to cell and face layers only.
    assert!(hdf.mesh_face_points(&MeshOptions::default().with_summary("Maximum Water Surface")).is_err());
}

#[test]
fn cross_section_tables_join_as_json() {
    use terrio::hecras::CrossSectionTable as T;
    let hdf = open();
    let options = CrossSectionOptions::default().with_table(T::StationElevation).with_table(T::ManningsN).with_table(T::IneffectiveAreas);
    let l = hdf.cross_sections(&options).unwrap().layer;
    let kinds: Vec<_> = l.schema.fields().iter().rev().take(3).map(|f| (f.name.as_str(), f.field_type)).collect();
    assert_eq!(
        kinds,
        [
            ("ineffective_areas", terrio::FieldType::Json),
            ("mannings_n", terrio::FieldType::Json),
            ("station_elevation", terrio::FieldType::Json)
        ]
    );
    let text = |field: &str| -> Vec<String> {
        column(&l, field)
            .into_iter()
            .map(|v| match v {
                FieldValue::Text(t) => t,
                other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(
        text("station_elevation"),
        ["[[0.0,12.5],[10.0,10.25],[20.0,12.0]]", "[[0.0,11.0],[15.0,11.5]]", "[[0.0,9.0],[5.0,8.5],[10.0,8.0],[15.0,9.5]]"]
    );
    // float32 values widened exactly, in their shortest exact decimal form.
    let n = |x: f32| f64::from(x).to_string();
    assert_eq!(
        text("mannings_n"),
        [format!("[[0.0,{}]]", n(0.035)), format!("[[0.0,{}],[10.0,{}]]", n(0.04), n(0.03)), format!("[[0.0,{}]]", n(0.05))]
    );
    assert_eq!(
        text("ineffective_areas"),
        [
            "[]".to_string(),
            r#"[{"Left Sta":0.0,"Right Sta":5.0,"Elevation":11.25,"Permanent":0},{"Left Sta":10.0,"Right Sta":15.0,"Elevation":11.5,"Permanent":1}]"#.to_string(),
            "[]".to_string()
        ]
    );
    // The same through the open option.
    let mut o = terrio::VectorOpenOptions { layers: vec!["cross_sections".into()], ..Default::default() };
    o.hecras.set("XS_GEOMETRY", "station_elevation, INEFFECTIVE_AREAS").unwrap();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/hecras/results_mesh_xs.h5");
    let ds = terrio::open_vector(&path.to_string_lossy(), &o).unwrap();
    assert_eq!(column(&ds.layers[0], "station_elevation"), column(&l, "station_elevation"));
    assert!(o.hecras.set("XS_GEOMETRY", "LEVEES").is_err());
}

#[test]
fn root_attributes_are_read_as_stored() {
    let attributes = open().file_attributes().unwrap();
    let names: Vec<_> = attributes.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["File Type", "File Version", "Projection", "Units System"]);
    assert_eq!(attributes[0].1, "HEC-RAS Results");
    assert_eq!(attributes[3].1, "US Customary");
}
