//! Contours, zonal statistics and standalone SQL, including affine grids and invalid inputs.

use terrio::ops::contour::{ContourOptions, contour};
use terrio::ops::rasterize::Grid;
use terrio::ops::zonal::{Statistic as S, ZonalBand, ZonalOptions, ZonalSource, zonal_stats};
use terrio::vector::Coord;
use terrio::{
    DataType, Feature, FieldDef, FieldType, FieldValue as V, Geometry as G, Layer, LazyRaster, Raster, RasterConfig, RasterDataset,
};

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

fn src(prefix: &str, r: &RasterDataset, band: usize) -> ZonalSource {
    ZonalSource { raster: LazyRaster::from(r.clone()).into(), bands: vec![ZonalBand::new(band, prefix)] }
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
    let mut inputs = [src("x", &r, 1), src("b2", &r, 2), src("zero", &zero, 1)];
    let out = zonal_stats(&l, &mut inputs, &o).unwrap();
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
        let out = zonal_stats(&l, &mut [src("x", &r, 1)], &options).unwrap();
        assert_eq!(field(&out, 0, "x_median"), &V::Float(expected), "{values:?}");
    }
}

#[test]
fn zonal_all_touched_rotated_grids_and_crs_transformations() {
    let mut r = raster(3, 3, 1, &[1.; 9], None);
    let l = zones(vec![Some(rectangle(0.9, 0.9, 1.1, 1.1))]);
    let mut inputs = [src("x", &r, 1)];
    assert_eq!(field(&zonal_stats(&l, &mut inputs, &Default::default()).unwrap(), 0, "x_count"), &V::Integer(0));
    assert_eq!(
        field(&zonal_stats(&l, &mut inputs, &ZonalOptions { all_touched: true, ..Default::default() }).unwrap(), 0, "x_count"),
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
    assert_eq!(field(&zonal_stats(&l, &mut [src("x", &r, 1)], &Default::default()).unwrap(), 0, "x_count"), &V::Integer(9));
    let mut r = raster(1, 1, 1, &[8.], None);
    r.raster.crs.epsg = Some(3857);
    r.set_geo_transform([-100000., 200000., 0., 100000., 0., -200000.]);
    let mut l = zones(vec![Some(rectangle(-0.5, -0.5, 0.5, 0.5))]);
    l.set_crs_epsg(Some(4326));
    let out = zonal_stats(&l, &mut [src("x", &r, 1)], &Default::default()).unwrap();
    assert_eq!(field(&out, 0, "x_mean"), &V::Float(8.));
    assert_eq!(out.crs_epsg(), Some(4326));
    assert_eq!(out.features[0].geometry, l.features[0].geometry);
    l.crs = None;
    assert!(zonal_stats(&l, &mut [src("x", &r, 1)], &Default::default()).is_err());
}

#[test]
fn zonal_rejects_invalid_geometry_bands_and_statistics() {
    let r = raster(1, 1, 1, &[1.], None);
    let l = zones(vec![Some(rectangle(0., 0., 1., 1.))]);
    let mut inputs = [src("x", &r, 1)];
    assert!(zonal_stats(&l, &mut [], &Default::default()).is_err());
    for stats in [vec![], vec![S::Mean, S::Mean]] {
        assert!(zonal_stats(&l, &mut inputs, &ZonalOptions { stats, ..Default::default() }).is_err());
    }
    assert!(zonal_stats(&l, &mut [src("x", &r, 0)], &Default::default()).is_err());
    let point = zones(vec![Some(G::point(0., 0.))]);
    assert!(zonal_stats(&point, &mut inputs, &Default::default()).is_err());
    assert!(S::parse("anything").is_err());
    let mut projected = r.clone();
    projected.raster.crs = terrio::crs::SpatialRef::parse("+proj=ortho +lat_0=0 +lon_0=0 +datum=WGS84 +units=m").unwrap().to_crs_info();
    let mut outside = zones(vec![Some(rectangle(170., -1., 180., 1.))]);
    outside.set_crs_epsg(Some(4326));
    let error = zonal_stats(&outside, &mut [src("x", &projected, 1)], &Default::default()).unwrap_err();
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
    for bad in ["SELECT * FROM missing", "SELECT name AS x, population AS X FROM zones", "DELETE FROM zones"] {
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
fn translate_sql_preserves_legacy_duplicate_fields_and_single_layer_fallback() {
    let mut l = zones(vec![Some(G::point(0., 0.))]);
    l.schema.add_field(FieldDef::new("NAME", FieldType::Text));
    l.features[0].attributes.push(V::Text("other name".into()));
    for (sql, fields) in [("SELECT * FROM legacy_layer_name", ["name", "NAME"]), ("SELECT name AS x, name AS X FROM zones", ["x", "X"])] {
        let options = terrio::ops::vector_filter::VectorTranslateOptions { sql: Some(sql.into()), ..Default::default() };
        let out = terrio::ops::vector_filter::translate(std::slice::from_ref(&l), &options).unwrap();
        assert_eq!(out[0].len(), 1);
        assert_eq!(out[0].features[0].fid, l.features[0].fid);
        assert_eq!(out[0].features[0].geometry, l.features[0].geometry);
        assert_eq!(out[0].schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), fields);
    }
    for sql in ["SELECT * FROM zones", "SELECT name AS x, name AS X FROM zones", "SELECT * FROM legacy_layer_name"] {
        assert!(terrio::ops::sql::query(std::slice::from_ref(&l), sql).is_err(), "{sql}");
    }
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
        &mut [src("nlcd", &landcover, 1)],
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

#[test]
fn zonal_output_controls_leave_source_and_analytical_grid_unchanged() {
    let mut r = raster(2, 1, 1, &[1.125, 2.125], None);
    r.raster.crs.epsg = Some(4326);
    let mut l = zones(vec![Some(rectangle(0., 0., 2., 1.)), None]);
    l.set_crs_epsg(Some(4326));
    l.schema.add_field(FieldDef::new("source_float", FieldType::Float));
    for feature in &mut l.features {
        feature.attributes.push(V::Float(1.23456789));
    }
    let mut inputs = [src("v", &r, 1)];
    let options = ZonalOptions {
        stats: vec![S::Count, S::Mean, S::Sum],
        dst_srs: Some(terrio::crs::SpatialRef::from_epsg(3857).unwrap()),
        decimal_places: Some(2),
        ..Default::default()
    };
    let out = zonal_stats(&l, &mut inputs, &options).unwrap();
    assert_eq!(out.crs_epsg(), Some(3857));
    assert_eq!(out.features[0].attributes, [V::Text("zone 0".into()), V::Float(1.23456789), V::Integer(2), V::Float(1.63), V::Float(3.25)]);
    assert_eq!(out.features[1].attributes[2..], [V::Integer(0), V::Null, V::Null]);
    assert_eq!(out.features[0].fid, l.features[0].fid);
    let expected = terrio::vector::reproject_layer(&l, options.dst_srs.as_ref().unwrap()).unwrap();
    assert_eq!(out.features[0].geometry, expected.features[0].geometry);
    assert_eq!(field(&zonal_stats(&l, &mut inputs, &Default::default()).unwrap(), 0, "v_mean"), &V::Float(1.625));
    assert!(zonal_stats(&l, &mut inputs, &ZonalOptions { decimal_places: Some(16), ..Default::default() }).is_err());
}

#[test]
fn fractional_zonal_coverage_and_weighted_statistics() {
    use terrio::ops::zonal::ZonalWeight;
    let r = raster(2, 1, 1, &[10., 30.], None);
    let w = raster(2, 1, 1, &[2., 4.], None);
    let l = zones(vec![Some(rectangle(0.5, 0., 2., 1.)), None, Some(rectangle(9., 9., 10., 10.))]);
    let mut inputs = [src("v", &r, 1)];
    let o = ZonalOptions {
        fractional: true,
        weights: Some(ZonalWeight::new(w.clone().into(), 1)),
        stats: vec![S::Count, S::Min, S::Max, S::Mean, S::Sum, S::StdDev, S::Median, S::WeightedMean, S::WeightedSum, S::WeightedStdDev],
        ..Default::default()
    };
    let out = zonal_stats(&l, &mut inputs, &o).unwrap();
    for (name, expected) in [
        ("count", 1.5),
        ("min", 10.),
        ("max", 30.),
        ("sum", 35.),
        ("median", 20.),
        ("mean", 35. / 1.5),
        ("stddev", (800f64 / 9.).sqrt()),
        ("weighted_mean", 26.),
        ("weighted_sum", 130.),
        ("weighted_stddev", 8.),
    ] {
        let V::Float(got) = field(&out, 0, &format!("v_{name}")) else { panic!("expected float {name}") };
        assert!((got - expected).abs() < 1e-10, "{name}: {got} != {expected}");
    }
    for row in [1, 2] {
        assert_eq!(field(&out, row, "v_count"), &V::Float(0.));
        assert_eq!(field(&out, row, "v_weighted_sum"), &V::Null);
    }
    assert_eq!(out.features[0].geometry, l.features[0].geometry);
    assert_eq!(out.features[0].fid, l.features[0].fid);
    assert!(zonal_stats(&l, &mut inputs, &ZonalOptions { all_touched: true, ..o.clone() }).is_err());
    assert!(zonal_stats(&l, &mut inputs, &ZonalOptions { weights: None, ..o }).is_err());
}

#[test]
fn fractional_zonal_triangle_holes_and_rotated_grids() {
    let mut r = raster(2, 1, 1, &[10., 30.], None);
    let triangle = G::polygon(vec![Coord::xy(0., 0.), Coord::xy(2., 0.), Coord::xy(0., 1.), Coord::xy(0., 0.)], vec![]);
    let o = ZonalOptions { fractional: true, ..Default::default() };
    // `tolerance` covers pixel-space roundoff from inverting a rotated geotransform.
    let check = |r: &RasterDataset, g, tolerance: f64| {
        let out = zonal_stats(&zones(vec![Some(g)]), &mut [src("v", r, 1)], &o).unwrap();
        for (name, expected) in [("v_count", 1.), ("v_sum", 15.)] {
            let &V::Float(value) = field(&out, 0, name) else { panic!("{name} is not a float") };
            assert!((value - expected).abs() <= tolerance, "{name}: {value} != {expected}");
        }
    };
    check(&r, triangle.clone(), 0.);
    let gt = [100., 10., 2., 200., 1., 10.];
    r.set_geo_transform(gt);
    let mut mapped = triangle;
    terrio::vector::map_coords(&mut mapped, &mut |c| {
        let row = 1. - c.y;
        (c.x, c.y) = (gt[0] + c.x * gt[1] + row * gt[2], gt[3] + c.x * gt[4] + row * gt[5]);
        Ok(())
    })
    .unwrap();
    check(&r, mapped, 1e-12);
    let r = raster(2, 2, 1, &[1.; 4], None);
    let G::Polygon { exterior, .. } = rectangle(0., 0., 2., 2.) else { unreachable!() };
    let G::Polygon { exterior: hole, .. } = rectangle(0.5, 0.5, 1.5, 1.5) else { unreachable!() };
    let l = zones(vec![Some(G::Polygon { exterior, interiors: vec![hole] })]);
    let out = zonal_stats(&l, &mut [src("v", &r, 1)], &o).unwrap();
    assert_eq!(field(&out, 0, "v_count"), &V::Float(3.));
}

#[test]
fn zonal_weights_missing_zero_negative_and_grid_validation() {
    use terrio::ops::zonal::ZonalWeight;
    let r = raster(2, 1, 1, &[10., 30.], None);
    let mut w = raster(2, 1, 1, &[0., 0.], None);
    let l = zones(vec![Some(rectangle(0., 0., 2., 1.))]);
    let mut inputs = [src("v", &r, 1)];
    let mut run = |w: &RasterDataset| {
        zonal_stats(
            &l,
            &mut inputs,
            &ZonalOptions {
                weights: Some(ZonalWeight::new(w.clone().into(), 1)),
                stats: vec![S::Count, S::Mean, S::WeightedMean, S::WeightedSum],
                ..Default::default()
            },
        )
    };
    let out = run(&w).unwrap();
    assert_eq!(field(&out, 0, "v_count"), &V::Integer(2));
    assert_eq!(field(&out, 0, "v_weighted_sum"), &V::Float(0.));
    assert_eq!(field(&out, 0, "v_weighted_mean"), &V::Null);
    w.raster.set(0, 0, 1, f64::NAN).unwrap();
    let out = run(&w).unwrap();
    assert_eq!(field(&out, 0, "v_weighted_sum"), &V::Null);
    assert_eq!(field(&out, 0, "v_mean"), &V::Float(20.));
    w.raster.set(0, 0, 1, -1.).unwrap();
    assert!(run(&w).is_err());
    w.raster.set(0, 0, 1, 1.).unwrap();
    w.set_geo_transform([1., 1., 0., 1., 0., -1.]);
    assert!(run(&w).is_err());
}

#[test]
fn zonal_uses_selected_band_nodata_and_rejects_weight_overflow() {
    use terrio::ops::zonal::ZonalWeight;
    let mut r = raster(2, 1, 2, &[1., 2., 10., -99.], None);
    r.band_nodata = vec![None, Some(-99.)];
    let l = zones(vec![Some(rectangle(0., 0., 2., 1.))]);
    let mut inputs = [src("v", &r, 2)];
    for fractional in [false, true] {
        let out = zonal_stats(&l, &mut inputs, &ZonalOptions { fractional, ..Default::default() }).unwrap();
        assert_eq!(field(&out, 0, "v_count"), if fractional { &V::Float(1.) } else { &V::Integer(1) });
        assert_eq!(field(&out, 0, "v_mean"), &V::Float(10.));
    }
    let w = raster(2, 1, 1, &[f64::MAX, 1.], None);
    let error = zonal_stats(
        &l,
        &mut inputs,
        &ZonalOptions { stats: vec![S::WeightedSum], weights: Some(ZonalWeight::new(w.clone().into(), 1)), ..Default::default() },
    )
    .unwrap_err();
    assert!(error.to_string().contains("overflow"));
}

#[test]
fn fractional_geometry_errors_identify_the_source_zone() {
    let r = raster(2, 2, 1, &[1.; 4], None);
    let g = G::polygon(vec![Coord::xy(0., 0.), Coord::xy(2., 2.), Coord::xy(0., 2.), Coord::xy(2., 0.), Coord::xy(0., 0.)], vec![]);
    let error =
        zonal_stats(&zones(vec![Some(g)]), &mut [src("v", &r, 1)], &ZonalOptions { fractional: true, ..Default::default() }).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("zone 100"));
    assert!(text.contains("feature 100"));
    assert!(!text.contains("feature 0"));
}

#[test]
fn statistic_errors_come_before_the_weights_raster_is_read() {
    use terrio::ops::zonal::ZonalWeight;
    let r = raster(2, 1, 1, &[1., 1.], None);
    let l = zones(vec![Some(rectangle(0., 0., 2., 1.))]);
    // Band 2 of a one-band weights raster is an error too, found only once its header is read.
    let o = ZonalOptions {
        stats: vec![S::WeightedSum, S::Quantile(150.)],
        weights: Some(ZonalWeight::new(raster(2, 1, 1, &[1., 1.], None).into(), 2)),
        ..Default::default()
    };
    let error = zonal_stats(&l, &mut [src("v", &r, 1)], &o).unwrap_err().to_string();
    assert!(error.contains("quantile percentage 150"), "{error}");
}

#[test]
fn only_requested_weighted_statistics_control_validation() {
    use terrio::ops::zonal::ZonalWeight;
    let r = raster(2, 1, 1, &[1e200, -1e200], None);
    let l = zones(vec![Some(rectangle(0., 0., 2., 1.))]);
    let mut inputs = [src("v", &r, 1)];
    let mut o = ZonalOptions {
        stats: vec![S::WeightedSum, S::WeightedMean],
        weights: Some(ZonalWeight::new(raster(2, 1, 1, &[1., 1.], None).into(), 1)),
        ..Default::default()
    };
    let out = zonal_stats(&l, &mut inputs, &o).unwrap();
    assert_eq!(field(&out, 0, "v_weighted_sum"), &V::Float(0.));
    assert_eq!(field(&out, 0, "v_weighted_mean"), &V::Float(0.));
    o.stats = vec![S::WeightedStdDev];
    assert!(zonal_stats(&l, &mut inputs, &o).unwrap_err().to_string().contains("overflow"));
    let extreme = raster(2, 1, 1, &[1e308, -1e308], None);
    o.stats = vec![S::WeightedSum];
    let mut extreme_inputs = [src("v", &extreme, 1)];
    assert_eq!(field(&zonal_stats(&l, &mut extreme_inputs, &o).unwrap(), 0, "v_weighted_sum"), &V::Float(0.));
    let large = raster(2, 1, 1, &[1e308, 1e308], None);
    let mut large_inputs = [src("v", &large, 1)];
    o.weights = Some(ZonalWeight::new(raster(2, 1, 1, &[10., 10.], None).into(), 1));
    o.stats = vec![S::WeightedMean, S::WeightedStdDev];
    let out = zonal_stats(&l, &mut large_inputs, &o).unwrap();
    assert_eq!(field(&out, 0, "v_weighted_mean"), &V::Float(1e308));
    assert_eq!(field(&out, 0, "v_weighted_stddev"), &V::Float(0.));
    o.stats = vec![S::WeightedSum];
    assert!(zonal_stats(&l, &mut large_inputs, &o).unwrap_err().to_string().contains("overflow"));
    o.stats = vec![S::Count];
    o.weights = Some(ZonalWeight::new(raster(1, 1, 1, &[-1.], None).into(), 99));
    assert_eq!(field(&zonal_stats(&l, &mut inputs, &o).unwrap(), 0, "v_count"), &V::Integer(2));
}

#[test]
fn fractional_invalid_zones_outside_the_grid_are_rejected() {
    let r = raster(2, 2, 1, &[1.; 4], None);
    let g =
        G::polygon(vec![Coord::xy(10., 10.), Coord::xy(12., 12.), Coord::xy(10., 12.), Coord::xy(12., 10.), Coord::xy(10., 10.)], vec![]);
    assert!(zonal_stats(&zones(vec![Some(g)]), &mut [src("v", &r, 1)], &ZonalOptions { fractional: true, ..Default::default() }).is_err());
}

fn one_zone_stats(values: &[f64], stats: &[&str], fractional: bool) -> Layer {
    let r = raster(values.len(), 1, 1, values, None);
    let l = zones(vec![Some(rectangle(0., 0., values.len() as f64, 1.))]);
    let stats = stats.iter().map(|s| S::parse(s).unwrap()).collect();
    let o = ZonalOptions { stats, fractional, ..Default::default() };
    zonal_stats(&l, &mut [src("v", &r, 1)], &o).unwrap()
}

#[test]
fn zonal_quantiles_use_linear_interpolation_on_center_pixels() {
    let out = one_zone_stats(&[1., 1., 1., 5.], &["median", "quantile_25", "quantile_50", "quantile_75"], false);
    assert_eq!(field(&out, 0, "v_quantile_50"), &V::Float(1.));
    assert_eq!(field(&out, 0, "v_quantile_50"), field(&out, 0, "v_median"));
    assert_eq!(field(&out, 0, "v_quantile_25"), &V::Float(1.));
    assert_eq!(field(&out, 0, "v_quantile_75"), &V::Float(2.));
    let out = one_zone_stats(&[1., 2., 3., 4.], &["median", "quantile_10", "quantile_50"], false);
    assert_eq!(field(&out, 0, "v_quantile_50"), &V::Float(2.5));
    assert_eq!(field(&out, 0, "v_quantile_50"), field(&out, 0, "v_median"));
    assert!((field(&out, 0, "v_quantile_10").as_f64().unwrap() - 1.3).abs() < 1e-12);
}

#[test]
fn zonal_fractional_quantiles_follow_exactextract() {
    let out = one_zone_stats(&[1., 1., 1., 5.], &["median", "quantile_50"], true);
    assert_eq!(field(&out, 0, "v_quantile_50"), &V::Float(3.));
    assert_eq!(field(&out, 0, "v_quantile_50"), field(&out, 0, "v_median"));
}

#[test]
fn zonal_mode_minority_variety_break_ties_like_exactextract() {
    for fractional in [false, true] {
        let out = one_zone_stats(&[2., 2., 7., 7., 9.], &["mode", "minority", "variety"], fractional);
        assert_eq!(field(&out, 0, "v_mode"), &V::Float(7.));
        assert_eq!(field(&out, 0, "v_minority"), &V::Float(9.));
        assert_eq!(field(&out, 0, "v_variety"), &V::Integer(3));
        let out = one_zone_stats(&[3., 3., 8., 8.], &["majority", "minority"], fractional);
        assert_eq!(field(&out, 0, "v_mode"), &V::Float(8.));
        assert_eq!(field(&out, 0, "v_minority"), &V::Float(3.));
    }
}

#[test]
fn zonal_variance_and_cv() {
    let out = one_zone_stats(&[2., 4., 4., 4., 5., 5., 7., 9.], &["variance", "stddev", "cv"], false);
    assert_eq!(field(&out, 0, "v_variance"), &V::Float(4.));
    assert_eq!(field(&out, 0, "v_stddev"), &V::Float(2.));
    assert_eq!(field(&out, 0, "v_cv"), &V::Float(0.4));
    let out = one_zone_stats(&[0., 0.], &["cv"], false);
    assert_eq!(field(&out, 0, "v_cv"), &V::Null);
}

#[test]
fn zonal_new_statistics_on_empty_zones() {
    let r = raster(2, 1, 1, &[1., 2.], None);
    let l = zones(vec![Some(rectangle(10., 10., 11., 11.)), None]);
    let names = ["variety", "quantile_50", "mode", "minority", "variance", "cv"];
    for fractional in [false, true] {
        let o = ZonalOptions { stats: names.iter().map(|s| S::parse(s).unwrap()).collect(), fractional, ..Default::default() };
        let out = zonal_stats(&l, &mut [src("v", &r, 1)], &o).unwrap();
        for i in 0..2 {
            assert_eq!(field(&out, i, "v_variety"), &V::Integer(0));
            for name in ["v_quantile_50", "v_mode", "v_minority", "v_variance", "v_cv"] {
                assert_eq!(field(&out, i, name), &V::Null, "{name}");
            }
        }
    }
}

#[test]
fn zonal_rejects_bad_quantiles() {
    for bad in ["quantile_101", "quantile_-1", "quantile_", "quantile_x", "quantile_nan"] {
        assert!(matches!(S::parse(bad), Err(terrio::Error::InvalidArgument(_))), "{bad}");
    }
    assert_eq!(S::parse("majority").unwrap().name(), "mode");
    assert_eq!(S::parse("quantile_2.5").unwrap().name(), "quantile_2.5");
    assert_eq!(S::parse("quantile_25.0").unwrap().name(), "quantile_25");
    assert_eq!(S::parse("coefficient_of_variation").unwrap().name(), "cv");
}

fn all_stats() -> Vec<S> {
    ["count", "min", "max", "mean", "sum", "stddev", "variance", "cv", "median", "quantile_25", "mode", "minority", "variety"]
        .iter()
        .map(|s| S::parse(s).unwrap())
        .collect()
}

#[test]
fn zonal_reuses_zones_across_sources_on_one_grid() {
    let a = raster(4, 4, 1, &(0..16).map(f64::from).collect::<Vec<_>>(), None);
    let b = raster(4, 4, 1, &(0..16).map(|v| f64::from(v * v)).collect::<Vec<_>>(), None);
    let l = zones(vec![Some(rectangle(0., 0., 3., 3.)), Some(rectangle(1., 1., 4., 4.)), None]);
    for fractional in [false, true] {
        let o = ZonalOptions { stats: all_stats(), fractional, ..Default::default() };
        let both = zonal_stats(&l, &mut [src("a", &a, 1), src("b", &b, 1)], &o).unwrap();
        let only_a = zonal_stats(&l, &mut [src("a", &a, 1)], &o).unwrap();
        let only_b = zonal_stats(&l, &mut [src("b", &b, 1)], &o).unwrap();
        let n = l.schema.len() + o.stats.len();
        for i in 0..l.len() {
            assert_eq!(both.features[i].attributes[..n], only_a.features[i].attributes[..]);
            assert_eq!(both.features[i].attributes[n..], only_b.features[i].attributes[l.schema.len()..]);
        }
    }
}

#[test]
fn zonal_lazy_cog_matches_in_memory() {
    let (cols, rows) = (1024, 1024);
    let values: Vec<f64> = (0..cols * rows).map(|i| ((i * 7919) % 1000) as f64).collect();
    let cfg = RasterConfig { cols, rows, bands: 1, data_type: DataType::U16, ..Default::default() };
    let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
    ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cog.tif");
    let options = terrio::RasterWriteOptions {
        driver: Some(terrio::formats::RasterDriver::Cog),
        tile_size: Some(256),
        compression: Some("DEFLATE".into()),
        ..Default::default()
    };
    terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &options).unwrap();
    let mut holed = rectangle(10., 10., 600., 700.);
    let G::Polygon { interiors, .. } = &mut holed else { panic!() };
    let G::Polygon { exterior, .. } = rectangle(100., 100., 300., 300.) else { panic!() };
    interiors.push(exterior);
    let ring = |x0: f64, y0: f64, x1: f64, y1: f64| {
        vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)]
    };
    let multi = G::multi_polygon(vec![(ring(700., 700., 800., 1000.), vec![]), (ring(900.5, 3.2, 1010.7, 90.9), vec![])]);
    let l = zones(vec![Some(holed), Some(multi), Some(rectangle(500., 500., 760.3, 780.2)), None]);
    for fractional in [false, true] {
        let o = ZonalOptions { stats: all_stats(), fractional, ..Default::default() };
        let lazy =
            ZonalSource { raster: terrio::open_raster_lazy(path.to_str().unwrap()).unwrap().into(), bands: vec![ZonalBand::new(1, "v")] };
        let from_lazy = zonal_stats(&l, &mut [lazy], &o).unwrap();
        let eager = terrio::open_raster(path.to_str().unwrap()).unwrap();
        let in_memory = zonal_stats(&l, &mut [src("v", &eager, 1)], &o).unwrap();
        for (a, b) in from_lazy.features.iter().zip(&in_memory.features) {
            assert_eq!(a.attributes, b.attributes);
        }
        assert_eq!(field(&from_lazy, 3, "v_count"), &if fractional { V::Float(0.) } else { V::Integer(0) });
    }
}

#[test]
fn zonal_stats_with_streams_each_band_once() {
    use terrio::ops::zonal::{BandStats, zonal_stats_with};
    let rasters: Vec<RasterDataset> =
        (0..3).map(|k| raster(5, 5, 2, &(0..50).map(|v| f64::from(v * (k + 1))).collect::<Vec<_>>(), None)).collect();
    let mut sources: Vec<ZonalSource> = rasters
        .iter()
        .map(|r| ZonalSource { raster: LazyRaster::from(r.clone()).into(), bands: vec![ZonalBand::new(1, "b1"), ZonalBand::new(2, "b2")] })
        .collect();
    let l = zones(vec![Some(rectangle(0., 0., 3., 3.)), Some(rectangle(2., 2., 5., 5.)), None]);
    let o = ZonalOptions { stats: vec![S::Count, S::Mean, S::Median], ..Default::default() };
    let seen = std::sync::Mutex::new(Vec::<BandStats>::new());
    zonal_stats_with(&l, &mut sources, &o, &|stats| {
        seen.lock().unwrap().push(stats);
        Ok(())
    })
    .unwrap();
    let mut seen = seen.into_inner().unwrap();
    seen.sort_by_key(|s| (s.source, s.band));
    assert_eq!(seen.iter().map(|s| (s.source, s.band)).collect::<Vec<_>>(), [(0, 0), (0, 1), (1, 0), (1, 1), (2, 0), (2, 1)]);
    let table = zonal_stats(&l, &mut sources, &o).unwrap();
    for (i, s) in seen.iter().enumerate() {
        assert_eq!(s.values.len(), 3);
        for (k, values) in s.values.iter().enumerate() {
            assert_eq!(values.len(), 3);
            for (f, v) in values.iter().enumerate() {
                assert_eq!(v, &table.features[f].attributes[1 + i * 3 + k]);
            }
        }
    }
}

#[test]
fn zonal_label_prefix_sanitizes() {
    use terrio::ops::zonal::label_prefix;
    assert_eq!(label_prefix(Some("2024-07-01"), "B04"), "2024_07_01_B04");
    assert_eq!(label_prefix(None, "NDVI"), "NDVI");
    assert_eq!(label_prefix(Some("2024-07-01T10:30:00Z"), "nir 08"), "2024_07_01T10_30_00Z_nir_08");
}

#[test]
fn zonal_sources_without_bands_are_not_opened() {
    let r = raster(2, 2, 1, &[1., 2., 3., 4.], None);
    let l = zones(vec![Some(rectangle(0., 0., 2., 2.))]);
    let unread = ZonalSource { raster: terrio::ops::zonal::ZonalRaster::Path("missing.tif".into()), bands: vec![] };
    let out = zonal_stats(&l, &mut [unread, src("v", &r, 1)], &ZonalOptions { stats: vec![S::Mean], ..Default::default() }).unwrap();
    assert_eq!(out.features[0].attributes.last(), Some(&V::Float(2.5)));
}

#[test]
fn zonal_quantiles_outside_0_to_100_are_refused() {
    let r = raster(2, 2, 1, &[1., 2., 3., 4.], None);
    let l = zones(vec![Some(rectangle(0., 0., 2., 2.))]);
    let o = ZonalOptions { stats: vec![S::Quantile(150.0)], ..Default::default() };
    assert!(zonal_stats(&l, &mut [src("v", &r, 1)], &o).unwrap_err().to_string().contains("between 0 and 100"));
}

/// A 300 x 200 `U16` grid in EPSG:32631 (10 m pixels), nodata 0, its pixels, and zones on it:
/// two overlapping polygons, a multipolygon with parts 120 rows apart, a polygon with a hole,
/// one outside the grid and one smaller than a pixel.
fn reducer_fixture() -> (RasterDataset, Vec<u16>, Layer) {
    let (cols, rows) = (300, 200);
    let mut state = 12345u64;
    let pixels: Vec<u16> = (0..cols * rows)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) % 50) as u16
        })
        .collect();
    assert!(pixels.contains(&0));
    let cfg = RasterConfig { cols, rows, data_type: DataType::U16, nodata: 0., ..Default::default() };
    let mut ds = RasterDataset::new(Raster::from_data(cfg, pixels.iter().map(|&v| f64::from(v)).collect()).unwrap(), true);
    ds.raster.crs.epsg = Some(32631);
    let (x0, y0) = (500_000., 5_000_000.);
    ds.set_geo_transform([x0, 10., 0., y0, 0., -10.]);
    // A ring over pixel columns c0..c1 and rows r0..r1.
    let ring = |c0: f64, r0: f64, c1: f64, r1: f64| {
        let (xa, xb, ya, yb) = (x0 + c0 * 10., x0 + c1 * 10., y0 - r1 * 10., y0 - r0 * 10.);
        vec![Coord::xy(xa, ya), Coord::xy(xb, ya), Coord::xy(xb, yb), Coord::xy(xa, yb), Coord::xy(xa, ya)]
    };
    let mut l = zones(vec![
        Some(G::polygon(ring(10.3, 5.2, 120.7, 90.1), vec![])),
        Some(G::polygon(ring(80.5, 40.5, 200.2, 150.8), vec![])),
        Some(G::multi_polygon(vec![(ring(5., 10., 40.4, 25.), vec![]), (ring(150.2, 145., 190., 170.6), vec![])])),
        Some(G::polygon(ring(200., 20.3, 290.5, 180.), vec![ring(230., 60., 260.2, 120.)])),
        Some(G::polygon(ring(400., 300., 450., 350.), vec![])),
        Some(G::polygon(ring(150.3, 100.3, 150.6, 100.6), vec![])),
    ]);
    l.set_crs_epsg(Some(32631));
    (ds, pixels, l)
}

fn reducer_stats() -> Vec<S> {
    ["count", "min", "max", "mean", "sum", "stddev", "variance", "median", "quantile_25", "mode", "minority", "variety"]
        .iter()
        .map(|s| S::parse(s).unwrap())
        .collect()
}

/// The pixels of `w`, row-major.
fn window_data(pixels: &[u16], cols: usize, w: terrio::Window) -> terrio::raster::RasterData {
    terrio::raster::RasterData::U16((w.y..w.y + w.height).flat_map(|row| pixels[row * cols + w.x..][..w.width].to_vec()).collect())
}

/// Statistics of `l` from a reducer fed the windows of `windows(Some(7))`.
fn reduce_arrays(l: &Layer, ds: &RasterDataset, pixels: &[u16], o: &ZonalOptions) -> Vec<Vec<V>> {
    let reducer = terrio::ops::zonal::ZonalReducer::new(l, Grid::like(ds), o, Some(7)).unwrap();
    assert_eq!(reducer.feature_count(), l.len());
    assert_eq!(reducer.stats(), &o.stats[..]);
    let mut stream = reducer.stream(Some(0.));
    for (i, &w) in reducer.windows().iter().enumerate() {
        stream.add(i, &window_data(pixels, ds.raster.cols, w)).unwrap();
    }
    stream.finish().unwrap()
}

fn pixel_inclusion_modes() -> [ZonalOptions; 3] {
    let o = ZonalOptions { stats: reducer_stats(), ..Default::default() };
    [o.clone(), ZonalOptions { all_touched: true, ..o.clone() }, ZonalOptions { fractional: true, ..o }]
}

/// `zonal_stats` of `l` on `ds`, as `[statistic][feature]`.
fn zonal_stats_table(l: &Layer, ds: &RasterDataset, o: &ZonalOptions) -> Vec<Vec<V>> {
    let out = zonal_stats(l, &mut [src("v", ds, 1)], o).unwrap();
    o.stats.iter().map(|s| (0..l.len()).map(|f| field(&out, f, &format!("v_{}", s.name())).clone()).collect()).collect()
}

#[test]
fn zonal_reducer_matches_zonal_stats() {
    let (ds, pixels, l) = reducer_fixture();
    for o in pixel_inclusion_modes() {
        let expected = zonal_stats_table(&l, &ds, &o);
        assert!(expected[0][..4].iter().all(|c| c != &V::Integer(0) && c != &V::Float(0.)), "{:?}", expected[0]);
        assert_eq!(reduce_arrays(&l, &ds, &pixels, &o), expected, "all_touched {} fractional {}", o.all_touched, o.fractional);
    }
}

#[test]
fn zonal_reducer_zones_in_other_crs() {
    let (ds, pixels, l) = reducer_fixture();
    let l = terrio::vector::reproject_layer(&l, &terrio::crs::SpatialRef::from_epsg(4326).unwrap()).unwrap();
    for o in pixel_inclusion_modes() {
        assert_eq!(reduce_arrays(&l, &ds, &pixels, &o), zonal_stats_table(&l, &ds, &o));
    }
}

#[test]
fn zonal_reducer_without_zones() {
    use terrio::ops::zonal::ZonalReducer;
    let (ds, _, mut l) = reducer_fixture();
    let o = ZonalOptions { stats: vec![S::Count, S::Mean, S::Median, S::Variety], ..Default::default() };
    let outside = l.features.remove(4);
    l.features.clear();
    let reducer = ZonalReducer::new(&l, Grid::like(&ds), &o, None).unwrap();
    assert!(reducer.windows().is_empty());
    assert_eq!(reducer.stream(None).finish().unwrap(), vec![Vec::<V>::new(); 4]);
    l.features.push(outside);
    let reducer = ZonalReducer::new(&l, Grid::like(&ds), &o, None).unwrap();
    assert!(reducer.windows().is_empty());
    assert_eq!(reducer.stream(None).finish().unwrap(), [[V::Integer(0)], [V::Null], [V::Null], [V::Integer(0)]]);
}

#[test]
fn zonal_reducer_rejects_out_of_order() {
    let (ds, pixels, mut l) = reducer_fixture();
    // One zone over rows 0 to 20: three windows of 7 rows.
    l.features.truncate(1);
    l.features[0].geometry = Some(rectangle(500_000., 5_000_000. - 210., 500_100., 5_000_000.));
    let reducer = terrio::ops::zonal::ZonalReducer::new(&l, Grid::like(&ds), &Default::default(), Some(7)).unwrap();
    let windows = reducer.windows();
    assert_eq!(windows.len(), 3);
    let data = |i: usize| window_data(&pixels, ds.raster.cols, windows[i]);
    let e = reducer.stream(Some(0.)).add(1, &data(1)).err().unwrap();
    assert!(e.to_string().contains("expected window 0"), "{e}");
    let e = reducer.stream(Some(0.)).add(0, &terrio::raster::RasterData::U16(vec![1; 3])).err().unwrap();
    assert!(e.to_string().contains("pixels"), "{e}");
    let mut stream = reducer.stream(Some(0.));
    stream.add(0, &data(0)).unwrap();
    assert_eq!(stream.next_window(), 1);
    assert_eq!(format!("{stream:?}"), "ZonalStream { next_window: 1, windows: 3, .. }");
    let e = stream.finish().err().unwrap();
    assert!(e.to_string().contains("missing windows"), "{e}");
    let mut stream = reducer.stream(Some(0.));
    for i in 0..3 {
        stream.add(i, &data(i)).unwrap();
    }
    let e = stream.add(3, &data(2)).err().unwrap();
    assert!(e.to_string().contains("past the last of 3"), "{e}");
}

#[test]
fn zonal_reducer_rejects_weighted() {
    let (ds, _, l) = reducer_fixture();
    let o = ZonalOptions { stats: vec![S::WeightedMean], ..Default::default() };
    let e = terrio::ops::zonal::ZonalReducer::new(&l, Grid::like(&ds), &o, None).err().unwrap();
    assert!(e.to_string().contains("weighted statistics need a weighting raster"), "{e}");
}

#[test]
fn zonal_reducer_rejects_bad_grids_and_empty_stats() {
    use terrio::ops::zonal::ZonalReducer;
    let (ds, _, l) = reducer_fixture();
    let new =
        |grid, stats: Vec<S>| ZonalReducer::new(&l, grid, &ZonalOptions { stats, ..Default::default() }, None).err().unwrap().to_string();
    let grid = |f: &dyn Fn(&mut Grid)| {
        let mut g = Grid::like(&ds);
        f(&mut g);
        g
    };
    for g in [grid(&|g| g.cols = 0), grid(&|g| g.rows = 0)] {
        assert!(new(g, vec![S::Count]).contains("must have pixels"));
    }
    for g in [grid(&|g| g.geo_transform[0] = f64::NAN), grid(&|g| g.geo_transform[1] = f64::INFINITY), grid(&|g| g.geo_transform[1] = 0.)] {
        assert!(new(g, vec![S::Count]).contains("finite and invertible"));
    }
    assert!(new(Grid::like(&ds), vec![]).contains("at least one statistic"));
    let zero = ZonalReducer::new(&l, Grid::like(&ds), &ZonalOptions::default(), Some(0)).err().unwrap();
    assert!(zero.to_string().contains("strip_rows must be at least 1"), "{zero}");
    // The zones have a CRS and the grid has none, or the other way round.
    let mut bare = l.clone();
    bare.set_crs_epsg(None);
    for (zones, grid) in [(&l, grid(&|g| g.crs = Default::default())), (&bare, Grid::like(&ds))] {
        let e = ZonalReducer::new(zones, grid, &ZonalOptions::default(), None).err().unwrap();
        assert!(matches!(e, terrio::Error::Crs(_)) && e.to_string().contains("both declare a CRS"), "{e}");
    }
}
