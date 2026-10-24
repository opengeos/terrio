use std::collections::HashMap;
use terrio::catalog::{CatalogOptions, RasterCatalog};
use terrio::raster::RasterData;
use terrio::vector::{Coord, Ring};
use terrio::{DataType, Raster, RasterConfig, RasterDataset, Window};
use terrio::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};
fn tile(x: f64, value: f64) -> RasterDataset {
    let mut ds = RasterDataset::new(
        Raster::from_data_native(
            RasterConfig {
                cols: 4,
                rows: 4,
                bands: 1,
                data_type: DataType::F64,
                crs: terrio::crs::SpatialRef::from_epsg(3857).unwrap().to_crs_info(),
                ..Default::default()
            },
            RasterData::F64(vec![value; 16]),
        )
        .unwrap(),
        false,
    );
    ds.set_geo_transform([x, 1.0, 0.0, 4.0, 0.0, -1.0]);
    ds
}
fn index() -> Layer {
    let mut layer = Layer::new("tiles");
    layer.set_crs_epsg(Some(3857));
    layer.add_field(FieldDef::new("location", FieldType::Text));
    layer.add_field(FieldDef::new("z", FieldType::Integer));
    for (id, path, x, z) in [(0, "left.tif", 0.0, 2), (1, "right.tif", 2.0, 1), (2, "missing.tif", 100.0, 3)] {
        let geometry = Geometry::Polygon {
            exterior: Ring(vec![Coord::xy(x, 0.0), Coord::xy(x + 4.0, 0.0), Coord::xy(x + 4.0, 4.0), Coord::xy(x, 4.0), Coord::xy(x, 0.0)]),
            interiors: Vec::new(),
        };
        let mut feature = Feature::with_geometry(id, geometry, 2);
        feature.attributes = vec![FieldValue::Text(path.into()), FieldValue::Integer(z)];
        layer.push(feature);
    }
    layer
}
fn sources() -> HashMap<String, RasterDataset> {
    HashMap::from([("left.tif".into(), tile(0.0, 10.0)), ("right.tif".into(), tile(2.0, 20.0))])
}
fn options() -> CatalogOptions {
    CatalogOptions {
        resolution: Some((1.0, 1.0)),
        extent: Some((0.0, 0.0, 6.0, 4.0)),
        bands: Some(1),
        data_type: Some(DataType::F64),
        cache_size: Some(1),
        ..Default::default()
    }
}
#[test]
fn window_selection_and_bounded_cache() {
    let mut catalog = RasterCatalog::from_layer(index(), sources(), options()).unwrap();
    assert_eq!(catalog.stats().source_opens, 0);
    for (x, value) in [(0, 10.0), (5, 20.0), (0, 10.0)] {
        let result = catalog.read_window(Window::new(x, 0, 1, 1), &[]).unwrap();
        assert_eq!(result.raster.band_view(0).as_slice(), &[value]);
    }
    assert_eq!(catalog.stats().source_opens, 3);
    assert_eq!(catalog.stats().selected_features, 3);
    assert_eq!(catalog.stats().peak_cached_sources, 1);
}
#[test]
fn z_order_and_nodata_transparency() {
    let mut input = sources();
    let left = input.get_mut("left.tif").unwrap();
    left.has_nodata = true;
    left.raster.nodata = -9999.0;
    if let RasterData::F64(v) = &mut left.raster.data {
        v[3] = -9999.0;
    }
    let mut catalog = RasterCatalog::from_layer(index(), input, CatalogOptions { sort_field: Some("z".into()), ..options() }).unwrap();
    let ds = catalog.read_window(Window::new(0, 0, 6, 4), &[]).unwrap();
    assert_eq!(&ds.raster.band_view(0).as_slice()[..6], &[10.0, 10.0, 10.0, 20.0, 20.0, 20.0]);
    assert_eq!(catalog.stats().warped_pixels, 32);
    assert!(catalog.read_mask(Window::new(0, 0, 6, 4)).unwrap().iter().all(|v| *v == 255));
}
#[test]
fn untagged_extra_band_does_not_mask_overlapping_tiles() {
    let mut input = sources();
    let right = input.get_mut("right.tif").unwrap();
    right.raster.bands = 2;
    let mut values = vec![20.0; 16];
    values.extend([0.0; 16]);
    right.raster.data = RasterData::F64(values);
    let mut catalog = RasterCatalog::from_layer(index(), input, options()).unwrap();
    let result = catalog.read_window(Window::new(0, 0, 6, 4), &[]).unwrap();
    assert_eq!(&result.raster.band_view(0).as_slice()[..6], &[10.0, 10.0, 20.0, 20.0, 20.0, 20.0]);
    assert_eq!(&catalog.read_mask(Window::new(0, 0, 6, 4)).unwrap()[..6], &[255; 6]);
    let mut lazy = catalog.into_lazy();
    assert!(lazy.has_mask());
    assert_eq!(&lazy.read_mask(Window::new(0, 0, 6, 4), None).unwrap().unwrap()[..6], &[255; 6]);
}

#[test]
fn floating_infinity_is_data_when_not_declared_nodata() {
    let mut inputs = sources();
    if let RasterData::F64(v) = &mut inputs.get_mut("left.tif").unwrap().raster.data {
        v[0] = f64::INFINITY;
    }
    let mut catalog = RasterCatalog::from_layer(index(), inputs, options()).unwrap();
    let window = Window::new(0, 0, 1, 1);
    assert_eq!(catalog.read_window(window, &[]).unwrap().raster.band_view(0).as_slice(), &[f64::INFINITY]);
    assert_eq!(catalog.read_mask(window).unwrap(), vec![255]);
}

#[cfg(any(feature = "sqlite", feature = "flatgeobuf"))]
#[test]
fn file_index_reader_is_reused_across_windows() {
    let directory = tempfile::tempdir().unwrap();
    for (name, dataset) in sources() {
        terrio::write_raster_dataset(&dataset, directory.path().join(name).to_str().unwrap(), &Default::default()).unwrap();
    }
    let mut extensions = Vec::new();
    #[cfg(feature = "sqlite")]
    extensions.push("gpkg");
    #[cfg(feature = "flatgeobuf")]
    extensions.push("fgb");
    for extension in extensions {
        let path = directory.path().join(format!("tiles.gti.{extension}"));
        terrio::write_vector(&[index()], path.to_str().unwrap(), &Default::default()).unwrap();
        let mut catalog = RasterCatalog::open(path.to_str().unwrap(), options()).unwrap();
        assert_eq!(catalog.stats().index_opens, 1);
        // On Unix an open file remains readable after rename. A per-window reopen
        // of the original path fails, so this checks the persistent handle too.
        #[cfg(unix)]
        std::fs::rename(&path, path.with_extension(format!("{extension}.moved"))).unwrap();
        for (x, value) in [(0, 10.0), (5, 20.0), (0, 10.0)] {
            let result = catalog.read_window(Window::new(x, 0, 1, 1), &[]).unwrap();
            assert_eq!(result.raster.band_view(0).as_slice(), &[value]);
        }
        assert_eq!(catalog.stats().index_opens, 1);
        assert_eq!(catalog.stats().index_queries, 3);
        assert_eq!(catalog.stats().peak_cached_sources, 1);
        let mut available = index();
        available.features.truncate(2);
        let inferred_path = directory.path().join(format!("infer.gti.{extension}"));
        terrio::write_vector(&[available], inferred_path.to_str().unwrap(), &Default::default()).unwrap();
        let inferred = RasterCatalog::open(
            inferred_path.to_str().unwrap(),
            CatalogOptions { resolution: None, bands: None, data_type: None, ..options() },
        )
        .unwrap();
        assert_eq!(inferred.stats().source_opens, 1);
        assert_eq!(inferred.header().raster.data_type, DataType::F64);
        assert_eq!((inferred.header().raster.cols, inferred.header().raster.rows), (6, 4));
    }
}

#[test]
fn many_disjoint_tiles_warp_only_their_intersections() {
    let mut footprints = index();
    footprints.features.clear();
    let mut inputs = HashMap::new();
    for i in 0..32 {
        let x = i as f64 * 4.0;
        let name = format!("tile-{i}");
        inputs.insert(name.clone(), tile(x, i as f64));
        let mut feature = Feature::with_geometry(
            i,
            Geometry::polygon(vec![Coord::xy(x, 0.0), Coord::xy(x + 4.0, 0.0), Coord::xy(x + 4.0, 4.0), Coord::xy(x, 4.0)], vec![]),
            2,
        );
        feature.attributes = vec![FieldValue::Text(name), FieldValue::Integer(i as i64)];
        footprints.push(feature);
    }
    let mut catalog =
        RasterCatalog::from_layer(footprints, inputs, CatalogOptions { extent: Some((0.0, 0.0, 128.0, 4.0)), ..options() }).unwrap();
    let output = catalog.read_window(Window::new(0, 0, 128, 4), &[]).unwrap();
    assert_eq!(catalog.stats().selected_features, 32);
    assert_eq!(catalog.stats().warped_pixels, 128 * 4);
    assert_eq!(catalog.stats().source_pixels, 128 * 4);
    for (i, value) in output.raster.band_view(0).as_slice().iter().enumerate() {
        assert_eq!(*value, ((i % 128) / 4) as f64);
    }
}

#[cfg(all(unix, feature = "sqlite", feature = "geotiff"))]
#[test]
fn failed_source_inventory_never_truncates_the_destination() {
    let directory = tempfile::tempdir().unwrap();
    for (name, dataset) in sources() {
        terrio::write_raster_dataset(&dataset, directory.path().join(name).to_str().unwrap(), &Default::default()).unwrap();
    }
    let index_path = directory.path().join("tiles.gti.gpkg");
    terrio::write_vector(&[index()], index_path.to_str().unwrap(), &Default::default()).unwrap();
    let mut catalog = RasterCatalog::open(index_path.to_str().unwrap(), options()).unwrap().into_lazy();
    let mut shared_catalog = catalog.shared();
    std::fs::rename(&index_path, index_path.with_extension("moved")).unwrap();
    let destination = directory.path().join("left.tif");
    let original = std::fs::read(&destination).unwrap();
    assert!(catalog.write(destination.to_str().unwrap(), &Default::default()).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), original);
    assert!(shared_catalog.write(destination.to_str().unwrap(), &Default::default()).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), original);

    // Once a complete inventory has been obtained, repeated exports reuse it.
    let moved = index_path.with_extension("moved");
    std::fs::rename(&moved, &index_path).unwrap();
    let mut catalog = RasterCatalog::open(index_path.to_str().unwrap(), options()).unwrap().into_lazy();
    let first = directory.path().join("first.tif");
    let second = directory.path().join("second.tif");
    catalog.write(first.to_str().unwrap(), &Default::default()).unwrap();
    std::fs::rename(&index_path, moved).unwrap();
    catalog.write(second.to_str().unwrap(), &Default::default()).unwrap();
    let first = terrio::open_raster(first.to_str().unwrap()).unwrap();
    let second = terrio::open_raster(second.to_str().unwrap()).unwrap();
    assert_eq!(first.raster.band_view(0).as_slice(), second.raster.band_view(0).as_slice());
}

#[test]
fn requested_bands_limit_reads_and_preserve_order_and_duplicates() {
    let mut source = tile(0.0, 0.0);
    source.raster.bands = 5;
    source.raster.data = RasterData::F64((1..=5).flat_map(|b| vec![b as f64 * 10.0; 16]).collect());
    let mut footprints = index();
    footprints.features.truncate(1);
    let mut catalog = RasterCatalog::from_layer(
        footprints,
        HashMap::from([("left.tif".into(), source)]),
        CatalogOptions { bands: Some(5), extent: Some((0., 0., 4., 4.)), ..options() },
    )
    .unwrap();
    let window = Window::new(0, 0, 4, 4);
    let selected = catalog.read_window(window, &[4, 2, 4]).unwrap();
    for (band, value) in [40.0, 20.0, 40.0].into_iter().enumerate() {
        assert_eq!(selected.raster.band_view(band).as_slice(), &[value; 16]);
    }
    assert_eq!(catalog.stats().source_pixels, 32);
    assert_eq!(catalog.stats().warped_pixels, 32);
    assert_eq!(catalog.read_mask(window).unwrap(), vec![255; 16]);
}

#[cfg(feature = "sqlite")]
#[test]
fn selected_data_reads_only_itself_and_the_extra_source_alpha() {
    let mut source = tile(0.0, 0.0);
    source.raster.bands = 6;
    let mut values: Vec<f64> = (1..=5).flat_map(|b| vec![b as f64 * 10.0; 16]).collect();
    values.extend([0.0].into_iter().chain([255.0; 15]));
    source.raster.data = RasterData::F64(values);
    let mut footprints = index();
    footprints.features.truncate(1);
    let dir = tempfile::tempdir().unwrap();
    let tile_path = dir.path().join("left.tif");
    terrio::write_raster_dataset(
        &source,
        tile_path.to_str().unwrap(),
        &terrio::RasterWriteOptions { creation_options: vec![("ALPHA".into(), "YES".into())], ..Default::default() },
    )
    .unwrap();
    let path = dir.path().join("index.gti.gpkg");
    terrio::write_vector(&[footprints], path.to_str().unwrap(), &Default::default()).unwrap();
    let mut catalog =
        RasterCatalog::open(path.to_str().unwrap(), CatalogOptions { bands: Some(5), extent: Some((0., 0., 4., 4.)), ..options() })
            .unwrap();
    let window = Window::new(0, 0, 4, 4);
    let selected = catalog.read_window(window, &[3]).unwrap();
    assert_eq!(selected.raster.band_view(0).as_slice()[0], 0.0);
    assert_eq!(&selected.raster.band_view(0).as_slice()[1..], &[30.0; 15]);
    assert_eq!(catalog.stats().source_pixels, 32);
    assert_eq!(catalog.stats().warped_pixels, 16);
    assert_eq!(catalog.read_mask(window).unwrap(), [vec![0], vec![255; 15]].concat());
}

#[test]
fn subset_reads_defer_full_coverage_and_generated_alpha_uses_all_data() {
    let mut source = tile(0.0, -9999.0);
    source.raster.bands = 2;
    source.has_nodata = true;
    source.raster.nodata = -9999.0;
    let mut values = vec![-9999.0; 32];
    values[1] = 10.0;
    values[16] = 20.0;
    source.raster.data = RasterData::F64(values);
    let mut footprints = index();
    footprints.features.truncate(1);
    let mut catalog = RasterCatalog::from_layer(
        footprints,
        HashMap::from([("left.tif".into(), source)]),
        CatalogOptions { bands: Some(3), alpha_band: Some(3), extent: Some((0., 0., 4., 4.)), ..options() },
    )
    .unwrap();
    let window = Window::new(0, 0, 4, 4);
    let selected = catalog.read_window(window, &[1]).unwrap();
    assert_eq!(selected.raster.band_view(0).as_slice()[1], 10.0);
    assert_eq!(catalog.stats().source_pixels, 16);
    let mask = catalog.read_mask(window).unwrap();
    assert_eq!(mask, [vec![255; 2], vec![0; 14]].concat());
    let before = catalog.stats();
    let alpha = catalog.read_window(window, &[3]).unwrap();
    assert_eq!(alpha.raster.band_view(0).as_slice(), &[vec![255.0; 2], vec![0.0; 14]].concat());
    assert_eq!(catalog.stats().source_pixels - before.source_pixels, 32);
    assert_eq!(catalog.stats().warped_pixels - before.warped_pixels, 16);
}

#[test]
fn optional_metadata_follows_gdal_first_tile_inspection() {
    let mut source = tile(0., 10.);
    source.has_nodata = true;
    source.raster.nodata = -9999.;
    if let RasterData::F64(values) = &mut source.raster.data {
        values[0] = -9999.;
    }
    let mut footprints = index();
    footprints.features.truncate(1);
    for (resolution, data_type, expected_type, expected_nodata, opens) in [
        (Some((1., 1.)), None, DataType::U8, None, 0),
        (Some((1., 1.)), Some(DataType::F64), DataType::F64, None, 0),
        (None, None, DataType::F64, Some(-9999.), 1),
    ] {
        let mut catalog = RasterCatalog::from_layer(
            footprints.clone(),
            HashMap::from([("left.tif".into(), source.clone())]),
            CatalogOptions { resolution, data_type, extent: Some((0., 0., 4., 4.)), ..options() },
        )
        .unwrap();
        assert_eq!(catalog.header().nodata(), expected_nodata);
        assert_eq!(catalog.header().raster.data_type, expected_type);
        assert_eq!(catalog.stats().source_opens, opens);
        assert_eq!(
            catalog.read_window(Window::new(0, 0, 1, 1), &[]).unwrap().raster.band_view(0).as_slice(),
            &[expected_nodata.unwrap_or(0.)]
        );
    }
}

#[test]
fn supplied_sources_defer_paths_and_encoded_files_until_selection() {
    use terrio::catalog::CatalogInput;
    let mut supplied = HashMap::from([
        ("left.tif".into(), CatalogInput::raster(tile(0., 10.).into())),
        ("right.tif".into(), CatalogInput::bytes(vec![0; 12], "invalid.tif")),
        ("missing.tif".into(), CatalogInput::path("nonexistent-catalog-source.tif")),
    ]);
    let mut catalog = RasterCatalog::from_sources(index(), std::mem::take(&mut supplied), options()).unwrap();
    assert_eq!(catalog.stats().source_opens, 0);
    assert_eq!(catalog.read_window(Window::new(0, 0, 1, 1), &[]).unwrap().raster.data.to_f64_vec(), vec![10.]);
    assert_eq!(catalog.stats().source_pixels, 1);
    assert!(catalog.read_window(Window::new(5, 0, 1, 1), &[]).is_err());
    assert_eq!(catalog.read_window(Window::new(0, 0, 1, 1), &[]).unwrap().raster.data.to_f64_vec(), vec![10.]);
    assert_eq!(catalog.stats().peak_cached_sources, 1);
}

#[cfg(feature = "geotiff")]
#[test]
fn supplied_lazy_handle_preserves_overwrite_protection() {
    use terrio::catalog::CatalogInput;
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("left.tif");
    terrio::write_raster_dataset(&tile(0., 10.), destination.to_str().unwrap(), &Default::default()).unwrap();
    let mut reader = terrio::LazyRaster::open(destination.to_str().unwrap()).unwrap();
    let mut footprint = index();
    footprint.features.truncate(1);
    let mut catalog = RasterCatalog::from_sources(
        footprint,
        HashMap::from([("left.tif".into(), CatalogInput::raster(reader.shared()))]),
        CatalogOptions { extent: Some((0., 0., 4., 4.)), ..options() },
    )
    .unwrap()
    .into_lazy();
    catalog.write(destination.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(terrio::open_raster(destination.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![10.; 16]);
}

#[test]
fn lazy_source_alias_overwrite_never_truncates_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("actual-source.tif");
    terrio::write_raster_dataset(&tile(0., 10.), path.to_str().unwrap(), &Default::default()).unwrap();
    let mut layer = index();
    layer.features.truncate(1);
    let sources = HashMap::from([("left.tif".into(), terrio::open_raster_lazy(path.to_str().unwrap()).unwrap())]);
    let catalog = RasterCatalog::from_lazy_layer(layer, sources, CatalogOptions { extent: Some((0., 0., 4., 4.)), ..options() }).unwrap();
    catalog.into_lazy().write(path.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(terrio::open_raster(path.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![10.; 16]);
}

#[test]
fn generated_alpha_ignores_source_alpha_when_all_data_is_nodata() {
    let mut source = tile(0., -9999.);
    source.raster.bands = 2;
    source.raster.data = RasterData::F64([vec![-9999.; 16], vec![255.; 16]].concat());
    source.raster.nodata = -9999.;
    source.has_nodata = true;
    source.driver = terrio::formats::RasterDriver::PNG;
    let mut footprints = index();
    footprints.features.truncate(1);
    let mut catalog = RasterCatalog::from_layer(
        footprints,
        HashMap::from([("left.tif".into(), source)]),
        CatalogOptions { bands: Some(3), alpha_band: Some(3), extent: Some((0., 0., 4., 4.)), ..options() },
    )
    .unwrap();
    assert_eq!(catalog.read_window(Window::new(0, 0, 1, 1), &[3]).unwrap().raster.data.to_f64_vec(), vec![0.]);
}

fn reference_vrt(source: &str, mask: Option<&str>) -> String {
    let source_band = |name: &str| {
        format!("<SimpleSource><SourceFilename relativeToVRT=\"1\">{name}</SourceFilename><SourceBand>1</SourceBand></SimpleSource>")
    };
    let mask = mask
        .map(|name| format!("<MaskBand><VRTRasterBand dataType=\"Byte\">{}</VRTRasterBand></MaskBand>", source_band(name)))
        .unwrap_or_default();
    format!(
        "<VRTDataset rasterXSize=\"4\" rasterYSize=\"4\"><SRS>EPSG:3857</SRS><GeoTransform>0,1,0,4,0,-1</GeoTransform><VRTRasterBand dataType=\"Float64\">{}</VRTRasterBand>{mask}</VRTDataset>",
        source_band(source)
    )
}

#[cfg(feature = "sqlite")]
#[test]
fn native_catalog_inventory_expands_nested_vrt_tiles_and_masks_before_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("actual.tif");
    let mask = dir.path().join("mask.tif");
    terrio::write_raster_dataset(&tile(0., 10.), data.to_str().unwrap(), &Default::default()).unwrap();
    terrio::write_raster_dataset(&tile(0., 255.), mask.to_str().unwrap(), &Default::default()).unwrap();
    std::fs::write(dir.path().join("nested.vrt"), reference_vrt("actual.tif", None)).unwrap();
    std::fs::write(dir.path().join("outer.vrt"), reference_vrt("nested.vrt", Some("mask.tif"))).unwrap();
    let mut footprints = index();
    footprints.features.truncate(1);
    footprints.features[0].attributes[0] = FieldValue::Text("outer.vrt".into());
    let index_path = dir.path().join("index.gti.gpkg");
    terrio::write_vector(&[footprints], index_path.to_str().unwrap(), &Default::default()).unwrap();
    for destination in [&data, &mask] {
        let catalog =
            RasterCatalog::open(index_path.to_str().unwrap(), CatalogOptions { extent: Some((0., 0., 4., 4.)), ..options() }).unwrap();
        catalog.into_lazy().write(destination.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(terrio::open_raster(destination.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![10.; 16]);
    }
}

#[test]
fn mapped_vrt_inputs_protect_actual_nested_files_for_handles_paths_and_bytes() {
    use terrio::catalog::CatalogInput;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("actual.tif");
    let child = dir.path().join("child.vrt");
    let parent = dir.path().join("parent.vrt");
    std::fs::write(&child, reference_vrt("actual.tif", None)).unwrap();
    let xml = reference_vrt("child.vrt", None);
    std::fs::write(&parent, &xml).unwrap();
    for mode in 0..3 {
        terrio::write_raster_dataset(&tile(0., 10.), data.to_str().unwrap(), &Default::default()).unwrap();
        let source = match mode {
            0 => CatalogInput::raster(terrio::open_raster_lazy(parent.to_str().unwrap()).unwrap()),
            1 => CatalogInput::path(parent.to_string_lossy()),
            _ => CatalogInput::bytes(xml.as_bytes().to_vec(), parent.to_string_lossy()),
        };
        let mut footprints = index();
        footprints.features.truncate(1);
        let catalog = RasterCatalog::from_sources(
            footprints,
            HashMap::from([("left.tif".into(), source)]),
            CatalogOptions { extent: Some((0., 0., 4., 4.)), ..options() },
        )
        .unwrap();
        catalog.into_lazy().write(data.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(terrio::open_raster(data.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![10.; 16]);
    }
}

#[test]
fn cyclic_vrt_inventory_fails_before_catalog_destination_is_opened() {
    use terrio::catalog::CatalogInput;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("actual.tif");
    let path = dir.path().join("cycle.vrt");
    terrio::write_raster_dataset(&tile(0., 10.), data.to_str().unwrap(), &Default::default()).unwrap();
    let original = std::fs::read(&data).unwrap();
    std::fs::write(&path, reference_vrt("cycle.vrt", None)).unwrap();
    let mut footprints = index();
    footprints.features.truncate(1);
    let catalog = RasterCatalog::from_sources(
        footprints,
        HashMap::from([("left.tif".into(), CatalogInput::path(path.to_string_lossy()))]),
        CatalogOptions { extent: Some((0., 0., 4., 4.)), ..options() },
    )
    .unwrap();
    let error = catalog.into_lazy().write(data.to_str().unwrap(), &Default::default()).unwrap_err();
    assert!(error.to_string().contains("cyclic"), "{error}");
    assert_eq!(std::fs::read(&data).unwrap(), original);
}

#[test]
fn failed_encoded_vrt_is_stable_and_blocks_export_before_creation() {
    use terrio::catalog::CatalogInput;
    let mut layer = index();
    layer.features.truncate(1);
    let mut catalog = RasterCatalog::from_sources(
        layer,
        HashMap::from([("left.tif".into(), CatalogInput::bytes(b"invalid VRT XML".to_vec(), "bad.vrt"))]),
        CatalogOptions { extent: Some((0., 0., 4., 4.)), ..options() },
    )
    .unwrap();
    let first = catalog.read_window(Window::new(0, 0, 1, 1), &[]).unwrap_err().to_string();
    assert_eq!(catalog.read_window(Window::new(0, 0, 1, 1), &[]).unwrap_err().to_string(), first);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("existing.tif");
    std::fs::write(&output, b"unchanged").unwrap();
    assert!(catalog.into_lazy().write(output.to_str().unwrap(), &Default::default()).is_err());
    assert_eq!(std::fs::read(&output).unwrap(), b"unchanged");
}
