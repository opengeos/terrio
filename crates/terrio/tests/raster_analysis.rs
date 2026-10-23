//! Contours, zonal statistics and standalone SQL, including affine grids and invalid inputs.

use terrio::ops::contour::{ContourOptions, contour};
use terrio::ops::zonal::{Statistic as S, ZonalInput, ZonalOptions, zonal_stats};
use terrio::vector::Coord;
use terrio::{DataType, Feature, FieldDef, FieldType, FieldValue as V, Geometry as G, Layer, Raster, RasterConfig, RasterDataset};

fn raster(cols: usize, rows: usize, bands: usize, values: &[f64], nodata: Option<f64>) -> RasterDataset {
    let cfg = RasterConfig { cols, rows, bands, data_type: DataType::F64, nodata: nodata.unwrap_or(-9999.0), ..Default::default() };
    let mut r = RasterDataset::new(Raster::from_data(cfg, values.to_vec()).unwrap(), nodata.is_some());
    r.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
    r
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> G {
    G::polygon(vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)], vec![])
}

fn zones(geometries: Vec<Option<G>>) -> Layer {
    let mut l = Layer::new("zones");
    l.schema.add_field(FieldDef::new("name", FieldType::Text));
    for (i, g) in geometries.into_iter().enumerate() {
        l.features.push(Feature { fid: 100 + i as u64, geometry: g, attributes: vec![V::Text(format!("zone {i}"))] });
    }
    l
}

fn field<'a>(l: &'a Layer, feature: usize, name: &str) -> &'a V {
    l.features[feature].get(&l.schema, name).unwrap()
}

#[test]
fn contour_ramp_reaches_raster_edges_and_has_elevation_and_z() {
    let mut r = raster(2, 2, 1, &[0., 10., 0., 10.], None);
    r.raster.crs.epsg = Some(4326);
    let out = contour(&r, &ContourOptions { levels: vec![5.], with_z: true, ..Default::default() }).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out.crs_epsg(), Some(4326));
    assert_eq!(out.features[0].attributes, [V::Integer(0), V::Float(5.)]);
    let Some(G::LineString(cs)) = &out.features[0].geometry else { panic!() };
    assert!(cs.iter().all(|c| (c.x - 1.).abs() < 1e-6 && c.z == Some(5.)));
    let bounds = out.features[0].geometry.as_ref().unwrap().bbox().unwrap();
    assert_eq!((bounds.min_y, bounds.max_y), (0., 2.));
}

#[test]
fn contour_closed_peak_and_nodata_and_rotated_grid() {
    let r = raster(3, 3, 1, &[0., 0., 0., 0., 10., 0., 0., 0., 0.], None);
    let o = ContourOptions { levels: vec![5.], ..Default::default() };
    let out = contour(&r, &o).unwrap();
    assert_eq!(out.len(), 1);
    let Some(G::LineString(cs)) = &out.features[0].geometry else { panic!() };
    assert_eq!(cs.first(), cs.last());
    let mut rotated = r.clone();
    let gt = [100., 10., 2., 200., 1., 10.];
    rotated.set_geo_transform(gt);
    let after = contour(&rotated, &o).unwrap();
    let Some(G::LineString(mapped)) = &after.features[0].geometry else { panic!() };
    for (a, b) in cs.iter().zip(mapped) {
        let row = 3. - a.y;
        assert!((b.x - (gt[0] + a.x * gt[1] + row * gt[2])).abs() < 1e-10);
        assert!((b.y - (gt[3] + a.x * gt[4] + row * gt[5])).abs() < 1e-10);
    }
    let r = raster(2, 2, 1, &[42., -9999., 42., -9999.], Some(-9999.));
    let o = ContourOptions { levels: vec![0.], ..Default::default() };
    assert!(contour(&r, &o).unwrap().features.is_empty());
    assert!(!contour(&r, &ContourOptions { ignore_nodata: true, ..o.clone() }).unwrap().features.is_empty());
    assert!(contour(&r, &ContourOptions { nodata: Some(42.), ..o }).unwrap().features.is_empty());
}

#[test]
fn contour_float32_nodata_uses_the_stored_sample_precision() {
    let cfg = RasterConfig { cols: 2, rows: 2, data_type: DataType::F32, nodata: -9999.1, ..Default::default() };
    let mut r = RasterDataset::new(Raster::from_data(cfg, vec![42., -9999.1, 42., -9999.1]).unwrap(), true);
    let options = ContourOptions { levels: vec![0.], ..Default::default() };
    assert!(contour(&r, &options).unwrap().features.is_empty());
    assert!(!contour(&r, &ContourOptions { ignore_nodata: true, ..options.clone() }).unwrap().features.is_empty());
    r.has_nodata = false;
    assert!(contour(&r, &ContourOptions { nodata: Some(-9999.1), ..options }).unwrap().features.is_empty());
}

#[test]
fn contour_interval_fixed_levels_and_argument_validation() {
    let r = raster(2, 2, 2, &[0., 10., 0., 10., 100., 110., 100., 110.], None);
    let o = ContourOptions { band: 2, interval: Some(5.), offset: 1., levels: vec![103., 103.], ..Default::default() };
    let out = contour(&r, &o).unwrap();
    let mut levels = out.features.iter().map(|f| f.attributes[1].as_f64().unwrap()).collect::<Vec<_>>();
    levels.sort_by(f64::total_cmp);
    levels.dedup();
    assert_eq!(levels, vec![101., 103., 106.]);
    for bad in [
        ContourOptions::default(),
        ContourOptions { band: 0, ..o.clone() },
        ContourOptions { band: 3, ..o.clone() },
        ContourOptions { interval: Some(0.), ..o.clone() },
        ContourOptions { interval: Some(-1.), ..o.clone() },
        ContourOptions { levels: vec![f64::NAN], ..o.clone() },
        ContourOptions { offset: f64::INFINITY, ..o.clone() },
        ContourOptions { field_name: "ID".into(), ..o.clone() },
        ContourOptions { interval: Some(1e-12), ..o.clone() },
        ContourOptions { ignore_nodata: true, nodata: Some(0.), ..o.clone() },
    ] {
        assert!(contour(&r, &bad).is_err(), "{bad:?}");
    }
    let empty = raster(1, 1, 1, &[f64::NAN], Some(f64::NAN));
    assert!(contour(&empty, &ContourOptions { interval: Some(1.), ..Default::default() }).unwrap().features.is_empty());
}

#[test]
fn zonal_holes_overlap_multiple_bands_nulls_and_field_collisions() {
    let mut values = (1..=16).map(|v| v as f64).collect::<Vec<_>>();
    values.extend([
        100., -9999., -9999., -9999., -9999., -9999., -9999., -9999., -9999., -9999., -9999., -9999., -9999., -9999., -9999., 200.,
    ]);
    let r = raster(4, 4, 2, &values, Some(-9999.));
    let zero = raster(4, 4, 1, &[0.; 16], None);
    let mut holed = rectangle(0., 0., 4., 4.);
    let G::Polygon { interiors, .. } = &mut holed else { panic!() };
    let G::Polygon { exterior, .. } = rectangle(1., 1., 3., 3.) else { panic!() };
    interiors.push(exterior);
    let mut l = zones(vec![Some(rectangle(0., 0., 4., 4.)), Some(holed), Some(rectangle(10., 10., 11., 11.)), None]);
    l.schema.add_field(FieldDef::new("x_count", FieldType::Integer));
    for f in &mut l.features {
        f.attributes.push(V::Integer(99));
    }
    let o = ZonalOptions { stats: vec![S::Count, S::Min, S::Max, S::Mean, S::Sum, S::StdDev, S::Median], ..Default::default() };
    let inputs = [
        ZonalInput { prefix: "x".into(), raster: &r, band: 1 },
        ZonalInput { prefix: "b2".into(), raster: &r, band: 2 },
        ZonalInput { prefix: "zero".into(), raster: &zero, band: 1 },
    ];
    let out = zonal_stats(&l, &inputs, &o).unwrap();
    assert_eq!(field(&out, 0, "x_count"), &V::Integer(99));
    assert_eq!(field(&out, 0, "x_count_2"), &V::Integer(16));
    assert_eq!(field(&out, 0, "x_sum"), &V::Float(136.));
    assert_eq!(field(&out, 0, "x_mean"), &V::Float(8.5));
    assert_eq!(field(&out, 0, "x_median"), &V::Float(8.5));
    assert!((field(&out, 0, "x_stddev").as_f64().unwrap() - 21.25f64.sqrt()).abs() < 1e-12);
    assert_eq!(field(&out, 1, "x_count_2"), &V::Integer(12));
    assert_eq!(field(&out, 1, "x_sum"), &V::Float(102.));
    assert_eq!(field(&out, 0, "b2_count"), &V::Integer(2));
    assert_eq!(field(&out, 0, "b2_median"), &V::Float(150.));
    assert_eq!(field(&out, 0, "zero_count"), &V::Integer(16));
    assert_eq!(field(&out, 0, "zero_mean"), &V::Float(0.));
    for i in [2, 3] {
        assert_eq!(field(&out, i, "x_count_2"), &V::Integer(0));
        for name in ["x_min", "x_max", "x_mean", "x_sum", "x_stddev", "x_median"] {
            assert_eq!(field(&out, i, name), &V::Null);
        }
    }
    for (a, b) in out.features.iter().zip(&l.features) {
        assert_eq!(a.fid, b.fid);
        assert_eq!(a.geometry, b.geometry);
        assert_eq!(&a.attributes[..2], &b.attributes);
    }
}

#[test]
fn zonal_median_preserves_subnormals_and_avoids_overflow() {
    let tiny = f64::from_bits(1);
    let l = zones(vec![Some(rectangle(0., 0., 2., 1.))]);
    let options = ZonalOptions { stats: vec![S::Median], ..Default::default() };
    for (values, expected) in [
        ([tiny, tiny], tiny),
        ([tiny, 5. * tiny], 3. * tiny),
        ([-tiny, -tiny], -tiny),
        ([f64::MAX, f64::MAX], f64::MAX),
        ([-f64::MAX, -f64::MAX], -f64::MAX),
        ([-f64::MAX, f64::MAX], 0.),
    ] {
        let r = raster(2, 1, 1, &values, None);
        let out = zonal_stats(&l, &[ZonalInput { prefix: "x".into(), raster: &r, band: 1 }], &options).unwrap();
        assert_eq!(field(&out, 0, "x_median"), &V::Float(expected), "{values:?}");
    }
}

#[test]
fn zonal_all_touched_rotated_grids_and_crs_transformations() {
    let mut r = raster(3, 3, 1, &[1.; 9], None);
    let l = zones(vec![Some(rectangle(0.9, 0.9, 1.1, 1.1))]);
    let inputs = [ZonalInput { prefix: "x".into(), raster: &r, band: 1 }];
    assert_eq!(field(&zonal_stats(&l, &inputs, &Default::default()).unwrap(), 0, "x_count"), &V::Integer(0));
    assert_eq!(
        field(&zonal_stats(&l, &inputs, &ZonalOptions { all_touched: true, ..Default::default() }).unwrap(), 0, "x_count"),
        &V::Integer(4)
    );
    let mut l = zones(vec![Some(rectangle(0., 0., 3., 3.))]);
    let gt = [100., 10., 2., 200., 1., 10.];
    r.set_geo_transform(gt);
    terrio::vector::map_coords(l.features[0].geometry.as_mut().unwrap(), &mut |c| {
        let (x, row) = (c.x, 3. - c.y);
        c.x = gt[0] + x * gt[1] + row * gt[2];
        c.y = gt[3] + x * gt[4] + row * gt[5];
        Ok(())
    })
    .unwrap();
    assert_eq!(
        field(&zonal_stats(&l, &[ZonalInput { prefix: "x".into(), raster: &r, band: 1 }], &Default::default()).unwrap(), 0, "x_count"),
        &V::Integer(9)
    );
    let mut r = raster(1, 1, 1, &[8.], None);
    r.raster.crs.epsg = Some(3857);
    r.set_geo_transform([-100000., 200000., 0., 100000., 0., -200000.]);
    let mut l = zones(vec![Some(rectangle(-0.5, -0.5, 0.5, 0.5))]);
    l.set_crs_epsg(Some(4326));
    let out = zonal_stats(&l, &[ZonalInput { prefix: "x".into(), raster: &r, band: 1 }], &Default::default()).unwrap();
    assert_eq!(field(&out, 0, "x_mean"), &V::Float(8.));
    assert_eq!(out.crs_epsg(), Some(4326));
    assert_eq!(out.features[0].geometry, l.features[0].geometry);
    l.crs = None;
    assert!(zonal_stats(&l, &[ZonalInput { prefix: "x".into(), raster: &r, band: 1 }], &Default::default()).is_err());
}

#[test]
fn zonal_rejects_invalid_geometry_bands_and_statistics() {
    let r = raster(1, 1, 1, &[1.], None);
    let l = zones(vec![Some(rectangle(0., 0., 1., 1.))]);
    let inputs = [ZonalInput { prefix: "x".into(), raster: &r, band: 1 }];
    assert!(zonal_stats(&l, &[], &Default::default()).is_err());
    for stats in [vec![], vec![S::Mean, S::Mean]] {
        assert!(zonal_stats(&l, &inputs, &ZonalOptions { stats, ..Default::default() }).is_err());
    }
    assert!(zonal_stats(&l, &[ZonalInput { prefix: "x".into(), raster: &r, band: 0 }], &Default::default()).is_err());
    let point = zones(vec![Some(G::point(0., 0.))]);
    assert!(zonal_stats(&point, &inputs, &Default::default()).is_err());
    assert!(S::parse("anything").is_err());
    let mut projected = r.clone();
    projected.raster.crs = terrio::crs::SpatialRef::parse("+proj=ortho +lat_0=0 +lon_0=0 +datum=WGS84 +units=m").unwrap().to_crs_info();
    let mut outside = zones(vec![Some(rectangle(170., -1., 180., 1.))]);
    outside.set_crs_epsg(Some(4326));
    let error = zonal_stats(&outside, &[ZonalInput { prefix: "x".into(), raster: &projected, band: 1 }], &Default::default()).unwrap_err();
    assert!(error.to_string().contains("zone 100"));
}

#[test]
fn standalone_sql_preserves_features_and_checks_layer_names_and_schema() {
    let mut l = zones(vec![Some(G::point(0., 0.)), Some(G::point(1., 1.)), None]);
    l.set_crs_epsg(Some(4326));
    l.schema.add_field(FieldDef::new("population", FieldType::Integer));
    l.features[0].attributes.push(V::Integer(2));
    l.features[1].attributes.push(V::Integer(5));
    let out = terrio::ops::sql::query(
        &[l.clone()],
        "SELECT name AS label, population FROM ZONES WHERE population >= 2 ORDER BY population DESC LIMIT 1",
    )
    .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out.features[0].fid, 101);
    assert_eq!(out.crs_epsg(), Some(4326));
    assert_eq!(out.features[0].geometry, l.features[1].geometry);
    assert_eq!(out.features[0].attributes, [V::Text("zone 1".into()), V::Integer(5)]);
    let sorted = terrio::ops::sql::query(&[l.clone()], "SELECT * FROM zones ORDER BY population").unwrap();
    assert_eq!(sorted.features[0].fid, 102);
    assert_eq!(sorted.features[0].attributes[1], V::Null);
    for bad in ["SELECT * FROM missing", "SELECT name AS x, population AS X FROM zones", "SELECT COUNT(*) FROM zones", "DELETE FROM zones"]
    {
        assert!(terrio::ops::sql::query(&[l.clone()], bad).is_err(), "{bad}");
    }
    l.features[0].attributes[1] = V::Integer(9_007_199_254_740_993);
    l.features[1].attributes[1] = V::Integer(9_007_199_254_740_992);
    let sorted = terrio::ops::sql::query(&[l.clone()], "SELECT * FROM zones ORDER BY population").unwrap();
    assert_eq!(sorted.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [102, 101, 100]);
    l.schema.add_field(FieldDef::new("enabled", FieldType::Boolean));
    l.features[0].attributes.push(V::Boolean(true));
    l.features[1].attributes.push(V::Boolean(false));
    let sorted = terrio::ops::sql::query(&[l.clone()], "SELECT * FROM zones ORDER BY enabled").unwrap();
    assert_eq!(sorted.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [102, 101, 100]);
    l.schema.add_field(FieldDef::new("NAME", FieldType::Text));
    assert!(terrio::ops::sql::query(&[l], "SELECT * FROM zones").is_err());
}

#[test]
fn standalone_sql_limits_after_ordering_without_changing_translate_filters() {
    let mut l = zones(vec![Some(G::point(0., 0.)), Some(G::point(1., 1.)), Some(G::point(2., 2.))]);
    l.schema.add_field(FieldDef::new("population", FieldType::Integer));
    for (i, feature) in l.features.iter_mut().enumerate() {
        feature.attributes.push(V::Integer(i as i64 + 1));
    }
    let query = "SELECT * FROM zones WHERE population >= 2 ORDER BY population DESC LIMIT 1";
    let out = terrio::ops::sql::query(std::slice::from_ref(&l), query).unwrap();
    assert_eq!(out.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [102]);
    let empty = terrio::ops::sql::query(std::slice::from_ref(&l), "SELECT name AS label FROM zones LIMIT 0").unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.schema.fields()[0].name, "label");
    assert!(terrio::ops::sql::query(std::slice::from_ref(&l), "SELECT missing FROM zones LIMIT 0").is_err());

    // Convert applies its attribute and spatial filters before the SQL limit.
    let options = terrio::ops::vector_filter::VectorTranslateOptions {
        sql: Some("SELECT * FROM zones ORDER BY population DESC LIMIT 1".into()),
        where_clause: Some("population <= 2".into()),
        spatial_filter: Some((0.75, 0.75, 1.25, 1.25)),
        ..Default::default()
    };
    let out = terrio::ops::vector_filter::translate(&[l], &options).unwrap();
    assert_eq!(out[0].features.iter().map(|f| f.fid).collect::<Vec<_>>(), [101]);
}

#[test]
fn real_world_contours_and_zonal_values_from_gdal() {
    // GDAL 3.12.2 reference values; the independent comparison script also checks every
    // contour's geometry and every county's statistics with GDAL, Shapely and NumPy.
    let Some(dir) = std::env::var_os("TERRIO_TEST_DATA") else { return };
    let dir = std::path::PathBuf::from(dir);
    let dem = terrio::open_raster(dir.join("dem.tif").to_str().unwrap()).unwrap();
    let crop = terrio::ops::translate::translate(
        &dem,
        &terrio::ops::translate::TranslateOptions { src_win: Some((100., 100., 160., 160.)), ..Default::default() },
    )
    .unwrap();
    let lines = contour(&crop, &ContourOptions { interval: Some(5.), ..Default::default() }).unwrap();
    assert_eq!(lines.len(), 184);
    let mut counties =
        terrio::open_vector(dir.join("TN_Counties.geojson").to_str().unwrap(), &Default::default()).unwrap().layers.remove(0);
    counties.features.retain(|f| f.get(&counties.schema, "NAME").unwrap().as_str() == Some("Knox County"));
    counties.extent = None;
    assert_eq!(counties.len(), 1);
    let landcover = terrio::open_raster(dir.join("nlcd_knoxville.tif").to_str().unwrap()).unwrap();
    let out = zonal_stats(
        &counties,
        &[ZonalInput { prefix: "nlcd".into(), raster: &landcover, band: 1 }],
        &ZonalOptions { stats: vec![S::Count, S::Sum, S::Mean, S::StdDev, S::Median], ..Default::default() },
    )
    .unwrap();
    assert_eq!(field(&out, 0, "nlcd_count"), &V::Integer(167466));
    assert_eq!(field(&out, 0, "nlcd_sum"), &V::Float(6714242.));
    assert_eq!(field(&out, 0, "nlcd_median"), &V::Float(41.));
    assert!((field(&out, 0, "nlcd_mean").as_f64().unwrap() - 40.09316517979769).abs() < 1e-10);
    assert!((field(&out, 0, "nlcd_stddev").as_f64().unwrap() - 22.454011195808928).abs() < 1e-9);
    let countries = terrio::open_vector(dir.join("countries.geojson").to_str().unwrap(), &Default::default()).unwrap();
    let query =
        format!("SELECT ADMIN, POP_EST FROM \"{}\" WHERE CONTINENT = 'Asia' ORDER BY POP_EST DESC LIMIT 10", countries.layers[0].name);
    let out = terrio::ops::sql::query(&countries.layers, &query).unwrap();
    assert_eq!(out.len(), 10);
    assert_eq!(field(&out, 0, "ADMIN").as_str(), Some("China"));
}
