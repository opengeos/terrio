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
    let dir = std::env::temp_dir().join(format!("terrio-wkt-file-nul-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut layer = terrio::Layer::new("p");
    layer.add_field(terrio::FieldDef::new("id", terrio::FieldType::Integer));
    let point = terrio::Geometry::Point(terrio::vector::Coord::xy(451_219.33, 5_411_550.2));
    layer.push(terrio::Feature { fid: 0, geometry: Some(point), attributes: vec![terrio::FieldValue::Integer(1)] });
    let path = dir.join("p.shp");
    terrio::write_vector(&[layer], path.to_str().unwrap(), &Default::default()).unwrap();
    std::fs::write(dir.join("p.prj"), prj).unwrap();
    let layers = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers;
    std::fs::remove_dir_all(&dir).ok();
    layers
}

/// A layer whose WKT ends in NULs, as a driver may keep it, is identified as without them.
#[test]
fn a_layer_wkt_padded_with_nuls_is_identified() {
    let mut layer = terrio::Layer::new("p");
    layer.set_crs_wkt(Some(PADDED_PRJ.to_string()));
    assert_eq!(terrio::crs::SpatialRef::from_layer(&layer).epsg, Some(32631));
}
