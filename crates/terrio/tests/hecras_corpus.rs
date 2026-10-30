//! Opt-in checks of HEC-RAS files from a corpus directory against an independent h5py oracle.
//!
//! Set `TERRIO_HECRAS_TEST_DATA` to the corpus directory and `TERRIO_HECRAS_ORACLE` to the JSON
//! that `scripts/hecras_oracle.py` writes for its files; `scripts/compare_hecras_with_h5py.py`
//! does both for the HEC-RAS 7.0.1 files of a directory. Without both variables, the test reports
//! that it was skipped and passes. Each file is reported as `OK <file>` or `DIFF <file>: ...`.
#![cfg(feature = "hecras")]

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use terrio::hecras::{
    CrossSectionOptions, CrossSectionTable, Extreme, FeatureSeriesOptions, HecRasHdf, MeshAttribute, MeshLayer, MeshOptions,
};
use terrio::{FieldValue, Geometry, Layer};

/// Reads the pinned engine refuses on these files, with the refusal it gives. ras-commander has
/// no h5py reading for them either (HEC-RAS 5.0 files without `2D Flow Areas/Attributes`). A
/// named file matches only when it is a HEC-RAS 5.0 file (its stored `File Version`): HEC-RAS
/// 7.0.1 runs of the same plans store the tables these reads need.
/// Through terrio a refusal must be an error, never an empty or guessed layer. A refusal not
/// listed here fails the test, and so does a listed one that starts to succeed.
const KNOWN_REFUSALS: &[(&str, &str, &str)] = &[
    ("BaldEagleDamBrk.p07.hdf", "maximum_face_velocity", "HEC-RAS unsupported(identity_layout)"),
    ("BaldEagleDamBrk.p08.hdf", "maximum_face_velocity", "HEC-RAS unsupported(identity_layout)"),
    ("BaldEagleDamBrk.p19.hdf", "maximum_face_velocity", "HEC-RAS unsupported(identity_layout)"),
    ("BaldEagleDamBrk.g12.hdf", "mesh_cell_points", "HEC-RAS not_found"),
    ("BaldEagleDamBrk.p07.hdf", "mesh_cell_points", "HEC-RAS not_found"),
    ("BaldEagleDamBrk.p08.hdf", "mesh_cell_points", "HEC-RAS not_found"),
    ("BaldEagleDamBrk.p19.hdf", "mesh_cell_points", "HEC-RAS not_found"),
    // Static mesh arrays: the engine reads them through the same `2D Flow Areas/Attributes`
    // descriptor. `*` matches every static array key.
    ("BaldEagleDamBrk.g12.hdf", "static:*", "HEC-RAS not_found"),
    ("BaldEagleDamBrk.p07.hdf", "static:*", "HEC-RAS not_found"),
    ("BaldEagleDamBrk.p08.hdf", "static:*", "HEC-RAS not_found"),
    ("BaldEagleDamBrk.p19.hdf", "static:*", "HEC-RAS not_found"),
    // A 2D flow area stored as an outline only (no computed mesh): the engine refuses the mesh
    // inventory, so the mesh layers and the perimeter are listed but refused.
    ("1101001404.g01.rr.hdf", "mesh:*", "HEC-RAS unsupported(not_implemented)"),
];

/// Named entries of [`KNOWN_REFUSALS`] that match whatever the file's HEC-RAS version.
const ANY_VERSION: &[&str] = &["1101001404.g01.rr.hdf"];

/// Whether a listed file (a file name, or `*` for any) is the oracle's file key (a relative path).
fn file_matches(listed: &str, file: &str) -> bool {
    listed == "*" || file.rsplit(['/', '\\']).next() == Some(listed)
}

fn refusal_matches(listed: &str, what: &str) -> bool {
    listed.strip_suffix('*').map_or(listed == what, |prefix| what.starts_with(prefix))
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// The oracle's line digest: per line its vertex count, then x, y as little-endian f64.
fn lines_digest<'a>(lines: impl Iterator<Item = Vec<(f64, f64)>> + 'a) -> String {
    let mut h = Sha256::new();
    for line in lines {
        h.update((line.len() as u64).to_le_bytes());
        for (x, y) in line {
            h.update(x.to_le_bytes());
            h.update(y.to_le_bytes());
        }
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The oracle's digest of a column over `rows`: little-endian f64 for floats, i64 for integers.
fn float_column(layer: &Layer, rows: std::ops::Range<usize>, field: &str) -> String {
    let features: Vec<_> = layer.features[rows].iter().collect();
    column_digest(layer, &features, field)
}

fn column_digest(layer: &Layer, features: &[&terrio::Feature], field: &str) -> String {
    let i = layer.schema.fields().iter().position(|f| f.name == field).unwrap_or_else(|| panic!("no field {field}"));
    let bytes: Vec<u8> = features
        .iter()
        .flat_map(|f| match f.attributes[i] {
            FieldValue::Float(v) => v.to_le_bytes(),
            FieldValue::Integer(v) => v.to_le_bytes(),
            ref other => panic!("{field}: {other:?}"),
        })
        .collect();
    sha256_hex(&bytes)
}

/// Per mesh (in `order`), the feature rows of a mesh layer.
fn mesh_rows(layer: &MeshLayer, order: &[String]) -> Vec<(String, std::ops::Range<usize>)> {
    order
        .iter()
        .map(|name| {
            let mesh = FieldValue::Text(name.clone());
            let first = layer.layer.features.iter().position(|f| f.attributes[0] == mesh).unwrap();
            let n = layer.layer.features[first..].iter().take_while(|f| f.attributes[0] == mesh).count();
            (name.clone(), first..first + n)
        })
        .collect()
}

/// Features in attribute-row (`source_row`) order.
fn by_source_row(layer: &Layer) -> Vec<&terrio::Feature> {
    let mut features: Vec<_> = layer.features.iter().collect();
    features.sort_by_key(|f| match f.attributes[0] {
        FieldValue::Integer(r) => r,
        ref other => panic!("source_row {other:?}"),
    });
    features
}

struct Check<'a> {
    file: &'a str,
    /// Whether the file is a HEC-RAS 5.0 file (named refusals apply only to those).
    legacy: bool,
    refusals_seen: Vec<(String, String)>,
}

impl Check<'_> {
    /// `Some(value)` on success; `None` for a listed refusal (checked).
    fn expect<T>(&mut self, what: &str, r: terrio::Result<T>) -> Option<T> {
        let known = KNOWN_REFUSALS.iter().find(|(f, w, _)| {
            file_matches(f, self.file) && (*f == "*" || self.legacy || ANY_VERSION.contains(f)) && refusal_matches(w, what)
        });
        match (r, known) {
            (Ok(v), None) => Some(v),
            (Ok(_), Some(_)) => panic!("{} {what}: now succeeds; update KNOWN_REFUSALS and the oracle checks", self.file),
            (Err(e), Some((_, _, prefix))) => {
                assert!(
                    matches!(&e, terrio::Error::Unsupported(m) | terrio::Error::InvalidArgument(m) if m.starts_with(prefix)),
                    "{} {what}: {e}",
                    self.file
                );
                self.refusals_seen.push((self.file.to_string(), what.to_string()));
                None
            }
            (Err(e), None) => panic!("{} {what}: unexpected refusal: {e}", self.file),
        }
    }
}

#[test]
fn public_models_match_the_h5py_oracle() {
    let (Some(root), Some(oracle)) = (std::env::var_os("TERRIO_HECRAS_TEST_DATA"), std::env::var_os("TERRIO_HECRAS_ORACLE")) else {
        eprintln!("skipped: set TERRIO_HECRAS_TEST_DATA and TERRIO_HECRAS_ORACLE");
        return;
    };
    let oracle: Value = serde_json::from_slice(&std::fs::read(oracle).unwrap()).unwrap();
    let files = oracle.as_object().unwrap();
    assert!(!files.is_empty(), "the oracle lists no files");
    let mut seen = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let (mut cells_checked, mut faces_checked, mut families_checked) = (0usize, 0usize, 0usize);
    let (mut joins_checked, mut xs_checked, mut series_checked) = (0usize, 0usize, 0usize);
    let mut stored_checked = 0usize;
    let legacy_files = std::sync::Mutex::new(Vec::<String>::new());
    for (file, want) in files {
        // Each file is checked on its own, and reported as `OK <file>` or `DIFF <file>: <what>`
        // (scripts/compare_hecras_with_h5py.py reads these lines); the test fails at the end.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let path = PathBuf::from(&root).join(file);
            assert_eq!(
                sha256_hex(&std::fs::read(&path).unwrap()),
                want["sha256"].as_str().unwrap(),
                "{file}: not the file the oracle read"
            );
            let hdf = HecRasHdf::open(&path).unwrap();
            let version = hdf.file_attributes().unwrap().into_iter().find(|(n, _)| n == "File Version").map(|(_, v)| v);
            let legacy = version.as_deref().is_some_and(|v| v.starts_with("HEC-RAS 5.0"));
            if legacy {
                legacy_files.lock().unwrap().push(file.clone());
            }
            let mut check = Check { file, legacy, refusals_seen: Vec::new() };
            assert_eq!(hdf.crs().unwrap().as_deref(), want["projection"].as_str(), "{file} CRS");
            assert_eq!(hdf.units_system().unwrap().as_deref(), want["units_system"].as_str(), "{file} units");
            let layers = hdf.layer_names().unwrap();
            let meshes = want["meshes"].as_object().unwrap();
            if meshes.is_empty() {
                // No mesh the oracle can read: a listed mesh layer must be a listed refusal.
                for layer in terrio::hecras::MESH_LAYERS.iter().filter(|l| layers.iter().any(|n| n == *l)) {
                    let read = check.expect(&format!("mesh:{layer}"), hdf.read_layer(layer));
                    assert!(read.is_none(), "{file} {layer}: read, but the oracle finds no mesh");
                }
            }

            if !meshes.is_empty() {
                let mut names = hdf.mesh_names().unwrap();
                names.sort();
                let mut want_names: Vec<_> = meshes.keys().cloned().collect();
                want_names.sort();
                assert_eq!(names, want_names, "{file} meshes");
                let order = hdf.mesh_names().unwrap();

                // Cells, with Maximum Water Surface where stored.
                let has_wse = meshes.values().any(|m| m.get("maximum_water_surface").is_some());
                let cells = hdf.mesh_cells(&MeshOptions::default()).unwrap();
                assert_eq!(cells.layer.crs.as_ref().and_then(|c| c.wkt.as_deref()), want["projection"].as_str());
                assert!(cells.layer.crs.as_ref().is_none_or(|c| c.epsg.is_none()), "no EPSG code is guessed");
                let joined = if has_wse {
                    check.expect("maximum_water_surface", hdf.mesh_cells(&MeshOptions::default().with_summary("Maximum Water Surface")))
                } else {
                    None
                };
                let mut offset = 0;
                for name in &order {
                    let mesh = &meshes[name];
                    let drawn = mesh["drawn"].as_u64().unwrap() as usize;
                    let rows = offset..offset + drawn;
                    let features = &cells.layer.features[rows.clone()];
                    assert!(features.iter().all(|f| f.attributes[0] == FieldValue::Text(name.clone())), "{file} {name}");
                    assert_eq!(features[0].attributes[1], FieldValue::Integer(mesh["drawn_cell_ids"][0].as_i64().unwrap()));
                    assert_eq!(features[drawn - 1].attributes[1], FieldValue::Integer(mesh["drawn_cell_ids"][1].as_i64().unwrap()));
                    let stored = mesh["stored_cells"].as_u64().unwrap() as usize;
                    assert_eq!(cells.omitted.iter().filter(|o| &o.mesh_name == name).count(), stored - drawn, "{file} {name} omitted");
                    if let Some(joined) = &joined {
                        let w = &mesh["maximum_water_surface"];
                        assert_eq!(
                            float_column(&joined.layer, rows.clone(), "maximum_water_surface"),
                            w["value_sha256"].as_str().unwrap(),
                            "{file} {name} WSE"
                        );
                        assert_eq!(
                            float_column(&joined.layer, rows.clone(), "maximum_water_surface_time"),
                            w["time_sha256"].as_str().unwrap(),
                            "{file} {name} WSE time"
                        );
                    }
                    cells_checked += drawn;
                    offset += drawn;
                }
                assert_eq!(offset, cells.layer.len(), "{file}: every polygon belongs to a listed mesh");

                // Cell points and ghost flags.
                if let Some(points) = check.expect("mesh_cell_points", hdf.mesh_cell_points(&MeshOptions::default())) {
                    let mut offset = 0;
                    for name in &order {
                        let mesh = &meshes[name];
                        let stored = mesh["stored_cells"].as_u64().unwrap() as usize;
                        let physical = mesh["physical_cells"].as_u64().expect("ghost flags need the physical count") as usize;
                        let ghosts = points.layer.features[offset..offset + stored]
                            .iter()
                            .filter(|f| f.attributes[2] == FieldValue::Boolean(true))
                            .count();
                        assert_eq!(ghosts, stored - physical, "{file} {name} ghosts");
                        offset += stored;
                    }
                    assert_eq!(offset, points.layer.len());
                }

                // Faces, with Maximum Face Velocity where stored.
                let has_fv = meshes.values().any(|m| m.get("maximum_face_velocity").is_some());
                let faces = hdf.mesh_faces(&MeshOptions::default()).unwrap();
                let joined = if has_fv {
                    check.expect("maximum_face_velocity", hdf.mesh_faces(&MeshOptions::default().with_summary("Maximum Face Velocity")))
                } else {
                    None
                };
                let mut offset = 0;
                for name in &order {
                    let mesh = &meshes[name];
                    let n = mesh["faces"].as_u64().unwrap() as usize;
                    let rows = offset..offset + n;
                    let digest = lines_digest(faces.layer.features[rows.clone()].iter().map(|f| match &f.geometry {
                        Some(Geometry::LineString(l)) => l.iter().map(|c| (c.x, c.y)).collect(),
                        other => panic!("{other:?}"),
                    }));
                    assert_eq!(digest, mesh["faces_sha256"].as_str().unwrap(), "{file} {name} faces");
                    if let Some(joined) = &joined {
                        let w = &mesh["maximum_face_velocity"];
                        assert_eq!(
                            float_column(&joined.layer, rows.clone(), "maximum_face_velocity"),
                            w["value_sha256"].as_str().unwrap(),
                            "{file} {name} FV"
                        );
                        assert_eq!(
                            float_column(&joined.layer, rows.clone(), "maximum_face_velocity_time"),
                            w["time_sha256"].as_str().unwrap(),
                            "{file} {name} FV time"
                        );
                    }
                    faces_checked += n;
                    offset += n;
                }
                assert_eq!(offset, faces.layer.len());

                // Perimeters (closed again for the oracle's ring digest). An area without a
                // stored Perimeter makes the layer an error.
                let perimeters = hdf.mesh_perimeters(&MeshOptions::default());
                if meshes.values().any(|m| m["perimeter_sha256"].is_null()) {
                    assert!(perimeters.is_err(), "{file}: a mesh stores no Perimeter, so mesh_perimeters must be an error");
                }
                for (name, f) in order.iter().zip(perimeters.iter().flat_map(|p| &p.layer.features)) {
                    let Some(Geometry::Polygon { exterior, .. }) = &f.geometry else { panic!() };
                    let mut ring: Vec<_> = exterior.coords().iter().map(|c| (c.x, c.y)).collect();
                    ring.push(ring[0]);
                    assert_eq!(
                        lines_digest(std::iter::once(ring)),
                        meshes[name]["perimeter_sha256"].as_str().unwrap(),
                        "{file} {name} perimeter"
                    );
                }
            }

            if !meshes.is_empty() {
                let order = hdf.mesh_names().unwrap();
                // Static mesh arrays: stored on every mesh, or the request is an error.
                for (key, attribute, on_cells) in [
                    ("cells_minimum_elevation", MeshAttribute::MinimumElevation, true),
                    ("cells_mannings_n", MeshAttribute::ManningsN, true),
                    ("faces_minimum_elevation", MeshAttribute::MinimumElevation, false),
                    ("faces_normal_and_length", MeshAttribute::FaceNormalAndLength, false),
                    ("faces_cells", MeshAttribute::FaceCells, false),
                ] {
                    let options = MeshOptions::default().with_attribute(attribute);
                    let r = if on_cells { hdf.mesh_cells(&options) } else { hdf.mesh_faces(&options) };
                    if !meshes.values().all(|m| m["static"].get(key).is_some()) {
                        assert!(r.is_err(), "{file} {key}: an array a mesh does not store must be an error");
                        continue;
                    }
                    let Some(l) = check.expect(&format!("static:{key}"), r) else { continue };
                    assert_eq!(l.joined.len(), 1);
                    for (name, rows) in mesh_rows(&l, &order) {
                        let want: Vec<_> = meshes[&name]["static"][key].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                        assert_eq!(l.joined[0].fields.len(), want.len(), "{file} {name} {key} columns");
                        for (field, digest) in l.joined[0].fields.iter().zip(want) {
                            assert_eq!(float_column(&l.layer, rows.clone(), field), digest, "{file} {name} {key} {field}");
                        }
                        joins_checked += rows.len();
                    }
                }
                // Time-step values and the Water Surface maximum over every stored row.
                if let Some(step) = want.get("time_step") {
                    let row = step["row"].as_u64().unwrap();
                    for (variable, field, on_cells) in
                        [("Water Surface", "water_surface", true), ("Face Velocity", "face_velocity", false), ("Depth", "depth", true)]
                    {
                        if !meshes.values().all(|m| m.get("time_step").is_some_and(|t| t.get(field).is_some())) {
                            // Not stored (depth in HEC-RAS 6.x/7.0 plans): an error, never a value.
                            let options = MeshOptions::default().with_series(variable).at_time_step(row);
                            let r = if on_cells { hdf.mesh_cells(&options) } else { hdf.mesh_faces(&options) };
                            assert!(r.is_err(), "{file} {variable}: not stored, so it must be an error");
                            continue;
                        }
                        let options = MeshOptions::default().with_series(variable).at_time_step(row);
                        let r = if on_cells { hdf.mesh_cells(&options) } else { hdf.mesh_faces(&options) };
                        let Some(l) = check.expect(field, r) else { continue };
                        let t = l.time_step.as_ref().unwrap();
                        assert_eq!(t.row, row);
                        assert_eq!(t.date_stamp.as_deref(), step["date_stamp"].as_str(), "{file} time stamp");
                        assert_eq!(t.days, step["days"].as_f64(), "{file} time days");
                        for (name, rows) in mesh_rows(&l, &order) {
                            let want = meshes[&name]["time_step"][field].as_str().unwrap();
                            assert_eq!(float_column(&l.layer, rows.clone(), field), want, "{file} {name} {field} at row {row}");
                            joins_checked += rows.len();
                        }
                    }
                    for (variable, field) in [("Water Surface", "water_surface_max"), ("Depth", "depth_max")] {
                        if !meshes.values().all(|m| m.get(field).is_some()) {
                            continue;
                        }
                        let options = MeshOptions::default().with_extreme(variable, Extreme::Maximum);
                        if let Some(l) = check.expect(field, hdf.mesh_cells(&options)) {
                            let row_field = format!("{field}_row");
                            assert_eq!(l.joined[0].fields, [field.to_string(), row_field.clone()]);
                            for (name, rows) in mesh_rows(&l, &order) {
                                let w = &meshes[&name][field];
                                assert_eq!(
                                    float_column(&l.layer, rows.clone(), field),
                                    w["value_sha256"].as_str().unwrap(),
                                    "{file} {name} {field}"
                                );
                                assert_eq!(
                                    float_column(&l.layer, rows.clone(), &row_field),
                                    w["row_sha256"].as_str().unwrap(),
                                    "{file} {name} {row_field}"
                                );
                                joins_checked += rows.len();
                            }
                        }
                    }
                }
            }

            // 1D results on the cross sections, in attribute-row (source_row) order.
            let one_d = want["one_d"].as_object().unwrap();
            for (variable, rows) in one_d.iter().filter(|(k, _)| !matches!(k.as_str(), "unsteady" | "steady")) {
                let x = hdf
                    .cross_sections(&CrossSectionOptions::default().with_summary(variable))
                    .unwrap_or_else(|e| panic!("{file} {variable}: {e}"));
                let fields = &x.fields[0].fields;
                let rows = rows.as_array().unwrap();
                assert_eq!(fields.len(), rows.len(), "{file} {variable} rows");
                assert_eq!(fields[0], terrio_field(variable));
                let features = by_source_row(&x.layer);
                for (field, digest) in fields.iter().zip(rows) {
                    assert_eq!(column_digest(&x.layer, &features, field), digest.as_str().unwrap(), "{file} {variable} {field}");
                }
                xs_checked += features.len() * fields.len();
            }
            for mode in ["unsteady", "steady"] {
                let Some(r) = one_d.get(mode) else { continue };
                let row = r["row"].as_u64().unwrap();
                for (variable, digest) in r["values"].as_object().unwrap() {
                    let options = if mode == "steady" {
                        CrossSectionOptions::default().with_steady(variable, row)
                    } else {
                        CrossSectionOptions::default().with_unsteady(variable, row)
                    };
                    let x = hdf.cross_sections(&options).unwrap_or_else(|e| panic!("{file} {mode} {variable}: {e}"));
                    assert_eq!(x.row_label.as_deref(), r["label"].as_str(), "{file} {mode} row label");
                    let features = by_source_row(&x.layer);
                    assert_eq!(
                        column_digest(&x.layer, &features, &terrio_field(variable)),
                        digest.as_str().unwrap(),
                        "{file} {mode} {variable}"
                    );
                    xs_checked += features.len();
                }
            }

            // Structure and boundary-condition line values at the oracle's output row.
            for (layer, rows) in [("structures", &want["structures"]), ("bc_lines", &want["bc_lines"])] {
                let rows = rows.as_object().unwrap();
                // Only where the file stores the layer's geometry: a structure table without
                // centerlines (such as a lateral structure of a pump example) has nothing to join to.
                if rows.is_empty() || !layers.iter().any(|l| l == layer) {
                    continue;
                }
                let options = FeatureSeriesOptions::at_row(want["time_step"]["row"].as_u64().unwrap());
                let x = if layer == "structures" {
                    hdf.structures_at(&options.with_variable("Structure Variables"))
                } else {
                    hdf.bc_lines_at(&options)
                }
                .unwrap_or_else(|e| panic!("{file} {layer}: {e}"));
                assert_eq!(x.row_label.as_deref(), want["time_step"]["date_stamp"].as_str());
                let field = &x.fields[0];
                let index: Vec<usize> =
                    field.fields.iter().map(|n| x.layer.schema.fields().iter().position(|f| &f.name == n).unwrap()).collect();
                let mut linked = 0;
                for f in &x.layer.features {
                    let FieldValue::Integer(row) = f.attributes[0] else { panic!() };
                    let present: Vec<usize> = (0..index.len()).filter(|&k| f.attributes[index[k]] != FieldValue::Null).collect();
                    let Some(w) = rows.get(&row.to_string()) else {
                        assert!(present.is_empty(), "{file} {layer} row {row}: values the oracle does not link");
                        continue;
                    };
                    // Fields are joined by column label: structures may store their columns in
                    // different orders, so compare the label sets, then values in stored order.
                    let mut labels: Vec<&str> = present.iter().map(|&k| field.labels[k].as_str()).collect();
                    let want_labels: Vec<&str> = w["labels"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
                    let in_stored_order: Vec<usize> = want_labels
                        .iter()
                        .map(|l| {
                            present
                                .iter()
                                .copied()
                                .find(|&k| field.labels[k] == *l)
                                .unwrap_or_else(|| panic!("{file} {layer} row {row}: no {l}"))
                        })
                        .collect();
                    labels.sort_unstable();
                    let mut sorted = want_labels.clone();
                    sorted.sort_unstable();
                    assert_eq!(labels, sorted, "{file} {layer} row {row} labels");
                    let bytes: Vec<u8> = in_stored_order
                        .iter()
                        .flat_map(|&k| match f.attributes[index[k]] {
                            FieldValue::Float(v) => v.to_le_bytes(),
                            ref other => panic!("{other:?}"),
                        })
                        .collect();
                    assert_eq!(sha256_hex(&bytes), w["values_sha256"].as_str().unwrap(), "{file} {layer} row {row} values");
                    linked += 1;
                    series_checked += present.len();
                }
                assert_eq!(linked, rows.len(), "{file} {layer}: every oracle row linked");
            }

            // Reference line and point values at the oracle's row, linked by the oracle by name.
            for layer in ["reference_lines", "reference_points"] {
                for (variable, w) in want[layer].as_object().unwrap() {
                    let options = FeatureSeriesOptions::at_row(want["time_step"]["row"].as_u64().unwrap()).with_variable(variable);
                    let r = if layer == "reference_lines" { hdf.reference_lines_at(&options) } else { hdf.reference_points_at(&options) };
                    let Some(x) = check.expect(&format!("{layer}:{variable}"), r) else { continue };
                    assert_eq!(x.fields[0].fields, [terrio_field(variable)], "{file} {layer} {variable}");
                    let i = x.layer.schema.fields().iter().position(|f| f.name == terrio_field(variable)).unwrap();
                    let mut bytes = Vec::new();
                    for r in w["rows"].as_array().unwrap() {
                        let f = x.layer.features.iter().find(|f| f.attributes[0] == FieldValue::Integer(r.as_i64().unwrap())).unwrap();
                        match f.attributes[i] {
                            FieldValue::Float(v) => bytes.extend(v.to_le_bytes()),
                            ref other => panic!("{file} {layer} {variable}: {other:?}"),
                        }
                        series_checked += 1;
                    }
                    assert_eq!(sha256_hex(&bytes), w["values_sha256"].as_str().unwrap(), "{file} {layer} {variable}");
                }
            }

            // Family identities, for the families whose geometry the file stores.
            for (layer, fam) in want["families"].as_object().unwrap() {
                if !layers.iter().any(|l| l == layer) {
                    continue;
                }
                let l = hdf.read_layer(layer).unwrap();
                assert_eq!(l.len() as u64, fam["count"].as_u64().unwrap(), "{file} {layer} count");
                let idx: Vec<usize> = fam["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|f| {
                        let name = terrio_field(f.as_str().unwrap());
                        l.schema.fields().iter().position(|d| d.name == name).unwrap_or_else(|| panic!("{file} {layer}: no field {name}"))
                    })
                    .collect();
                let rows: Vec<String> = l
                    .features
                    .iter()
                    .map(|f| {
                        idx.iter()
                            .map(|&i| match &f.attributes[i] {
                                FieldValue::Text(t) => t.trim_end().to_string(),
                                other => panic!("{other:?}"),
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect();
                assert_eq!(sha256_hex(rows.join("\n").as_bytes()), fam["sha256"].as_str().unwrap(), "{file} {layer} identities");
                if let Some(points) = fam.get("points_sha256") {
                    let bytes: Vec<u8> = l
                        .features
                        .iter()
                        .flat_map(|f| match &f.geometry {
                            Some(Geometry::Point(c)) => [c.x.to_le_bytes(), c.y.to_le_bytes()].concat(),
                            other => panic!("{other:?}"),
                        })
                        .collect();
                    assert_eq!(sha256_hex(&bytes), points.as_str().unwrap(), "{file} {layer} points");
                }
                families_checked += l.len();
            }
            stored_checked += check_stored(&hdf, file, want);
            seen.extend(check.refusals_seen);
        }));
        match outcome {
            Ok(()) => eprintln!("OK {file}"),
            Err(e) => {
                let what = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                eprintln!("DIFF {file}: {}", what.unwrap_or_default().replace('\n', " "));
                failed.push(file.clone());
            }
        }
    }
    assert!(failed.is_empty(), "{} of {} files differ from the oracle: {}", failed.len(), files.len(), failed.join(", "));
    let legacy_files = legacy_files.into_inner().unwrap();
    for (file, what, _) in KNOWN_REFUSALS {
        let applies = |k: &String| file_matches(file, k) && (ANY_VERSION.contains(file) || legacy_files.contains(k));
        if *file != "*" && files.keys().any(applies) {
            assert!(
                seen.iter().any(|(f, w)| file_matches(file, f) && refusal_matches(what, w)),
                "{file} {what}: listed refusal not exercised"
            );
        }
    }
    eprintln!(
        "checked {cells_checked} cells, {faces_checked} faces, {families_checked} family features, {joins_checked} joined mesh values, \
         {xs_checked} cross-section values, {series_checked} structure and boundary values, {stored_checked} stored attribute values; {} known refusals",
        seen.len()
    );
}

/// The digest of one stored column as the oracle computes it (`values_digest`).
fn values_digest(values: &[&FieldValue], kind: &str) -> String {
    let mut bytes = Vec::new();
    match kind {
        "text" => {
            let texts: Vec<&str> = values
                .iter()
                .map(|v| match v {
                    FieldValue::Text(t) => t.as_str(),
                    other => panic!("text column holds {other:?}"),
                })
                .collect();
            return sha256_hex(texts.join("\n").as_bytes());
        }
        "int" => values.iter().for_each(|v| match v {
            FieldValue::Integer(i) => bytes.extend(i.to_le_bytes()),
            other => panic!("integer column holds {other:?}"),
        }),
        "float" => values.iter().for_each(|v| match v {
            FieldValue::Float(x) => bytes.extend(x.to_le_bytes()),
            other => panic!("float column holds {other:?}"),
        }),
        other => panic!("column kind {other}"),
    }
    sha256_hex(&bytes)
}

/// The terrio field type for an oracle column kind.
fn field_type(kind: &str) -> terrio::FieldType {
    match kind {
        "text" => terrio::FieldType::Text,
        "int" => terrio::FieldType::Integer,
        "float" => terrio::FieldType::Float,
        other => panic!("column kind {other}"),
    }
}

/// Checks every oracle column of `columns` against the layer: field type and every value, in
/// the order of `features`.
fn check_columns(layer: &Layer, features: &[&terrio::Feature], columns: &Value, what: &str) -> usize {
    let mut checked = 0;
    for column in columns.as_array().unwrap() {
        let (name, kind) = (column["name"].as_str().unwrap(), column["kind"].as_str().unwrap());
        let field = terrio_field(name);
        let i = layer.schema.fields().iter().position(|f| f.name == field).unwrap_or_else(|| panic!("{what}: no field {field}"));
        assert_eq!(layer.schema.fields()[i].field_type, field_type(kind), "{what} {field}: field type");
        let values: Vec<_> = features.iter().map(|f| &f.attributes[i]).collect();
        assert_eq!(values_digest(&values, kind), column["sha256"].as_str().unwrap(), "{what} {field}: values");
        checked += values.len();
    }
    checked
}

/// Every stored attribute terrio reads without h5py, against the oracle's `stored` section.
/// Returns the number of values compared.
fn check_stored(hdf: &HecRasHdf, file: &str, want: &Value) -> usize {
    let stored = &want["stored"];
    let mut checked = 0;

    // Root attributes, in stored order: text exactly, numbers by value.
    let attributes = hdf.file_attributes().unwrap();
    let expected = stored["file_attributes"].as_array().unwrap();
    let mut names: Vec<_> = attributes.iter().map(|(n, _)| n.as_str()).collect();
    let mut want_names: Vec<_> = expected.iter().map(|e| e[0].as_str().unwrap()).collect();
    names.sort_unstable();
    want_names.sort_unstable();
    assert_eq!(names, want_names, "{file} root attribute names");
    for e in expected {
        let name = e[0].as_str().unwrap();
        let value = &attributes.iter().find(|(n, _)| n == name).unwrap().1;
        match e[1].as_str().unwrap() {
            "text" => assert_eq!(value, e[2].as_str().unwrap(), "{file} root attribute {name}"),
            _ => {
                let got: Vec<f64> = value.split(", ").map(|v| v.parse().unwrap()).collect();
                let want: Vec<f64> = e[2].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
                assert_eq!(got, want, "{file} root attribute {name}");
            }
        }
        checked += 1;
    }

    // Every column of every family attribute table.
    let layers = hdf.layer_names().unwrap();
    for (layer, table) in stored["tables"].as_object().unwrap() {
        assert!(layers.contains(layer), "{file}: {layer} is stored but not listed");
        let l = hdf.read_layer(layer).unwrap_or_else(|e| panic!("{file} {layer}: {e}"));
        let features = by_source_row(&l);
        assert_eq!(features.len() as u64, table["rows"].as_u64().unwrap(), "{file} {layer}: one feature per attribute row");
        checked += check_columns(&l, &features, &table["columns"], &format!("{file} {layer}"));
    }

    // Pipe network geometry.
    if let Some(want) = stored.get("pipe_conduits_lines_sha256") {
        let l = hdf.read_layer("pipe_conduits").unwrap();
        let lines = by_source_row(&l).into_iter().flat_map(|f| match &f.geometry {
            Some(Geometry::LineString(c)) => vec![c.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>()],
            Some(Geometry::MultiLineString(parts)) => parts.iter().map(|c| c.iter().map(|p| (p.x, p.y)).collect()).collect(),
            other => panic!("{other:?}"),
        });
        assert_eq!(lines_digest(lines), want.as_str().unwrap(), "{file} pipe_conduits lines");
    }
    if let Some(want) = stored.get("pipe_nodes_points_sha256") {
        let l = hdf.read_layer("pipe_nodes").unwrap();
        let bytes: Vec<u8> = by_source_row(&l)
            .into_iter()
            .flat_map(|f| match &f.geometry {
                Some(Geometry::Point(c)) => [c.x.to_le_bytes(), c.y.to_le_bytes()].concat(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sha256_hex(&bytes), want.as_str().unwrap(), "{file} pipe_nodes points");
    }

    // The 2D flow area table on the perimeters, and the face points.
    let meshes = want["meshes"].as_object().unwrap();
    if !meshes.is_empty() {
        // An area without a stored Perimeter makes the layer an error (checked above).
        if meshes.values().all(|m| !m["perimeter_sha256"].is_null()) {
            let perimeters = hdf.mesh_perimeters(&MeshOptions::default()).unwrap();
            let fields = perimeters.layer.schema.fields();
            match stored.get("flow_areas") {
                Some(areas) => {
                    for f in &perimeters.layer.features {
                        let FieldValue::Text(mesh) = &f.attributes[0] else { panic!() };
                        for column in areas[mesh].as_array().unwrap() {
                            let (name, kind, sha) = (column[0].as_str().unwrap(), column[1].as_str().unwrap(), column[2].as_str().unwrap());
                            let field = terrio_field(name);
                            let i = fields.iter().position(|d| d.name == field).unwrap_or_else(|| panic!("{file} perimeters: no {field}"));
                            assert_eq!(fields[i].field_type, field_type(kind), "{file} {mesh} {field}: field type");
                            assert_eq!(values_digest(&[&f.attributes[i]], kind), sha, "{file} {mesh} {field}");
                            checked += 1;
                        }
                    }
                }
                None => assert_eq!(fields.len(), 1, "{file}: no stored 2D flow area table, so only mesh_name"),
            }
        }
        let points = hdf.mesh_face_points(&MeshOptions::default()).unwrap();
        let mut offset = 0;
        for name in hdf.mesh_names().unwrap() {
            let w = &stored["face_points"][&name];
            let n = w["count"].as_u64().unwrap() as usize;
            let rows = &points.layer.features[offset..offset + n];
            assert!(rows.iter().all(|f| f.attributes[0] == FieldValue::Text(name.clone())), "{file} {name} face points");
            let xy: Vec<u8> = rows
                .iter()
                .flat_map(|f| match &f.geometry {
                    Some(Geometry::Point(c)) => [c.x.to_le_bytes(), c.y.to_le_bytes()].concat(),
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(sha256_hex(&xy), w["xy_sha256"].as_str().unwrap(), "{file} {name} face point coordinates");
            let ids: Vec<_> = rows.iter().map(|f| &f.attributes[1]).collect();
            assert!(ids.iter().enumerate().all(|(i, v)| **v == FieldValue::Integer(i as i64)), "{file} {name} face_point_id");
            let perimeter: Vec<_> = rows.iter().map(|f| &f.attributes[2]).collect();
            assert_eq!(Some(values_digest(&perimeter, "int").as_str()), w["is_perimeter_sha256"].as_str(), "{file} {name} is_perimeter");
            let elevation: Vec<_> = rows.iter().map(|f| &f.attributes[3]).collect();
            match w["elevation_sha256"].as_str() {
                Some(sha) => assert_eq!(values_digest(&elevation, "float"), sha, "{file} {name} face point elevation"),
                None => assert!(elevation.iter().all(|v| **v == FieldValue::Null), "{file} {name}: no stored elevation, so null"),
            }
            checked += 3 * n;
            offset += n;
        }
        assert_eq!(offset, points.layer.len(), "{file}: every face point belongs to a listed mesh");
    }

    // Cross-section station-elevation, Manning's n and ineffective areas (JSON fields).
    for (key, w) in stored["xs_tables"].as_object().unwrap() {
        let table = match key.as_str() {
            "station_elevation" => CrossSectionTable::StationElevation,
            "mannings_n" => CrossSectionTable::ManningsN,
            "ineffective_areas" => CrossSectionTable::IneffectiveAreas,
            other => panic!("{other}"),
        };
        let l = hdf.cross_sections(&CrossSectionOptions::default().with_table(table)).unwrap().layer;
        let i = l.schema.fields().iter().position(|f| f.name == *key).unwrap();
        assert_eq!(l.schema.fields()[i].field_type, terrio::FieldType::Json, "{file} {key}: a JSON field");
        let features = by_source_row(&l);
        assert_eq!(features.len() as u64, w["sections"].as_u64().unwrap(), "{file} {key} sections");
        let parsed: Vec<Vec<Value>> = features
            .iter()
            .map(|f| match &f.attributes[i] {
                FieldValue::Text(t) => serde_json::from_str::<Vec<Value>>(t).unwrap(),
                other => panic!("{other:?}"),
            })
            .collect();
        let counts: Vec<u8> = parsed.iter().flat_map(|rows| (rows.len() as i64).to_le_bytes()).collect();
        assert_eq!(sha256_hex(&counts), w["counts_sha256"].as_str().unwrap(), "{file} {key} rows per section");
        let rows: Vec<&Value> = parsed.iter().flatten().collect();
        let number = |v: &Value| v.as_f64().unwrap_or(f64::NAN);
        if let Some(sha) = w.get("values_sha256") {
            let bytes: Vec<u8> = rows.iter().flat_map(|r| r.as_array().unwrap().iter().flat_map(|v| number(v).to_le_bytes())).collect();
            assert_eq!(sha256_hex(&bytes), sha.as_str().unwrap(), "{file} {key} values");
        } else {
            for column in w["columns"].as_array().unwrap() {
                let (name, kind) = (column["name"].as_str().unwrap(), column["kind"].as_str().unwrap());
                let values: Vec<FieldValue> = rows
                    .iter()
                    .map(|r| match (kind, &r[name]) {
                        ("text", Value::String(s)) => FieldValue::Text(s.clone()),
                        ("int", v) => FieldValue::Integer(v.as_i64().unwrap()),
                        ("float", v) => FieldValue::Float(number(v)),
                        (k, v) => panic!("{file} {key} {name}: {k} {v:?}"),
                    })
                    .collect();
                let refs: Vec<_> = values.iter().collect();
                assert_eq!(values_digest(&refs, kind), column["sha256"].as_str().unwrap(), "{file} {key} {name}");
            }
        }
        checked += rows.len();
    }
    checked
}

/// The field name terrio-hecras gives a stored column (lower case, other runs as `_`).
fn terrio_field(stored: &str) -> String {
    let mut out = String::new();
    for c in stored.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}
