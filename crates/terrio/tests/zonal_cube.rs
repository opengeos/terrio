// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Zonal statistics as a cube indexed by the zones' geometries: cells equal the wide output's
//! columns, dimensions come from the inputs' times and labels.

#![cfg(not(target_family = "wasm"))]

use terrio::LazyRaster;
use terrio::ops::zonal::{Statistic, ZonalBand, ZonalOptions, ZonalRaster, ZonalSource, zonal_stats};
use terrio::ops::zonal_cube::{CubeValues, zonal_cube};
use terrio::vector::Coord;
use terrio::{DataType, Feature, FieldValue, Geometry, Layer, Raster, RasterConfig, RasterDataset};

/// Two zones on a 4 x 4 grid of 1 m pixels from (0, 0): the left half and the top-right quarter.
fn zones() -> Layer {
    let square = |x0: f64, y0: f64, x1: f64, y1: f64| {
        Geometry::polygon(vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)], vec![])
    };
    let mut zones = Layer::new("zones");
    zones.features.push(Feature { fid: 0, geometry: Some(square(0.0, 0.0, 2.0, 4.0)), attributes: vec![] });
    zones.features.push(Feature { fid: 1, geometry: Some(square(2.0, 2.0, 4.0, 4.0)), attributes: vec![] });
    zones
}

/// A 4 x 4 single-band raster of `scale * (1..=16)` over the zones.
fn raster(scale: f64) -> ZonalRaster {
    let cfg = RasterConfig { cols: 4, rows: 4, bands: 2, data_type: DataType::F64, ..Default::default() };
    let values: Vec<f64> = (1..=32).map(|v| scale * v as f64).collect();
    let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
    ds.set_geo_transform([0.0, 1.0, 0.0, 4.0, 0.0, -1.0]);
    ZonalRaster::Open(LazyRaster::from(ds))
}

fn labelled(scale: f64, time: Option<&str>, label: Option<&str>, prefix: &str) -> ZonalSource {
    let band = ZonalBand { band: 1, prefix: prefix.into(), time: time.map(Into::into), label: label.map(Into::into) };
    ZonalSource { raster: raster(scale), bands: vec![band] }
}

fn options() -> ZonalOptions {
    ZonalOptions { stats: vec![Statistic::Mean, Statistic::Count], ..Default::default() }
}

fn f64s(values: &CubeValues) -> Vec<f64> {
    match values {
        CubeValues::F64(v) => v.clone(),
        CubeValues::F32(v) => v.iter().map(|&x| x as f64).collect(),
    }
}

fn number(value: &FieldValue) -> f64 {
    match value {
        FieldValue::Float(x) => *x,
        FieldValue::Integer(i) => *i as f64,
        other => panic!("not a number: {other:?}"),
    }
}

#[test]
fn cube_cells_equal_wide_columns() {
    let sources = || {
        vec![
            labelled(1.0, Some("2024-07-01"), Some("B04"), "2024_07_01_B04"),
            labelled(2.0, Some("2024-07-01"), Some("B08"), "2024_07_01_B08"),
            labelled(3.0, Some("2024-07-11"), Some("B04"), "2024_07_11_B04"),
        ]
    };
    let cube = zonal_cube(&zones(), &mut sources(), &options(), "ndvi").unwrap();
    assert_eq!(cube.statistics, vec![Statistic::Mean, Statistic::Count]);
    let var = &cube.variables[0];
    assert_eq!(var.name, "ndvi");
    let dims: Vec<_> = var.dims.iter().map(|d| (d.name.as_str(), d.labels.clone())).collect();
    assert_eq!(dims, vec![("time", vec!["2024-07-01".to_string(), "2024-07-11".into()]), ("band", vec!["B04".to_string(), "B08".into()])]);
    assert_eq!(cube.shape(0), Some(vec![2, 2, 2, 2]));
    let values = f64s(&var.values);
    let wide = zonal_stats(&zones(), &mut sources(), &options()).unwrap();
    let column = |name: &str| wide.schema.fields().iter().position(|f| f.name == name).unwrap();
    for (t, time) in ["2024_07_01", "2024_07_11"].iter().enumerate() {
        for (b, band) in ["B04", "B08"].iter().enumerate() {
            for (s, stat) in ["mean", "count"].iter().enumerate() {
                for f in 0..2 {
                    let got = values[((f * 2 + t) * 2 + b) * 2 + s];
                    if (*time, *band) == ("2024_07_11", "B08") {
                        assert!(got.is_nan(), "missing cell {time} {band} {stat} {f}: {got}");
                    } else {
                        let expected = number(&wide.features[f].attributes[column(&format!("{time}_{band}_{stat}"))]);
                        assert_eq!(got, expected, "{time} {band} {stat} feature {f}");
                    }
                }
            }
        }
    }
}

#[test]
fn no_labels_no_time_gives_geometry_statistic() {
    let mut sources = vec![ZonalSource { raster: raster(1.0), bands: vec![ZonalBand::new(1, "dem_b1")] }];
    let cube = zonal_cube(&zones(), &mut sources, &options(), "dem").unwrap();
    assert!(cube.variables[0].dims.is_empty());
    assert_eq!(cube.shape(0), Some(vec![2, 2]));
}

#[test]
fn several_unlabelled_bands_give_a_band_dim() {
    let bands = vec![ZonalBand::new(1, "img_b1"), ZonalBand::new(2, "img_b2")];
    let mut sources = vec![ZonalSource { raster: raster(1.0), bands }];
    let cube = zonal_cube(&zones(), &mut sources, &options(), "img").unwrap();
    let dim = &cube.variables[0].dims[0];
    assert_eq!((dim.name.as_str(), dim.labels.clone()), ("band", vec!["img_b1".to_string(), "img_b2".into()]));
    let values = f64s(&cube.variables[0].values);
    assert_ne!(values[0], values[2], "band 2 differs from band 1"); // feature 0, mean of b1 vs b2
}

#[test]
fn time_on_some_inputs_only_is_an_error() {
    let mut sources = vec![labelled(1.0, Some("2024-07-01"), Some("B04"), "dated"), labelled(1.0, None, Some("B04"), "undated")];
    let err = zonal_cube(&zones(), &mut sources, &options(), "x").unwrap_err().to_string();
    assert!(err.contains("undated"), "{err}");
}

#[test]
fn same_cell_twice_is_an_error() {
    let mut sources =
        vec![labelled(1.0, Some("2024-07-01"), Some("B04"), "first"), labelled(2.0, Some("2024-07-01"), Some("B04"), "second")];
    let err = zonal_cube(&zones(), &mut sources, &options(), "x").unwrap_err().to_string();
    assert!(err.contains("first") && err.contains("second"), "{err}");
}

#[test]
fn weighted_and_output_options_are_refused() {
    let mut sources = vec![labelled(1.0, None, None, "a")];
    let rounded = ZonalOptions { decimal_places: Some(2), ..options() };
    assert!(zonal_cube(&zones(), &mut sources, &rounded, "x").is_err());
}

#[cfg(feature = "zarr")]
mod zarr {
    use terrio::crs::SpatialRef;
    use terrio::ops::zonal::Statistic;
    use terrio::ops::zonal_cube::{CubeDim, CubeValues, CubeVariable, ZonalCube};
    use terrio::raster::RasterData;
    use terrio::vector::Coord;
    use terrio::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};

    fn ring(points: &[(f64, f64)]) -> Vec<Coord> {
        points.iter().map(|&(x, y)| Coord::xy(x, y)).collect()
    }

    /// A clockwise square with an anticlockwise hole, and a two-part multipolygon, in EPSG:32631,
    /// with a text and an integer attribute (one null).
    fn zones() -> Layer {
        let mut zones = Layer::new("zones");
        zones.schema.add_field(FieldDef::new("name", FieldType::Text));
        zones.schema.add_field(FieldDef::new("code", FieldType::Integer));
        let square = ring(&[(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (0.0, 0.0)]); // clockwise
        let hole = ring(&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0), (2.0, 2.0)]); // anticlockwise
        let a = ring(&[(20.0, 0.0), (30.0, 0.0), (30.0, 10.0), (20.0, 0.0)]);
        let b = ring(&[(40.0, 0.0), (50.0, 0.0), (50.0, 10.0), (40.0, 0.0)]);
        let features = [
            (Geometry::polygon(square, vec![hole]), FieldValue::Text("north".into()), FieldValue::Integer(7)),
            (Geometry::multi_polygon(vec![(a, vec![]), (b, vec![])]), FieldValue::Text("south".into()), FieldValue::Null),
        ];
        for (fid, (geometry, name, code)) in features.into_iter().enumerate() {
            zones.features.push(Feature { fid: 10 + fid as u64, geometry: Some(geometry), attributes: vec![name, code] });
        }
        SpatialRef::parse("EPSG:32631").unwrap().apply_to_layer(&mut zones);
        zones
    }

    fn cube(zones: Layer) -> ZonalCube {
        let dims = vec![
            CubeDim { name: "time".into(), labels: vec!["2024-07-01".into(), "2024-07-11".into()] },
            CubeDim { name: "band".into(), labels: vec!["B04".into(), "B08".into()] },
        ];
        let values: Vec<f64> = (0..2 * 2 * 2 * 2).map(|v| v as f64).collect();
        let variables = vec![CubeVariable { name: "ndvi".into(), dims, values: CubeValues::F64(values) }];
        ZonalCube { zones, statistics: vec![Statistic::Mean, Statistic::Count], variables }
    }

    /// The values as f64 and the type's name.
    fn nums(data: RasterData) -> (&'static str, Vec<f64>) {
        match data {
            RasterData::I32(v) => ("i32", v.into_iter().map(f64::from).collect()),
            RasterData::I64(v) => ("i64", v.into_iter().map(|x| x as f64).collect()),
            RasterData::F32(v) => ("f32", v.into_iter().map(f64::from).collect()),
            RasterData::F64(v) => ("f64", v),
            other => panic!("unexpected type {other:?}"),
        }
    }

    fn json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path.join("zarr.json")).unwrap()).unwrap()
    }

    fn read(store: &str, array: &str) -> (Vec<String>, RasterData) {
        let ds = terrio::multidim::open(store).unwrap();
        let a = ds.array(array).unwrap();
        let dims = a.info.dimensions.iter().map(|d| d.name.clone()).collect();
        (dims, a.read().unwrap().data().unwrap())
    }

    /// Strings of a `vlen-utf8` array written without compression.
    fn strings(store: &std::path::Path, array: &str) -> Vec<String> {
        let bytes = std::fs::read(store.join(array).join("c/0")).unwrap();
        let count = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let mut at = 4;
        (0..count)
            .map(|_| {
                let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                at += 4 + n;
                String::from_utf8(bytes[at - n..at].to_vec()).unwrap()
            })
            .collect()
    }

    #[test]
    fn writes_a_cf_cube() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        let store = path.to_str().unwrap();
        cube(zones()).write_zarr(store).unwrap();

        let group = json(&path);
        let attrs = &group["attributes"];
        assert_eq!(attrs["Conventions"], "CF-1.8");
        assert_eq!(attrs["proj:code"], "EPSG:32631");
        assert_eq!(attrs["spatial:bbox"], serde_json::json!([0.0, 0.0, 50.0, 10.0]));
        assert!(!group.to_string().contains("vector"), "{group}");
        let container = json(&path.join("geometry_container"));
        assert_eq!(container["attributes"]["geometry_type"], "polygon");
        assert_eq!(container["attributes"]["node_coordinates"], "geometry_x geometry_y");
        assert!(json(&path.join("spatial_ref"))["attributes"]["crs_wkt"].as_str().unwrap().contains("31N"));

        let (dims, values) = read(store, "ndvi");
        assert_eq!(dims, ["geometry", "time", "band", "statistic"]);
        assert_eq!(nums(values), ("f64", (0..16).map(|v| v as f64).collect()));
        let var = json(&path.join("ndvi"));
        assert_eq!(var["attributes"]["geometry"], "geometry_container");
        assert_eq!(var["attributes"]["grid_mapping"], "spatial_ref");

        assert_eq!(nums(read(store, "geometry_node_count").1), ("i32", vec![10.0, 8.0]));
        assert_eq!(nums(read(store, "geometry_part_node_count").1), ("i32", vec![5.0, 5.0, 4.0, 4.0]));
        assert_eq!(nums(read(store, "geometry_interior_ring").1), ("i32", vec![0.0, 1.0, 0.0, 0.0]));
        assert_eq!(nums(read(store, "feature_id").1), ("i64", vec![10.0, 11.0]));
        match read(store, "code").1 {
            RasterData::I64(code) => assert_eq!(code, [7, i64::MIN], "an integer field with nulls"),
            other => panic!("code is {other:?}"),
        }
        assert_eq!(json(&path.join("code"))["fill_value"], i64::MIN);
        assert_eq!(json(&path.join("code"))["attributes"]["_FillValue"], i64::MIN, "CF readers mask by it");
        assert_eq!(strings(&path, "name"), ["north", "south"]);
        assert_eq!(strings(&path, "statistic"), ["mean", "count"]);
        assert_eq!(strings(&path, "band"), ["B04", "B08"]);
        // 2024-07-01 and 2024-07-11 as CF times
        let time = json(&path.join("time"));
        assert_eq!(time["attributes"]["units"], "seconds since 1970-01-01T00:00:00");
        assert_eq!(nums(read(store, "time").1), ("i64", vec![1_719_792_000.0, 1_720_656_000.0]));
    }

    #[test]
    fn exterior_rings_anticlockwise_holes_clockwise() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("c.zarr");
        cube(zones()).write_zarr(store.to_str().unwrap()).unwrap();
        let (_, x) = nums(read(store.to_str().unwrap(), "geometry_x").1);
        let (_, y) = nums(read(store.to_str().unwrap(), "geometry_y").1);
        let area = |from: usize, to: usize| (from..to - 1).map(|i| x[i] * y[i + 1] - x[i + 1] * y[i]).sum::<f64>() / 2.0;
        assert!(area(0, 5) > 0.0, "exterior anticlockwise");
        assert!(area(5, 10) < 0.0, "hole clockwise");
        assert!(area(10, 14) > 0.0 && area(14, 18) > 0.0, "parts anticlockwise");
    }

    #[test]
    fn labels_that_are_not_dates_stay_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        let mut cube = cube(zones());
        cube.variables[0].dims[0].labels = vec!["spring".into(), "summer".into()];
        cube.write_zarr(path.to_str().unwrap()).unwrap();
        assert_eq!(strings(&path, "time"), ["spring", "summer"]);
    }

    #[test]
    fn null_geometry_is_an_error() {
        let mut zones = zones();
        zones.features[1].geometry = None;
        let dir = tempfile::tempdir().unwrap();
        let err = cube(zones).write_zarr(dir.path().join("c.zarr").to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("feature 1") && err.contains("11"), "{err}");
    }

    /// Rings as terrio's readers give them: open (no closing vertex), far from the origin,
    /// starting anywhere.
    #[test]
    fn open_rings_far_from_the_origin_are_closed_and_oriented() {
        let (x0, y0) = (500_000.0, 5_000_000.0);
        let exterior = ring(&[(x0, y0 + 10.0), (x0, y0), (x0 + 10.0, y0), (x0 + 10.0, y0 + 10.0)]); // anticlockwise, open
        let hole = ring(&[(x0 + 2.0, y0 + 2.0), (x0 + 4.0, y0 + 2.0), (x0 + 4.0, y0 + 4.0), (x0 + 2.0, y0 + 4.0)]); // anticlockwise, open
        let mut zones = Layer::new("zones");
        zones.features.push(Feature { fid: 0, geometry: Some(Geometry::polygon(exterior, vec![hole])), attributes: vec![] });
        SpatialRef::parse("EPSG:32631").unwrap().apply_to_layer(&mut zones);
        let mut cube = cube(zones);
        cube.variables[0].values = CubeValues::F64(vec![0.0; 8]);
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("c.zarr");
        cube.write_zarr(store.to_str().unwrap()).unwrap();
        let (_, x) = nums(read(store.to_str().unwrap(), "geometry_x").1);
        let (_, y) = nums(read(store.to_str().unwrap(), "geometry_y").1);
        assert_eq!(nums(read(store.to_str().unwrap(), "geometry_part_node_count").1).1, [5.0, 5.0], "rings closed");
        assert_eq!((x[0], y[0]), (x[4], y[4]));
        let area = |from: usize, to: usize| {
            (from..to - 1).map(|i| (x[i] - x0) * (y[i + 1] - y0) - (x[i + 1] - x0) * (y[i] - y0)).sum::<f64>() / 2.0
        };
        assert_eq!(area(0, 5), 100.0, "exterior anticlockwise");
        assert_eq!(area(5, 10), -4.0, "hole clockwise");
    }

    #[test]
    fn date_time_fields_are_written_as_their_text() {
        let mut zones = zones();
        zones.schema.add_field(FieldDef::new("seen", FieldType::DateTime));
        for f in &mut zones.features {
            f.attributes.push(FieldValue::DateTime("2024-07-01T10:00:00".into()));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        cube(zones).write_zarr(path.to_str().unwrap()).unwrap();
        assert_eq!(strings(&path, "seen"), ["2024-07-01T10:00:00", "2024-07-01T10:00:00"]);
    }

    /// Names that would be paths outside an array, another array, or one of its dimensions get
    /// a name of their own.
    #[test]
    fn names_never_clash_or_leave_their_array() {
        let mut zones = zones();
        for name in ["", "a/b", "geometry_node", "Time", "Land Use", "ZARR.json", "__meta", "..", "ends."] {
            zones.schema.add_field(FieldDef::new(name, FieldType::Text));
            for f in &mut zones.features {
                f.attributes.push(FieldValue::Text("x".into()));
            }
        }
        let mut cube = cube(zones);
        cube.variables[0].name = "band".into();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        cube.write_zarr(path.to_str().unwrap()).unwrap();
        assert_eq!(json(&path)["attributes"]["Conventions"], "CF-1.8", "the group's metadata is intact");
        let mut names: Vec<String> = std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        for expected in
            ["band_2", "field", "a_b", "geometry_node_2", "Time_2", "Land_Use", "ZARR.json_2", "field__meta", "field__", "ends_"]
        {
            assert!(names.contains(&expected.to_string()), "{expected} not in {names:?}");
        }
        assert_eq!(strings(&path, "band"), ["B04", "B08"], "the band coordinate is untouched");
        let coordinates = json(&path.join("band_2"))["attributes"]["coordinates"].as_str().unwrap().to_string();
        assert!(coordinates.split_whitespace().any(|c| c == "Land_Use"), "{coordinates}");
    }

    /// Integers past 2^53 keep every digit next to nulls; `i64::MIN`, the nulls' marker, is refused
    /// among nulls before anything is written.
    #[test]
    fn integer_fields_with_nulls_are_lossless() {
        let mut zones = zones();
        zones.features[0].attributes[1] = FieldValue::Integer(i64::MAX);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        cube(zones.clone()).write_zarr(path.to_str().unwrap()).unwrap();
        match read(path.to_str().unwrap(), "code").1 {
            RasterData::I64(code) => assert_eq!(code, [i64::MAX, i64::MIN]),
            other => panic!("code is {other:?}"),
        }
        // `i64::MIN` without nulls is a value like any other: no fill value.
        zones.features[0].attributes[1] = FieldValue::Integer(i64::MIN);
        zones.features[1].attributes[1] = FieldValue::Integer(1);
        let path = dir.path().join("full.zarr");
        cube(zones.clone()).write_zarr(path.to_str().unwrap()).unwrap();
        assert_eq!(json(&path.join("code"))["fill_value"], 0, "no nulls: the writers' default fill");
        // An all-null field: every value is the fill value.
        for f in &mut zones.features {
            f.attributes[1] = FieldValue::Null;
        }
        let path = dir.path().join("nulls.zarr");
        cube(zones.clone()).write_zarr(path.to_str().unwrap()).unwrap();
        assert_eq!(json(&path.join("code"))["fill_value"], i64::MIN);
        match read(path.to_str().unwrap(), "code").1 {
            RasterData::I64(code) => assert_eq!(code, [i64::MIN, i64::MIN]),
            other => panic!("code is {other:?}"),
        }
        zones.features[0].attributes[1] = FieldValue::Integer(i64::MIN);
        let path = dir.path().join("min.zarr");
        let err = cube(zones).write_zarr(path.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("'code'"), "{err}");
        assert!(!path.exists(), "nothing written");
    }

    /// An integer field holding a float, as some readers give, keeps every value as float64.
    #[test]
    fn integer_fields_holding_floats_stay_float64() {
        let mut zones = zones();
        zones.features[1].attributes[1] = FieldValue::Float(2.5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        cube(zones).write_zarr(path.to_str().unwrap()).unwrap();
        assert_eq!(nums(read(path.to_str().unwrap(), "code").1), ("f64", vec![7.0, 2.5]));
    }

    /// A field's type, not its values, decides its array type: an all-null float field is float64.
    #[test]
    fn all_null_number_fields_stay_numbers() {
        let mut zones = zones();
        zones.schema.add_field(FieldDef::new("area", FieldType::Float));
        for f in &mut zones.features {
            f.attributes.push(FieldValue::Null);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        cube(zones).write_zarr(path.to_str().unwrap()).unwrap();
        let (kind, values) = nums(read(path.to_str().unwrap(), "area").1);
        assert_eq!(kind, "f64");
        assert!(values.iter().all(|v| v.is_nan()));
    }

    #[test]
    fn values_of_the_wrong_length_are_an_error_before_writing() {
        let mut cube = cube(zones());
        cube.variables[0].values = CubeValues::F64(vec![0.0; 3]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zarr");
        let err = cube.write_zarr(path.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("ndvi") && err.contains("16"), "{err}");
        assert!(!path.exists(), "nothing written");
        assert_eq!(cube.shape(1), None);
        assert_eq!(cube.shape(0), Some(vec![2, 2, 2, 2]));
    }

    #[test]
    fn existing_output_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("c.zarr");
        std::fs::create_dir(&store).unwrap();
        assert!(cube(zones()).write_zarr(store.to_str().unwrap()).is_err());
    }
}
