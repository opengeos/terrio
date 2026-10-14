//! Vector conversion options, rasterize and polygonize on real-world files, with expected values
//! taken from GDAL 3.12 (`ogr2ogr`, `gdal_rasterize` and `gdal_polygonize.py` with the same
//! options; see `scripts/compare_vector_ops_with_gdal.py`).
//!
//! The files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything.

use std::collections::BTreeMap;
use std::path::PathBuf;
use terrio::ops::polygonize::{PolygonizeOptions, polygonize};
use terrio::ops::rasterize::{Grid, RasterizeOptions, rasterize};
use terrio::ops::vector_filter::{VectorTranslateOptions, translate};
use terrio::{DataType, Geometry};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

fn layers(path: &str) -> Vec<terrio::Layer> {
    terrio::open_vector(path, &Default::default()).unwrap().layers
}

fn count(path: &str, options: VectorTranslateOptions) -> usize {
    translate(&layers(path), &options).unwrap()[0].len()
}

#[test]
fn attribute_and_spatial_filters() {
    let Some(countries) = data("countries.geojson") else { return };
    let w = |e: &str| VectorTranslateOptions { where_clause: Some(e.into()), ..Default::default() };
    assert_eq!(count(&countries, w("POP_EST > 50000000")), 27);
    assert_eq!(count(&countries, w("NOT (CONTINENT = 'Africa' OR CONTINENT = 'Asia') AND POP_EST >= 1e7")), 30);
    assert_eq!(count(&countries, w("continent IN ('Europe', 'Oceania')")), 46);
    assert_eq!(count(&countries, w("ADMIN LIKE '%Republic%' OR ADMIN LIKE '%republic%'")), 6);
    assert_eq!(count(&countries, w("ADMIN LIKE '%republic%'")), 0);
    let spat = VectorTranslateOptions { spatial_filter: Some((-10.0, 35.0, 30.0, 60.0)), ..Default::default() };
    assert_eq!(count(&countries, spat), 42);

    let Some(counties) = data("UScounties.gpkg") else { return };
    let spat = VectorTranslateOptions {
        spatial_filter: Some((-9_000_000.0, 4_000_000.0, -8_500_000.0, 4_500_000.0)),
        spatial_filter_srs: Some(terrio::crs::SpatialRef::from_epsg(3857).unwrap()),
        ..Default::default()
    };
    assert_eq!(count(&counties, spat), 151);
    assert_eq!(count(&counties, w("STATE NOT IN ('TX', 'GA') AND LSAD <> 'County'")), 214);
}

#[test]
fn select_sql_explode() {
    let Some(places) = data("ne_places/ne_10m_populated_places_simple.shp") else { return };
    let o = VectorTranslateOptions {
        where_clause: Some("adm0name = 'China'".into()),
        select: Some(vec!["NAME".into(), "pop_max".into()]),
        limit: Some(25),
        ..Default::default()
    };
    let out = translate(&layers(&places), &o).unwrap();
    assert_eq!(out[0].len(), 25);
    assert_eq!(out[0].schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["name", "pop_max"]);
    assert_eq!(
        count(&places, VectorTranslateOptions { where_clause: Some("pop_max BETWEEN 1000000 AND 2000000".into()), ..Default::default() }),
        289
    );

    let Some(countries) = data("countries.geojson") else { return };
    let sql = "SELECT ADMIN, POP_EST AS pop FROM countries WHERE CONTINENT = 'Asia' ORDER BY POP_EST DESC";
    let out = translate(&layers(&countries), &VectorTranslateOptions { sql: Some(sql.into()), ..Default::default() }).unwrap();
    assert_eq!(out[0].len(), 47);
    assert_eq!(out[0].features[0].attributes[0].as_str(), Some("China"));
    assert_eq!(count(&countries, VectorTranslateOptions { explode_collections: true, ..Default::default() }), 280);
}

#[test]
fn rasterize_like_gdal() {
    let Some(cables) = data("cables.geojson") else { return };
    let o = RasterizeOptions {
        burn_values: vec![1.0],
        resolution: Some((0.1, 0.1)),
        all_touched: true,
        data_type: Some(DataType::U8),
        ..Default::default()
    };
    let r = rasterize(&layers(&cables), &o).unwrap();
    assert_eq!((r.raster.cols, r.raster.rows), (3601, 1333));
    assert_eq!(r.raster.band_to_vec_f64(0).iter().sum::<f64>(), 183_708.0);

    let Some(tn) = data("TN_Counties.geojson") else { return };
    let o = RasterizeOptions {
        attribute: Some("POPULATION".into()),
        resolution: Some((0.01, 0.01)),
        data_type: Some(DataType::I32),
        ..Default::default()
    };
    let r = rasterize(&layers(&tn), &o).unwrap();
    assert_eq!((r.raster.cols, r.raster.rows), (866, 170));
    let gt = r.geo_transform();
    assert!((gt[0] - -90.31044831499997).abs() < 1e-12 && (gt[3] - 36.68468319800008).abs() < 1e-12, "{gt:?}");
    assert_eq!(r.raster.band_to_vec_f64(0).iter().sum::<f64>(), 9_673_675_739.0);
}

/// The signed area of a closed ring.
fn area(r: &[(f64, f64)]) -> f64 {
    r.windows(2).map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1).sum::<f64>() / 2.0
}

fn classes(l: &terrio::Layer) -> BTreeMap<i64, (usize, f64)> {
    let mut out = BTreeMap::new();
    for f in &l.features {
        let Some(Geometry::Polygon { exterior, interiors }) = &f.geometry else { panic!("not a polygon") };
        let a = std::iter::once(exterior).chain(interiors).map(|r| area(&r.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>())).sum::<f64>();
        let e = out.entry(f.attributes[0].as_i64().unwrap()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += a;
    }
    out
}

#[test]
fn polygonize_like_gdal() {
    let Some(p) = data("OPERA_L3_DSWx_WTR.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let l = polygonize(&ds, &Default::default()).unwrap();
    assert_eq!(l.len(), 6967);
    let expected =
        [(0, 308, 10_917_668_700.0), (1, 1798, 267_851_700.0), (2, 4760, 7_501_500.0), (252, 56, 25_911_000.0), (253, 45, 49_580_100.0)];
    let got = classes(&l);
    for (v, n, area) in expected {
        assert_eq!(got[&v].0, n, "polygons of value {v}");
        assert!((got[&v].1 - area).abs() < 1e-3, "area of value {v}: {}", got[&v].1);
    }

    // Rasterizing the polygons onto the input grid gives the input back.
    let Some(p) = data("nlcd_knoxville.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let l = polygonize(&ds, &Default::default()).unwrap();
    assert_eq!(l.len(), 149_456);
    assert_eq!(
        l.features
            .iter()
            .map(|f| if let Some(Geometry::Polygon { interiors, .. }) = &f.geometry { interiors.len() } else { 0 })
            .sum::<usize>(),
        14_487
    );
    let eight = polygonize(&ds, &PolygonizeOptions { connectedness8: true, ..Default::default() }).unwrap();
    assert_eq!(eight.len(), 93_703);
    let o =
        RasterizeOptions { attribute: Some("DN".into()), grid: Some(Grid::like(&ds)), data_type: Some(DataType::U8), ..Default::default() };
    let back = rasterize(&[l], &o).unwrap();
    assert_eq!(back.raster.band_to_vec_f64(0), ds.raster.band_to_vec_f64(0));
}
