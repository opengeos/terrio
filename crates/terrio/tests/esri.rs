//! CRSs of PROJ's ESRI authority (`ESRI:54009`): parsed as PROJ parses them, transformed as PROJ
//! transforms them, and reported under the ESRI authority as GDAL reports them, never as EPSG
//! codes.

use terrio::crs::{SpatialRef, Transformer};
use terrio::raster::RasterData;
use terrio::{DataType, RasterConfig, RasterDataset};

/// The ESRI PE string GDAL 3.12 writes in a GeoTIFF's `PCSCitationGeoKey` for `ESRI:54009`.
const GDAL_MOLLWEIDE: &str = r#"PROJCS["World_Mollweide",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Mollweide"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0],UNIT["Meter",1.0]]"#;

#[test]
fn esri_codes_transform_as_proj() {
    // The reference uses no grid (PROJ 9.7, network off): neither does terrio.
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::crs::set_network_enabled(Some(false));
    // pyproj.Transformer.from_crs("EPSG:4326", code, always_xy=True).transform(lon, lat)
    let cases = [
        ("ESRI:54009", -100.0, 40.0, -8492765.18278967, 4789399.149897031),
        ("esri:54009", 120.0, -60.0, 7782767.166184467, -6876758.993328802),
        ("urn:ogc:def:crs:ESRI::54009", -100.0, 40.0, -8492765.18278967, 4789399.149897031),
        ("ESRI:102003", -100.0, 40.0, -338390.5875508669, 287313.8794652284),
        ("ESRI:102003", -75.0, 45.0, 1640181.9421992023, 1020895.4097072422),
        ("ESRI:102100", 10.0, 50.0, 1113194.9079327357, 6446275.841017159),
        ("ESRI:54030", -100.0, 40.0, -8706986.515662674, 4276351.583838239),
    ];
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (code, lon, lat, x, y) in cases {
        let crs = SpatialRef::parse(code).unwrap();
        assert_eq!((crs.epsg, crs.label()), (None, format!("ESRI:{}", code.rsplit(':').next().unwrap())), "{code}");
        let (tx, ty) = Transformer::new(&wgs84, &crs).unwrap().transform(lon, lat).unwrap();
        assert!((tx - x).abs() < 1e-3 && (ty - y).abs() < 1e-3, "{code}: ({tx}, {ty}) vs PROJ's ({x}, {y})");
        let (tlon, tlat) = Transformer::new(&crs, &wgs84).unwrap().transform(x, y).unwrap();
        assert!((tlon - lon).abs() < 1e-8 && (tlat - lat).abs() < 1e-8, "{code} inverse: ({tlon}, {tlat})");
    }
    // The CRS's name and the WKT GDAL writes for it.
    let crs = SpatialRef::parse("ESRI:102003").unwrap();
    assert_eq!(crs.name().unwrap(), "USA_Contiguous_Albers_Equal_Area_Conic");
    assert!(crs.to_wkt().unwrap().ends_with(r#"AUTHORITY["ESRI","102003"]]"#));
}

#[test]
fn esri_codes_terrio_cannot_compute_are_refused() {
    // World Sinusoidal, which terrio computes on the sphere where PROJ uses the ellipsoid, Goode's
    // homolosine, which terrio does not compute, and a code PROJ does not know.
    for code in ["ESRI:54008", "ESRI:54052", "ESRI:1"] {
        assert!(SpatialRef::parse(code).is_err(), "{code}");
    }
}

#[test]
fn esri_crss_are_identified_and_reported_as_esri() {
    // GDAL's ESRI PE string, which declares no code: identified by its name and definition.
    let crs = SpatialRef::from_wkt(GDAL_MOLLWEIDE);
    assert_eq!((crs.epsg, crs.esri, crs.label()), (None, Some(54009), "ESRI:54009".to_string()));
    assert_eq!(crs.authority(), Some(("ESRI", 54009)));
    // The same name on another projection is not ESRI:54009.
    let other = GDAL_MOLLWEIDE.replace(r#"PARAMETER["Central_Meridian",0.0]"#, r#"PARAMETER["Central_Meridian",10.0]"#);
    assert_eq!(SpatialRef::from_wkt(&other).authority(), None);
    // A declared ESRI code wins over the EPSG code of the same definition (EPSG:3857).
    let web = SpatialRef::parse("ESRI:102100").unwrap();
    assert_eq!(SpatialRef::from_wkt(&web.to_wkt().unwrap()).authority(), Some(("ESRI", 102100)));
    // wbprojection's pseudo-EPSG code for Mollweide is ESRI's.
    assert_eq!(SpatialRef::parse("EPSG:54009").unwrap().authority(), Some(("ESRI", 54009)));
    // Other ESRI codes are not EPSG codes, as in PROJ.
    assert!(SpatialRef::parse("EPSG:102003").is_err());
}

#[test]
fn esri_geotiff_is_reported_as_esri() {
    // A GeoTIFF with the ESRI PE string in its citation, as GDAL writes ESRI:54009.
    let crs = SpatialRef::from_wkt(GDAL_MOLLWEIDE);
    let cfg = RasterConfig { cols: 4, rows: 4, bands: 1, data_type: DataType::F32, crs: crs.to_crs_info(), ..Default::default() };
    let mut ds = RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::F32(vec![0.0; 16])).unwrap(), false);
    ds.set_geo_transform([0.0, 1000.0, 0.0, 0.0, 0.0, -1000.0]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mollweide.tif").to_string_lossy().into_owned();
    terrio::write_raster_dataset(&ds, &path, &Default::default()).unwrap();
    let read = terrio::open_raster(&path).unwrap();
    assert_eq!(read.crs().authority_code().as_deref(), Some("ESRI:54009"));
    assert_eq!(read.crs().to_wkt().as_deref(), Some(GDAL_MOLLWEIDE));
    let text = terrio::info::info_path(&path, &Default::default()).unwrap().to_text();
    assert!(text.contains("CRS: ESRI:54009 (World_Mollweide)") && !text.contains("EPSG:54009"), "{text}");
}
