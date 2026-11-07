//! Rasters whose CRS is declared by an EPSG code alone (a GeoTIFF's `ProjectedCSTypeGeoKey`, a
//! raster built with `CrsInfo::from_epsg`) export the WKT of terrio's EPSG registry, which GDAL
//! 3.13 writes for the code. wbraster's generated WKT, exported before, described some codes
//! otherwise: MGA zone 55 without its false northing (Melbourne at 52°N), NY Long Island in metres
//! with other standard parallels, Web Mercator on the ellipsoid.

use terrio::crs::{SpatialRef, Transformer};
use terrio::{DataType, LazyRaster, Raster, RasterConfig, RasterDataset, RasterWriteOptions, open_raster, write_raster_dataset};

/// EPSG codes and a point in their area of use: MGA zones on GDA94 (whose generated WKT lacked
/// the false northing) and GDA2020, UTM north and south, State Plane in US survey feet (LCC 2SP
/// and Transverse Mercator), Web Mercator, Vicgrid (LCC 2SP), Australian Albers, the British
/// National Grid and geographic CRSs.
const CODES: &[(u32, f64, f64)] = &[
    (28355, 320000.0, 5810000.0),
    (28350, 390000.0, 6460000.0),
    (28356, 330000.0, 6250000.0),
    (7855, 320000.0, 5810000.0),
    (32633, 500000.0, 5000000.0),
    (32755, 320000.0, 5810000.0),
    (2263, 990000.0, 200000.0),
    (2229, 6500000.0, 1850000.0),
    (2262, 600000.0, 1000000.0),
    (2264, 2000000.0, 600000.0),
    (3857, 1000000.0, 5621521.486),
    (3111, 2500000.0, 2400000.0),
    (3577, 1000000.0, -4000000.0),
    (27700, 530000.0, 180000.0),
    (4326, 144.9, -37.8),
    (4269, -100.0, 40.0),
];

/// A 2 x 2 raster whose upper-left corner is at `(x, y)`, in the CRS `crs`.
fn dataset(crs: wbraster::CrsInfo, x: f64, y: f64) -> RasterDataset {
    let config = RasterConfig {
        cols: 2,
        rows: 2,
        bands: 1,
        data_type: DataType::F32,
        x_min: x,
        y_min: y - 2.0,
        cell_size: 1.0,
        crs,
        ..Default::default()
    };
    RasterDataset::new(Raster::new(config), false)
}

/// Longitude and latitude on WGS 84 of a point of `crs`.
fn to_wgs84(crs: &SpatialRef, x: f64, y: f64) -> (f64, f64) {
    Transformer::new(crs, &SpatialRef::from_epsg(4326).unwrap()).unwrap().transform(x, y).unwrap()
}

/// `crs` exports the registry's WKT for `code`, whose text (read back, and read without the code
/// it declares) places `(x, y)` where the code does.
fn assert_registry_wkt(crs: &SpatialRef, code: u32, x: f64, y: f64) {
    let registry = SpatialRef::from_epsg(code).unwrap();
    let wkt = crs.to_wkt().unwrap();
    assert_eq!(crs.epsg, Some(code));
    assert_eq!(wkt, registry.to_wkt().unwrap(), "EPSG:{code}");
    assert!(wkt.ends_with(&format!("AUTHORITY[\"EPSG\",\"{code}\"]]")), "EPSG:{code}: {wkt}");
    let expected = to_wgs84(&registry, x, y);
    let parsed = SpatialRef::parse(&wkt).unwrap();
    assert_eq!(parsed.epsg, Some(code), "EPSG:{code}");
    let text_only = SpatialRef { wkt: Some(wkt), ..Default::default() };
    for got in [to_wgs84(&parsed, x, y), to_wgs84(&text_only, x, y)] {
        assert!((got.0 - expected.0).abs() < 1e-9 && (got.1 - expected.1).abs() < 1e-9, "EPSG:{code}: {got:?} vs {expected:?}");
    }
}

#[test]
fn code_only_geotiff_exports_registry_wkt() {
    let dir = tempfile::tempdir().unwrap();
    for &(code, x, y) in CODES {
        let path = dir.path().join(format!("{code}.tif"));
        let path = path.to_str().unwrap();
        write_raster_dataset(
            &dataset(wbraster::CrsInfo { epsg: Some(code), ..Default::default() }, x, y),
            path,
            &RasterWriteOptions::default(),
        )
        .unwrap();
        assert_registry_wkt(&LazyRaster::open(path).unwrap().crs(), code, x, y);
        assert_registry_wkt(&open_raster(path).unwrap().crs(), code, x, y);
    }
}

#[test]
fn wbraster_epsg_crs_exports_registry_wkt() {
    // wbraster's own WKT for the code, as `CrsInfo::from_epsg` builds it (and wbraster's
    // readers): read as the code, not as the definition a file stores.
    for &(code, x, y) in CODES {
        let crs = wbraster::CrsInfo::from_epsg(code);
        assert!(crs.wkt.is_some(), "EPSG:{code}");
        assert_registry_wkt(&dataset(crs, x, y).crs(), code, x, y);
    }
}

#[test]
fn stored_wkt_with_code_is_kept() {
    // A WKT a file stores alongside the code (an ESRI `.prj`) is not replaced by the registry's.
    let esri = "PROJCS[\"WGS_1984_UTM_Zone_33N\",GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"False_Easting\",500000.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",15.0],PARAMETER[\"Scale_Factor\",0.9996],PARAMETER[\"Latitude_Of_Origin\",0.0],UNIT[\"Meter\",1.0]]";
    let crs = wbraster::CrsInfo { epsg: Some(32633), wkt: Some(esri.to_string()), proj4: None };
    let srs = dataset(crs, 500000.0, 5000000.0).crs();
    assert_eq!(srs.epsg, Some(32633));
    assert_eq!(srs.to_wkt().as_deref(), Some(esri));
}
