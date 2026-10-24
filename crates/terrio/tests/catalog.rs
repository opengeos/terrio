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
    assert!(catalog.read_mask(Window::new(0, 0, 6, 4)).unwrap().iter().all(|v| *v == 255));
}
#[test]
fn extra_alpha_band_masks_overlapping_tiles() {
    let mut input = sources();
    let right = input.get_mut("right.tif").unwrap();
    right.raster.bands = 2;
    let mut values = vec![20.0; 16];
    values.extend([0.0; 16]);
    right.raster.data = RasterData::F64(values);
    let mut catalog = RasterCatalog::from_layer(index(), input, options()).unwrap();
    let result = catalog.read_window(Window::new(0, 0, 6, 4), &[]).unwrap();
    assert_eq!(&result.raster.band_view(0).as_slice()[..6], &[10.0, 10.0, 10.0, 10.0, 0.0, 0.0]);
    assert_eq!(&catalog.read_mask(Window::new(0, 0, 6, 4)).unwrap()[..6], &[255, 255, 255, 255, 0, 0]);
    let mut lazy = catalog.into_lazy();
    assert!(lazy.has_mask());
    assert_eq!(&lazy.read_mask(Window::new(0, 0, 6, 4), None).unwrap().unwrap()[..6], &[255, 255, 255, 255, 0, 0]);
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
