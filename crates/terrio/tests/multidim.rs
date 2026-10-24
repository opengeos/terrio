use serde_json::json;
use terrio::{
    DataType,
    multidim::{Array, ArrayInfo, Dataset, Dimension, Group},
};

fn dataset() -> Dataset {
    let mut root = Group::new("/");
    root.attributes = json!({"title":"multidimensional test","Conventions":"CF-1.8"});
    for (name, size, unlimited) in [("time", 3, true), ("y", 2, false), ("x", 4, false)] {
        root.dimensions.push(Dimension {
            name: name.into(),
            path: format!("/{name}"),
            size,
            unlimited,
            indexing_variable: Some(format!("/{name}")),
        });
    }
    for d in &root.dimensions {
        let values: Vec<i64> = (0..d.size).map(|n| n as i64).collect();
        root.arrays.push(
            Array::from_bytes(
                ArrayInfo {
                    name: d.name.clone(),
                    path: d.path.clone(),
                    data_type: DataType::I64,
                    dimensions: vec![d.clone()],
                    chunks: Some(vec![1]),
                    attributes: if d.name == "time" { json!({"units":"hours since 2000-01-01"}) } else { json!({}) },
                    coordinates: vec![d.path.clone()],
                },
                values.iter().flat_map(|n| n.to_le_bytes()).collect(),
            )
            .unwrap(),
        );
    }
    let data: Array = Array::from_bytes(
        ArrayInfo {
            name: "temperature".into(),
            path: "/temperature".into(),
            data_type: DataType::I64,
            dimensions: root.dimensions.clone(),
            chunks: Some(vec![1, 2, 2]),
            attributes: json!({"units":"K","_FillValue":-9999}),
            coordinates: root.dimensions.iter().map(|d| d.path.clone()).collect(),
        },
        (0i64..24).flat_map(i64::to_le_bytes).collect(),
    )
    .unwrap();
    root.arrays.push(data);
    let mut child = Group::new("/science");
    child.attributes = json!({"instrument":"test"});
    let mut array = root.arrays.last().unwrap().clone();
    array.info.path = "/science/temperature".into();
    child.arrays.push(array);
    root.groups.push(child);
    Dataset { driver: "Memory".into(), root }
}
#[test]
fn native_typed_lazy_views_slice_index_and_transpose() {
    let ds = dataset();
    let a = ds.array("temperature").unwrap();
    let view = a.view().isel("time", 2).unwrap().slice("x", 1, 2, 2).unwrap().transpose(&[1, 0]).unwrap();
    assert_eq!(view.shape(), [2, 2]);
    assert_eq!(view.dimensions().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["x", "y"]);
    let b = view.read().unwrap();
    assert_eq!(b.values().unwrap(), json!([17, 21, 19, 23]));
    assert_eq!(a.view().shape(), [3, 2, 4]);
    let empty = a.view().slice("x", 4, 0, 1).unwrap().read().unwrap();
    assert!(empty.bytes.is_empty());
    assert_eq!(empty.shape, [3, 2, 0]);
    assert!(a.view().slice("x", 0, 1, 0).is_err());
    assert!(a.view().isel("time", 3).is_err());
    assert!(a.view().transpose(&[0, 0, 2]).is_err());
    assert!(ds.array("missing").is_err());
}
#[test]
fn scalar_and_large_integer_values_are_exact() {
    let a = Array::from_bytes(
        ArrayInfo {
            name: "n".into(),
            path: "/n".into(),
            data_type: DataType::U64,
            dimensions: vec![],
            chunks: None,
            attributes: json!({}),
            coordinates: vec![],
        },
        u64::MAX.to_le_bytes().to_vec(),
    )
    .unwrap();
    assert_eq!(a.read().unwrap().values().unwrap(), json!([u64::MAX]));
    assert!(a.read().unwrap().shape.is_empty());
}
#[cfg(all(feature = "netcdf-hdf5", feature = "zarr"))]
#[test]
fn netcdf_zarr_round_trip_preserves_groups_coordinates_chunks_and_unlimited_axes() {
    let dir = tempfile::tempdir().unwrap();
    let nc = dir.path().join("source.nc");
    let zarr = dir.path().join("cube.zarr");
    let back = dir.path().join("back.nc");
    let original = dataset();
    original.write(nc.to_str().unwrap()).unwrap();
    let n = terrio::multidim::open(nc.to_str().unwrap()).unwrap();
    assert_eq!(n.array("temperature").unwrap().shape(), [3, 2, 4]);
    assert_eq!(n.array("temperature").unwrap().info.chunks, Some(vec![1, 2, 2]));
    assert!(n.root.dimensions.iter().any(|d| d.name == "time" && d.unlimited));
    assert_eq!(n.array("science/temperature").unwrap().info.dimensions[0].path, "/time");
    n.write(zarr.to_str().unwrap()).unwrap();
    let z = terrio::multidim::open(zarr.to_str().unwrap()).unwrap();
    assert!(z.root.dimensions.iter().any(|d| d.name == "time" && d.unlimited));
    assert_eq!(z.array("science/temperature").unwrap().info.dimensions[0].path, "/time");
    assert_eq!(z.array("temperature").unwrap().read().unwrap().bytes, original.array("temperature").unwrap().read().unwrap().bytes);
    z.write(back.to_str().unwrap()).unwrap();
    let b = terrio::multidim::open(back.to_str().unwrap()).unwrap();
    assert_eq!(b.array("temperature").unwrap().info.chunks, Some(vec![1, 2, 2]));
    assert!(b.root.dimensions.iter().any(|d| d.name == "time" && d.unlimited));
    for a in original.root.arrays_recursive() {
        assert_eq!(b.array(&a.info.path).unwrap().read().unwrap().bytes, a.read().unwrap().bytes);
    }
    let bytes = std::fs::read(&nc).unwrap();
    let memory = terrio::multidim::open_bytes(&bytes).unwrap();
    let mut userblock = vec![0; 512];
    userblock.extend_from_slice(&bytes);
    let prefixed = terrio::multidim::open_bytes(&userblock).unwrap();
    drop(userblock);
    assert_eq!(prefixed.array("temperature").unwrap().read().unwrap().bytes, memory.array("temperature").unwrap().read().unwrap().bytes);
    drop(bytes);
    assert_eq!(
        memory.array("temperature").unwrap().view().isel("time", 1).unwrap().read().unwrap().values().unwrap(),
        json!((8..16).collect::<Vec<_>>())
    );
    assert!(original.write(nc.to_str().unwrap()).is_err());
}

#[cfg(feature = "zarr")]
#[test]
fn in_memory_store_prefixes_and_array_lifetimes() {
    let original = dataset();
    let files = original.to_zarr_store().unwrap().into_iter().map(|(name, bytes)| (format!("outer/cube.zarr/{name}"), bytes)).collect();
    let opened = terrio::multidim::open_zarr_store(files).unwrap();
    let handle = opened.array("temperature").unwrap();
    drop(opened);
    assert_eq!(handle.view().isel("time", 1).unwrap().read().unwrap().values().unwrap(), json!((8..16).collect::<Vec<_>>()));
}
#[test]
fn snapshots_reject_invalid_or_duplicate_paths() {
    let mut original = dataset();
    let first = original.root.arrays[0].clone();
    original.root.arrays.push(first);
    assert!(original.to_netcdf_bytes().unwrap_err().to_string().contains("unique"));
    let mut original = dataset();
    original.root.arrays[0].info.path = "/../escape".into();
    assert!(original.to_netcdf_bytes().is_err());
}

#[cfg(feature = "netcdf-hdf5")]
#[test]
fn netcdf_clamps_fixed_axis_chunks_and_preserves_unlimited_chunks() {
    let mut original = dataset();
    original.root.arrays[0].info.chunks = Some(vec![256]);
    original.root.arrays[1].info.chunks = Some(vec![256]);
    original.root.arrays[2].info.chunks = Some(vec![256]);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("chunks.nc");
    original.write(file.to_str().unwrap()).unwrap();
    let opened = terrio::multidim::open(file.to_str().unwrap()).unwrap();
    assert_eq!(opened.array("time").unwrap().info.chunks, Some(vec![256]));
    assert_eq!(opened.array("y").unwrap().info.chunks, Some(vec![2]));
    assert_eq!(opened.array("x").unwrap().info.chunks, Some(vec![4]));
    for name in ["time", "y", "x"] {
        assert_eq!(opened.array(name).unwrap().read().unwrap().bytes, original.array(name).unwrap().read().unwrap().bytes);
    }
}

#[cfg(feature = "zarr")]
#[test]
fn failed_snapshot_leaves_no_destination_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.zarr");
    std::fs::create_dir_all(source.join("a")).unwrap();
    std::fs::write(source.join(".zgroup"), br#"{"zarr_format":2}"#).unwrap();
    std::fs::write(
        source.join("a/.zarray"),
        br#"{"zarr_format":2,"shape":[2],"chunks":[2],"dtype":"<i8","compressor":null,"fill_value":0,"order":"C","filters":null}"#,
    )
    .unwrap();
    std::fs::write(source.join("a/0"), [1]).unwrap();
    let broken = terrio::multidim::open(source.to_str().unwrap()).unwrap();
    let destination = dir.path().join("copy.zarr");
    assert!(broken.write(destination.to_str().unwrap()).is_err());
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".terrio-md-")));
    let bytes: Vec<_> = [7i64, 8].into_iter().flat_map(i64::to_le_bytes).collect();
    std::fs::write(source.join("a/0"), &bytes).unwrap();
    let fixed = terrio::multidim::open(source.to_str().unwrap()).unwrap();
    fixed.write(destination.to_str().unwrap()).unwrap();
    let copy = terrio::multidim::open(destination.to_str().unwrap()).unwrap();
    assert_eq!(copy.array("a").unwrap().read().unwrap().bytes, bytes);
    let before = std::fs::read(destination.join("a/0")).unwrap();
    assert!(fixed.write(destination.to_str().unwrap()).is_err());
    assert_eq!(std::fs::read(destination.join("a/0")).unwrap(), before);
}

#[test]
fn repeated_dimension_axes_have_positional_selections() {
    let mut ds = dataset();
    let x = ds.root.dimensions.iter().find(|d| d.name == "x").unwrap().clone();
    let a = Array::from_bytes(
        ArrayInfo {
            name: "covariance".into(),
            path: "/covariance".into(),
            data_type: DataType::I64,
            dimensions: vec![x.clone(), x],
            chunks: Some(vec![2, 2]),
            attributes: json!({}),
            coordinates: vec!["/x".into()],
        },
        (0i64..16).flat_map(i64::to_le_bytes).collect(),
    )
    .unwrap();
    assert!(a.view().isel("x", 1).is_err());
    assert_eq!(a.view().isel_axis(0, 1).unwrap().read().unwrap().values().unwrap(), json!([4, 5, 6, 7]));
    assert_eq!(a.view().slice_axis(1, 1, 2, 1).unwrap().read().unwrap().values().unwrap(), json!([1, 2, 5, 6, 9, 10, 13, 14]));
    assert!(a.view().isel_axis(2, 0).is_err());
    ds.root.arrays.push(a);
    #[cfg(feature = "zarr")]
    {
        let reopened = terrio::multidim::open_zarr_store(ds.to_zarr_store().unwrap()).unwrap();
        assert_eq!(reopened.array("covariance").unwrap().read().unwrap().values().unwrap(), json!((0..16).collect::<Vec<_>>()));
        #[cfg(feature = "netcdf-hdf5")]
        assert_eq!(
            terrio::multidim::open_bytes(&reopened.to_netcdf_bytes().unwrap()).unwrap().array("covariance").unwrap().shape(),
            [4, 4]
        );
    }
}

#[test]
fn snapshots_reject_invalid_fill_values_and_coordinate_links() {
    for fill in [json!("missing"), json!(null), json!([]), json!({})] {
        let mut ds = dataset();
        ds.root.arrays[0].info.attributes["_FillValue"] = fill;
        assert!(ds.to_netcdf_bytes().unwrap_err().to_string().contains("_FillValue"));
        #[cfg(feature = "zarr")]
        assert!(ds.to_zarr_store().unwrap_err().to_string().contains("_FillValue"));
    }
    let mut ds = dataset();
    let path = ds.root.dimensions[0].path.clone();
    fn invalid_coordinate(group: &mut Group, path: &str) {
        for d in group.dimensions.iter_mut().chain(group.arrays.iter_mut().flat_map(|a| &mut a.info.dimensions)) {
            if d.path == path {
                d.indexing_variable = Some("/x".into());
            }
        }
        for child in &mut group.groups {
            invalid_coordinate(child, path);
        }
    }
    invalid_coordinate(&mut ds.root, &path);
    assert!(ds.to_netcdf_bytes().unwrap_err().to_string().contains("indexing variable"));
}

#[cfg(feature = "zarr")]
#[test]
fn sparse_zarr_strides_skip_corrupt_intervening_chunks() {
    let files=vec![(".zgroup".into(),br#"{"zarr_format":2}"#.to_vec()),
        ("a/.zarray".into(),br#"{"zarr_format":2,"shape":[1000001],"chunks":[1],"dtype":"<u2","compressor":null,"fill_value":0,"order":"C","filters":null}"#.to_vec()),
        ("a/.zattrs".into(),br#"{"_ARRAY_DIMENSIONS":["x"]}"#.to_vec()),
        ("a/0".into(),7u16.to_le_bytes().to_vec()),("a/1000000".into(),9u16.to_le_bytes().to_vec()),("a/500000".into(),vec![1])];
    let ds = terrio::multidim::open_zarr_store(files).unwrap();
    let view = ds.array("a").unwrap().view().slice("x", 0, 2, 1000000).unwrap();
    assert_eq!(view.read().unwrap().values().unwrap(), json!([7, 9]));
}

#[cfg(feature = "zarr")]
#[test]
fn zarr_saved_dimensions_and_store_roots_are_unambiguous() {
    let files = dataset().to_zarr_store().unwrap();
    let mut stale = files.clone();
    stale.retain(|(name, _)| name != ".zmetadata");
    let attrs = stale.iter_mut().find(|(name, _)| name == ".zattrs").unwrap();
    let mut metadata: serde_json::Value = serde_json::from_slice(&attrs.1).unwrap();
    metadata["_terrio_dimensions"][0]["size"] = json!(999);
    attrs.1 = serde_json::to_vec(&metadata).unwrap();
    assert!(terrio::multidim::open_zarr_store(stale).is_err());
    let two: Vec<_> =
        files.iter().map(|(n, b)| (format!("a/{n}"), b.clone())).chain(files.iter().map(|(n, b)| (format!("bb/{n}"), b.clone()))).collect();
    assert!(terrio::multidim::open_zarr_store(two.clone()).unwrap_err().to_string().contains("multiple Zarr store roots"));
    assert!(terrio::multidim::open_zarr_store(two.into_iter().rev().collect()).is_err());
    let root = vec![(".zgroup".into(), br#"{"zarr_format":2}"#.to_vec()), ("a/.zgroup".into(), br#"{"zarr_format":2}"#.to_vec())];
    assert!(terrio::multidim::open_zarr_store(root).is_ok());
}

#[cfg(feature = "zarr")]
#[test]
fn selected_zarr_subgroups_keep_external_dimension_references() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cube.zarr");
    let original = dataset();
    original.write(path.to_str().unwrap()).unwrap();
    let expected = original.array("science/temperature").unwrap().read().unwrap().bytes;
    let group = terrio::multidim::open(&format!("ZARR:\"{}\":/science", path.display())).unwrap();
    assert_eq!(group.array("temperature").unwrap().read().unwrap().bytes, expected);
    let array = terrio::multidim::open(&format!("ZARR:\"{}\":/science/temperature", path.display())).unwrap();
    assert_eq!(array.array("array").unwrap().read().unwrap().bytes, expected);
}

#[test]
fn snapshots_are_reproducible_and_preserve_existing_destinations() {
    let ds = dataset();
    assert_eq!(ds.to_netcdf_bytes().unwrap(), ds.to_netcdf_bytes().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.nc");
    std::fs::write(&path, b"existing content").unwrap();
    assert!(ds.write(path.to_str().unwrap()).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"existing content");
}

#[cfg(all(feature = "netcdf-hdf5", feature = "zarr"))]
#[test]
fn snapshots_reject_conflicting_dimension_declarations() {
    for unlimited in [false, true] {
        let mut source = dataset();
        if unlimited {
            source.root.dimensions[0].unlimited = !source.root.dimensions[0].unlimited;
        } else {
            source.root.dimensions[0].size += 1;
        }
        assert!(source.to_netcdf_bytes().unwrap_err().to_string().contains("conflicting declarations for dimension"));
        assert!(source.to_zarr_store().unwrap_err().to_string().contains("conflicting declarations for dimension"));
    }
}
