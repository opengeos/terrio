//! WKT read from a file ends at its first NUL, as GDAL reads it (a C string). GDAL 3.10.3's
//! `ogrinfo -so` reports a shapefile whose `.prj` is ESRI's WGS 84 / UTM zone 31N followed by two
//! NUL bytes as EPSG:32631, and one with a NUL inside the CRS's name as having no CRS ("missing ,
//! or ]"): terrio reads both so.

/// ESRI's WGS 84 / UTM zone 31N, as ArcGIS writes it, followed by two NUL bytes.
const PADDED_PRJ: &str = "PROJCS[\"WGS_1984_UTM_Zone_31N\",GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"False_Easting\",500000.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",3.0],PARAMETER[\"Scale_Factor\",0.9996],PARAMETER[\"Latitude_Of_Origin\",0.0],UNIT[\"Meter\",1.0]]\0\0";

#[test]
fn a_prj_padded_with_nuls_is_identified() {
    let layers = shapefile_with_prj("padded", PADDED_PRJ);
    assert_eq!(layers[0].crs_epsg(), Some(32631));
    assert_eq!(terrio::crs::SpatialRef::from_layer(&layers[0]).epsg, Some(32631));
}

#[test]
fn a_prj_with_a_nul_in_its_name_has_no_crs() {
    let prj = PADDED_PRJ.trim_end_matches('\0').replace("WGS_1984_UTM_Zone_31N", "WGS_1984_UTM\0_Zone_31N");
    let layers = shapefile_with_prj("named", &prj);
    assert_eq!(layers[0].crs_epsg(), None);
    let crs = terrio::crs::SpatialRef::from_layer(&layers[0]);
    assert_eq!(crs.epsg, None);
    let wgs84 = terrio::crs::SpatialRef::from_epsg(4326).unwrap();
    assert!(terrio::crs::Transformer::new(&crs, &wgs84).is_err());
}

/// A one-point shapefile with this `.prj`, as terrio reads it.
fn shapefile_with_prj(name: &str, prj: &str) -> Vec<terrio::Layer> {
    // Removed when dropped, on a panic too.
    let tmp = tempfile::Builder::new().prefix(&format!("terrio-wkt-file-nul-{name}-")).tempdir().unwrap();
    let dir = tmp.path();
    let mut layer = terrio::Layer::new("p");
    layer.add_field(terrio::FieldDef::new("id", terrio::FieldType::Integer));
    let point = terrio::Geometry::Point(terrio::vector::Coord::xy(451_219.33, 5_411_550.2));
    layer.push(terrio::Feature { fid: 0, geometry: Some(point), attributes: vec![terrio::FieldValue::Integer(1)] });
    let path = dir.join("p.shp");
    terrio::write_vector(&[layer], path.to_str().unwrap(), &Default::default()).unwrap();
    std::fs::write(dir.join("p.prj"), prj).unwrap();
    terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers
}

/// A layer whose WKT ends in NULs, as a driver may keep it, is identified as without them.
#[test]
fn a_layer_wkt_padded_with_nuls_is_identified() {
    let mut layer = terrio::Layer::new("p");
    layer.set_crs_wkt(Some(PADDED_PRJ.to_string()));
    assert_eq!(terrio::crs::SpatialRef::from_layer(&layer).epsg, Some(32631));
}

/// PROJ 9.9.0's PROJJSON (pyproj 3.8.0, `scripts/gen_wkt1_proj_reads.py`) of
/// EPSG:32631 with a NUL (`\u0000`) in a parameter's name, and of Mollweide with one in its name.
fn projjson_with_a_nul() -> [(String, Option<u32>, &'static str); 2] {
    let data: serde_json::Value = serde_json::from_str(include_str!("../../terrio-crs/tests/data/wkt1_proj_reads.json")).unwrap();
    let text = |key: &str| data[key]["projjson"].as_str().unwrap().to_string();
    [(text("projjson"), Some(32631), "\"Latitude of\""), (text("mollweide_projjson"), None, "\"Moll\"")]
}

/// A CRS read from PROJJSON with a NUL in a name, as PROJ 9.9.0 reads it, keeps it in its stored
/// WKT. Set on a layer, its WKT is written with the name cut at the NUL, as PROJ gives such a
/// name (`CRS.name` of `"WGS\u0000 84 / UTM zone 31N"` is `WGS`; PROJ's own WKT of it ends at
/// the NUL), so the layer reads back as the CRS: with its EPSG code, and without one.
#[test]
fn a_crs_with_a_nul_in_its_name_round_trips_through_a_layer() {
    use terrio::crs::{SpatialRef, Transformer};
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (json, epsg, cut) in projjson_with_a_nul() {
        let crs = SpatialRef::from_projjson(&json).unwrap();
        assert!(crs.to_wkt().is_some_and(|w| w.contains('\0')));
        assert_eq!(crs.epsg, epsg);
        let mut layer = terrio::Layer::new("p");
        crs.apply_to_layer(&mut layer);
        assert!(layer.crs_wkt().is_some_and(|w| !w.contains('\0') && w.contains(cut)), "{:?}", layer.crs_wkt());
        let back = SpatialRef::from_layer(&layer);
        assert_eq!(back.epsg, epsg);
        assert_eq!(back.to_proj4(), crs.to_proj4());
        let (a, b) = (Transformer::new(&crs, &wgs84).unwrap(), Transformer::new(&back, &wgs84).unwrap());
        assert_eq!(b.transform(500_000.0, 5_000_000.0).unwrap(), a.transform(500_000.0, 5_000_000.0).unwrap());
    }
}

/// A raster rasterized in a CRS with a NUL in its name (Mollweide's PROJ 9.9.0 PROJJSON) has
/// that CRS: its stored WKT has the name cut at the NUL, so it reads back.
#[test]
fn a_raster_made_in_a_crs_with_a_nul_in_its_name_has_that_crs() {
    use terrio::crs::SpatialRef;
    use terrio::ops::rasterize::{RasterizeOptions, rasterize};
    let [_, (json, _, _)] = projjson_with_a_nul();
    let crs = SpatialRef::from_projjson(&json).unwrap();
    let mut layer = terrio::Layer::new("p");
    let point = terrio::Geometry::Point(terrio::vector::Coord::xy(5.0, 5.0));
    layer.push(terrio::Feature { fid: 0, geometry: Some(point), attributes: vec![] });
    let options = RasterizeOptions {
        burn_values: vec![1.0],
        extent: Some((0.0, 0.0, 10.0, 10.0)),
        resolution: Some((1.0, 1.0)),
        assign_srs: Some(crs.clone()),
        ..Default::default()
    };
    let raster = rasterize(&[layer], &options).unwrap();
    let back = raster.crs();
    assert_eq!(back.to_proj4(), crs.to_proj4());
    assert!(back.to_wkt().is_some_and(|w| !w.contains('\0') && w.contains("\"Moll\"")), "{:?}", back.to_wkt());
}

/// A shapefile whose `.prj` is padded with NULs is copied as the same `.prj` without them is:
/// to a GeoPackage as EPSG:32631, as GDAL 3.10.3's `ogr2ogr` writes it (`srs_id` 32631,
/// organization EPSG), and to a shapefile with the same `.prj` (terrio writes the EPSG code's
/// WKT for both; `ogr2ogr` keeps the ESRI WKT, without the NULs). One with a NUL in its name
/// is copied without a CRS, as by `ogr2ogr`.
#[test]
fn a_prj_padded_with_nuls_is_copied_as_without_them() {
    let copy = |name: &str, prj: &str, ext: &str| {
        let layers = shapefile_with_prj(name, prj);
        let tmp = tempfile::Builder::new().prefix("terrio-wkt-file-nul-copy-").tempdir().unwrap();
        let path = tmp.path().join(format!("c.{ext}"));
        terrio::write_vector(&layers, path.to_str().unwrap(), &Default::default()).unwrap();
        let back = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers;
        let prj = std::fs::read(tmp.path().join("c.prj")).ok();
        (back[0].crs_epsg(), prj)
    };
    assert_eq!(copy("gpkg", PADDED_PRJ, "gpkg").0, Some(32631));
    let plain = copy("plain", PADDED_PRJ.trim_end_matches('\0'), "shp");
    let padded = copy("padded-shp", PADDED_PRJ, "shp");
    assert!(plain.1.as_deref().is_some_and(|p| !p.contains(&0)));
    assert_eq!(padded, plain);
    // With a NUL inside the CRS's name, the CRS is none (GDAL: "missing , or ]"), and none is
    // written: GDAL 3.10.3's `ogr2ogr` writes no `.prj` (and GeoPackage `srs_id` 99999).
    let named = PADDED_PRJ.trim_end_matches('\0').replace("WGS_1984_UTM_Zone_31N", "WGS_1984_UTM\0_Zone_31N");
    assert_eq!(copy("named-shp", &named, "shp"), (None, None));
}
