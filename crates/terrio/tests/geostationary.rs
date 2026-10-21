//! The geostationary satellite view (`+proj=geos`) against PROJ, for both sweep axes: GOES-16
//! (`+sweep=x`, GRS80) and Meteosat SEVIRI (`+sweep=y`, its own ellipsoid). The reference points
//! are pyproj's (PROJ 9.7.1), from the geographic CRS on the same ellipsoid, so that no datum
//! transformation is involved.

use terrio::crs::{SpatialRef, Transformer};

const GOES: &str = "+proj=geos +h=35786023 +lon_0=-75 +sweep=x +ellps=GRS80 +units=m +no_defs";
const GOES_GEOG: &str = "+proj=longlat +ellps=GRS80 +no_defs";

const SEVIRI: &str = "+proj=geos +h=35785831 +lon_0=0 +sweep=y +a=6378169 +b=6356583.8 +units=m +no_defs";
const SEVIRI_GEOG: &str = "+proj=longlat +a=6378169 +b=6356583.8 +no_defs";

/// Longitude and latitude in degrees and PROJ's x and y in metres, from nadir to near the limb.
const GOES_POINTS: &[(f64, f64, f64, f64)] = &[
    (-75.0, 0.0, 0.0, 0.0),
    (-74.0, 0.5, 111305.36574654478, 55284.532964953985),
    (-70.0, 5.0, 552934.1056737453, 551396.2299694433),
    (-90.0, 0.0, -1639670.198596629, 0.0),
    (-75.0, -15.0, 0.0, -1629136.0730140982),
    (-55.0, 35.0, 1709204.1237692675, 3483923.2637939486),
    (-105.0, -40.0, -2291893.2531235996, -3833062.4133029096),
    (-30.0, 10.0, 4192877.523456901, 1043479.831017525),
    (-135.0, 20.0, -4709774.753098331, 1978596.4061921614),
    (-5.0, -5.0, 5303340.1016310975, -494092.038764708),
    (-65.0, 75.0, 251009.50191380794, 5378906.753128008),
    (-115.0, -65.0, -1534075.7246537933, -5104219.254082671),
    (3.0, 0.0, 5425023.24926228, 0.0),
    (-20.0, 55.0, 2655764.883775899, 4620477.366198244),
];

const SEVIRI_POINTS: &[(f64, f64, f64, f64)] = &[
    (0.0, 0.0, 0.0, 0.0),
    (1.0, 0.5, 111306.05726802953, 55281.05726193307),
    (5.0, 5.0, 553002.6586090655, 551298.5599665294),
    (-15.0, 0.0, -1639678.2986153536, 0.0),
    (0.0, -15.0, 0.0, -1629045.541578382),
    (20.0, 35.0, 1717351.513809403, 3479775.726167576),
    (-30.0, -40.0, -2305110.3781973496, -3825005.6867915546),
    (45.0, 10.0, 4194667.166002093, 1036264.0035360178),
    (-60.0, 20.0, -4716936.884713916, 1961360.1333948574),
    (70.0, -5.0, 5303859.66907667, -488647.12599109),
    (10.0, 75.0, 253880.5203321382, 5378621.102131265),
    (-40.0, -65.0, -1549842.1156801214, -5099336.579691021),
    (78.0, 0.0, 5425042.746693887, 0.0),
    (55.0, 55.0, 2678042.931082768, 4607519.426421587),
];

/// Points the satellite cannot see, relative to the sub-satellite longitude: PROJ refuses them.
const HIDDEN: &[(f64, f64)] = &[(85.0, 0.0), (0.0, 85.0), (-120.0, 10.0), (180.0, 0.0)];

/// Coordinates off the Earth's disk: PROJ refuses them.
const OFF_DISK: &[(f64, f64)] = &[(5.6e6, 0.0), (0.0, 5.6e6), (4.2e6, 4.2e6)];

fn check(geos: &str, geog: &str, lon0: f64, points: &[(f64, f64, f64, f64)]) {
    let geos = SpatialRef::from_proj4(geos).unwrap();
    let geog = SpatialRef::from_proj4(geog).unwrap();
    let fwd = Transformer::new(&geog, &geos).unwrap();
    let inv = Transformer::new(&geos, &geog).unwrap();
    for &(lon, lat, x, y) in points {
        let (tx, ty) = fwd.transform(lon, lat).unwrap();
        assert!((tx - x).abs() < 1e-3 && (ty - y).abs() < 1e-3, "forward ({lon}, {lat}): ({tx}, {ty}), PROJ ({x}, {y})");
        let (tlon, tlat) = inv.transform(x, y).unwrap();
        assert!((tlon - lon).abs() < 1e-8 && (tlat - lat).abs() < 1e-8, "inverse ({x}, {y}): ({tlon}, {tlat}), PROJ ({lon}, {lat})");
    }
    for &(dlon, lat) in HIDDEN {
        let r = fwd.transform(lon0 + dlon, lat);
        assert!(r.is_err() || !r.unwrap().0.is_finite(), "forward of hidden ({}, {lat}) succeeded", lon0 + dlon);
    }
    for &(x, y) in OFF_DISK {
        let r = inv.transform(x, y);
        assert!(r.is_err() || !r.unwrap().0.is_finite(), "inverse of off-disk ({x}, {y}) succeeded");
    }
}

#[test]
fn goes16_sweep_x_matches_proj() {
    check(GOES, GOES_GEOG, -75.0, GOES_POINTS);
}

#[test]
fn seviri_sweep_y_matches_proj() {
    check(SEVIRI, SEVIRI_GEOG, 0.0, SEVIRI_POINTS);
}

#[test]
fn scan_angles_past_a_right_angle_are_refused() {
    let geos = SpatialRef::from_proj4(GOES).unwrap();
    let inv = Transformer::new(&geos, &SpatialRef::from_proj4(GOES_GEOG).unwrap()).unwrap();
    // A y scan angle near pi (the outer angle for +sweep=x): the ray points away from the Earth.
    let y = 35786023.0 * 3.0;
    let r = inv.transform(0.0, y);
    assert!(r.is_err() || !r.unwrap().0.is_finite(), "inverse of (0, {y}) succeeded");
}

// ─── WKT (#119) ──────────────────────────────────────────────────────────────────────────────

/// GDAL's WKT 1 of GOES-16 (`+sweep=x`): the sweep axis is only in the `PROJ4` extension.
const GOES_GDAL_WKT: &str = r#"PROJCS["unknown",GEOGCS["unknown",DATUM["Unknown based on GRS 1980 ellipsoid",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]],PROJECTION["Geostationary_Satellite"],PARAMETER["central_meridian",-75],PARAMETER["satellite_height",35786023],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],EXTENSION["PROJ4","+proj=geos +sweep=x +lon_0=-75 +h=35786023 +x_0=0 +y_0=0 +ellps=GRS80 +units=m +no_defs"]]"#;
/// GDAL's WKT 1 of the same CRS with `+sweep=y`: no extension.
const GOES_Y_GDAL_WKT: &str = r#"PROJCS["unknown",GEOGCS["unknown",DATUM["Unknown based on GRS 1980 ellipsoid",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]],PROJECTION["Geostationary_Satellite"],PARAMETER["central_meridian",-75],PARAMETER["satellite_height",35786023],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#;
/// ESRI's WKT 1 of the `+sweep=y` CRS, as GDAL writes it in a GeoTIFF's PE string: `Height`.
const GOES_Y_ESRI_WKT: &str = r#"PROJCS["unknown",GEOGCS["GCS_unknown",DATUM["D_Unknown_based_on_GRS_1980_ellipsoid",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Geostationary_Satellite"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Longitude_Of_Center",-75.0],PARAMETER["Height",35786023.0],PARAMETER["Option",0.0],UNIT["Meter",1.0]]"#;
/// PROJ's WKT 2 of both.
const GOES_WKT2: &str = r#"PROJCRS["unknown",BASEGEOGCRS["unknown",DATUM["Unknown based on GRS 1980 ellipsoid",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1],ID["EPSG",7019]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],CONVERSION["unknown",METHOD["Geostationary Satellite (Sweep X)"],PARAMETER["Longitude of natural origin",-75,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Satellite Height",35786023,LENGTHUNIT["metre",1,ID["EPSG",9001]]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1,ID["EPSG",9001]]],AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1,ID["EPSG",9001]]]]"#;

/// The coordinates of WGS 84 points in a CRS.
fn from_wgs84(crs: &str) -> terrio::Result<Vec<(f64, f64)>> {
    let t = Transformer::new(&SpatialRef::parse("EPSG:4326")?, &SpatialRef::parse(crs)?)?;
    GOES_POINTS.iter().take(10).map(|p| t.transform(p.0, p.1)).collect()
}

fn assert_same(crs: &str, reference: &str, what: &str) {
    let (a, b) = (from_wgs84(crs).unwrap_or_else(|e| panic!("{what}: {e}")), from_wgs84(reference).unwrap());
    let d = a.iter().zip(&b).map(|(p, q)| (p.0 - q.0).hypot(p.1 - q.1)).fold(0.0, f64::max);
    assert!(d < 1e-3, "{what}: {d} m from the PROJ string");
}

#[test]
fn geostationary_wkt_keeps_its_sweep_axis_and_height() {
    let goes_y = GOES.replace("+sweep=x", "+sweep=y");
    // terrio's own WKT.
    let own = SpatialRef::parse(GOES).unwrap().to_wkt().unwrap();
    assert!(own.contains("EXTENSION[\"PROJ4\"") && own.contains("+sweep=x"), "{own}");
    assert_same(&own, GOES, "terrio's WKT, sweep x");
    let own_y = SpatialRef::parse(&goes_y).unwrap().to_wkt().unwrap();
    assert!(!own_y.contains("EXTENSION"), "{own_y}");
    assert_same(&own_y, &goes_y, "terrio's WKT, sweep y");
    // GDAL's and ESRI's WKT 1, and PROJ's WKT 2.
    assert_same(GOES_GDAL_WKT, GOES, "GDAL's WKT 1, sweep x");
    assert_same(GOES_Y_GDAL_WKT, &goes_y, "GDAL's WKT 1, sweep y");
    assert_same(GOES_Y_ESRI_WKT, &goes_y, "ESRI's WKT 1 (Height)");
    assert_same(GOES_WKT2, GOES, "WKT 2, sweep x");
    assert_same(&GOES_WKT2.replace("(Sweep X)", "(Sweep Y)"), &goes_y, "WKT 2, sweep y");
    // And PROJ's coordinates, through GDAL's WKT.
    let t = Transformer::new(&SpatialRef::parse(GOES_GEOG).unwrap(), &SpatialRef::parse(GOES_GDAL_WKT).unwrap()).unwrap();
    for &(lon, lat, x, y) in GOES_POINTS.iter().take(10) {
        let (tx, ty) = t.transform(lon, lat).unwrap();
        assert!((tx - x).hypot(ty - y) < 1e-3, "({lon}, {lat}): ({tx}, {ty}) vs PROJ ({x}, {y})");
    }
}

#[test]
fn geostationary_wkt_without_a_height_is_refused() {
    let gdal = GOES_Y_GDAL_WKT.replace(r#"PARAMETER["satellite_height",35786023],"#, "");
    assert!(from_wgs84(&gdal).is_err());
    let esri = GOES_Y_ESRI_WKT.replace(r#"PARAMETER["Height",35786023.0],"#, "");
    assert!(from_wgs84(&esri).is_err());
    let wkt2 = GOES_WKT2.replace(r#"PARAMETER["Satellite Height",35786023,LENGTHUNIT["metre",1,ID["EPSG",9001]]],"#, "");
    assert!(from_wgs84(&wkt2).is_err());
}

#[test]
fn geostationary_geotiff_keeps_its_sweep_axis() {
    use terrio::raster::RasterData;
    use terrio::{DataType, RasterConfig, RasterDataset};
    // GeoTIFF has no geostationary method: the CRS goes in an ESRI PE string, which keeps the
    // sweep axis in its `PROJ4` extension, as GDAL reads it.
    let crs = SpatialRef::parse(GOES).unwrap();
    let cfg = RasterConfig { cols: 4, rows: 4, bands: 1, data_type: DataType::F32, crs: crs.to_crs_info(), ..Default::default() };
    let mut ds = RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::F32(vec![0.0; 16])).unwrap(), false);
    ds.set_geo_transform([0.0, 1000.0, 0.0, 0.0, 0.0, -1000.0]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("goes.tif").to_string_lossy().into_owned();
    terrio::write_raster_dataset(&ds, &path, &Default::default()).unwrap();
    let wkt = terrio::open_raster(&path).unwrap().crs().to_wkt().unwrap();
    assert!(wkt.contains("EXTENSION[\"PROJ4\"") && wkt.contains("+sweep=x"), "{wkt}");
    assert_same(&wkt, GOES, "terrio's GeoTIFF, sweep x");
}
