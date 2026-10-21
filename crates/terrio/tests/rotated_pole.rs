//! Rotated-pole grids (#129): CORDEX EUR-11's grid, its north pole at 39.25° N, 162° W, as a PROJ
//! string, as the WKT 2 forms PROJ writes (on COSMO-CLM's sphere, and on WGS 84 with a `DATUM`
//! and with an `ENSEMBLE` node) and as a CF grid mapping in a netCDF file, transformed forward and
//! back as PROJ 9.7 transforms them (`pyproj.Transformer.from_crs("EPSG:4326", crs)`).

use std::path::Path;

use terrio::crs::{SpatialRef, Transformer};

const EUR11: &str = "+proj=ob_tran +o_proj=longlat +o_lon_p=0 +o_lat_p=39.25 +lon_0=18 +R=6371229 +no_defs";

const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]"#;

/// A WKT 2 rotated-pole CRS, as PROJ writes it: a base CRS and a deriving conversion.
fn wkt2(base: &str, method: &str, params: &[(&str, f64)]) -> String {
    let params: Vec<String> = params.iter().map(|(n, v)| format!(r#"PARAMETER["{n}",{v},{DEG}]"#)).collect();
    format!(
        r#"GEOGCRS["undefined",BASEGEOGCRS["undefined",{base},PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],DERIVINGCONVERSION["unknown",METHOD["{method}"],{}],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],{DEG}],AXIS["latitude",north,ORDER[2],{DEG}]]"#,
        params.join(",")
    )
}

const SPHERE: &str = r#"DATUM["undefined",ELLIPSOID["undefined",6371229,0,LENGTHUNIT["metre",1,ID["EPSG",9001]]]]"#;
const DATUM: &str =
    r#"DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ID["EPSG",6326]]"#;
const ENSEMBLE: &str = r#"ENSEMBLE["World Geodetic System 1984 ensemble",MEMBER["World Geodetic System 1984 (Transit)",ID["EPSG",1166]],MEMBER["World Geodetic System 1984 (G730)",ID["EPSG",1152]],MEMBER["World Geodetic System 1984 (G873)",ID["EPSG",1153]],MEMBER["World Geodetic System 1984 (G1150)",ID["EPSG",1154]],MEMBER["World Geodetic System 1984 (G1674)",ID["EPSG",1155]],MEMBER["World Geodetic System 1984 (G1762)",ID["EPSG",1156]],MEMBER["World Geodetic System 1984 (G2139)",ID["EPSG",1309]],MEMBER["World Geodetic System 1984 (G2296)",ID["EPSG",1383]],ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1],ID["EPSG",7030]],ENSEMBLEACCURACY[2.0],ID["EPSG",6326]]"#;

const CF: &str = "Pole rotation (netCDF CF convention)";

/// The CF form's parameters: the grid's north pole and the grid longitude of the geographic one.
fn cf_params(lat: f64, lon: f64, o_lon: f64) -> [(&'static str, f64); 3] {
    [
        ("Grid north pole latitude (netCDF CF convention)", lat),
        ("Grid north pole longitude (netCDF CF convention)", lon),
        ("North pole grid longitude (netCDF CF convention)", o_lon),
    ]
}

/// Points (lon, lat) and their EUR-11 grid coordinates, by PROJ.
const FWD: [Pair; 6] = [
    ((10.0, 45.0), (-5.6732984750900215, -5.443208334080359)),
    ((-5.0, 52.0), (-13.944869601997066, 3.4234705910559105)),
    ((30.0, 70.0), (4.329059340342213, 19.601633368966866)),
    ((-25.0, 35.0), (-34.16190012172183, -5.798251047828555)),
    ((179.0, -60.0), (170.47116611028576, -10.476811549245486)),
    ((-170.5, 10.25), (163.07456063053985, 60.0256806692558)),
];

/// EUR-11 grid coordinates and their longitude and latitude, by PROJ.
const INV: [Pair; 5] = [
    ((-5.67, -5.44), (10.004125482875015, 45.003544887124626)),
    ((-28.375, -23.375), (-10.063879662216037, 21.98782875683831)),
    ((20.0, 10.0), (54.751097928944496, 55.741263061720545)),
    ((170.0, -80.0), (20.638114054150687, -49.07100666800251)),
    ((-179.9, 60.0), (-161.94934126640257, 9.250034234418239)),
];

/// A point and its transformed coordinates.
type Pair = ((f64, f64), (f64, f64));

/// Checks the transformations from and to WGS 84 against PROJ's, within 1e-9°.
fn check(crs: &SpatialRef, fwd: &[Pair], inv: &[Pair], what: &str) {
    assert!(crs.is_rotated_pole() && !crs.is_geographic(), "{what}: not read as a rotated pole");
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let to = Transformer::new(&wgs84, crs).unwrap();
    let from = Transformer::new(crs, &wgs84).unwrap();
    for (pairs, t, dir) in [(fwd, &to, "forward"), (inv, &from, "inverse")] {
        for &((x, y), (ex, ey)) in pairs {
            let (gx, gy) = t.transform(x, y).unwrap();
            assert!((gx - ex).abs() < 1e-9 && (gy - ey).abs() < 1e-9, "{what}, {dir}: ({x}, {y}) -> ({gx}, {gy}), PROJ ({ex}, {ey})");
        }
    }
}

#[test]
fn proj_string() {
    let crs = SpatialRef::parse(EUR11).unwrap();
    check(&crs, &FWD, &INV, "PROJ string");
    // lon_0 is not wrapped by PROJ: 198 is the same meridian as -162.
    let same = SpatialRef::parse("+proj=ob_tran +o_proj=longlat +o_lat_p=39.25 +lon_0=378 +datum=WGS84").unwrap();
    check(&same, &FWD, &INV, "PROJ string on WGS 84, lon_0 beyond 180");
    // Its WKT, read back.
    let wkt = crs.to_wkt().unwrap();
    let back = SpatialRef::from_wkt(&wkt);
    assert_eq!(back.epsg, None, "{wkt}");
    check(&back, &FWD, &INV, "the PROJ string's WKT");
}

#[test]
fn wkt2_forms() {
    let ob_tran = |base| wkt2(base, "PROJ ob_tran o_proj=longlat", &[("o_lon_p", 0.0), ("o_lat_p", 39.25), ("lon_0", 18.0)]);
    let cases = [
        ("CF, sphere", wkt2(SPHERE, CF, &cf_params(39.25, -162.0, 0.0))),
        ("CF, WGS 84 ensemble", wkt2(ENSEMBLE, CF, &cf_params(39.25, -162.0, 0.0))),
        ("CF, WGS 84 datum", wkt2(DATUM, CF, &cf_params(39.25, -162.0, 0.0))),
        ("ob_tran, sphere", ob_tran(SPHERE)),
        ("ob_tran, WGS 84 datum", ob_tran(DATUM)),
        ("ob_tran, WGS 84 ensemble", ob_tran(ENSEMBLE)),
        // GRIB's form: the grid's south pole.
        (
            "GRIB, WGS 84 datum",
            wkt2(
                DATUM,
                "Pole rotation (GRIB convention)",
                &[
                    ("Latitude of the southern pole (GRIB convention)", -39.25),
                    ("Longitude of the southern pole (GRIB convention)", 18.0),
                    ("Axis rotation (GRIB convention)", 0.0),
                ],
            ),
        ),
    ];
    for (what, wkt) in cases {
        let crs = SpatialRef::parse(&wkt).unwrap();
        assert_eq!(crs.epsg, None, "{what}: identified as EPSG:{:?}", crs.epsg);
        check(&crs, &FWD, &INV, what);
    }
}

#[test]
fn southern_pole_and_grid_longitude() {
    // A pole south of the equator, with a grid longitude of the geographic pole other than 0.
    let crs = SpatialRef::parse(&wkt2(ENSEMBLE, CF, &cf_params(-20.0, 40.0, 12.5))).unwrap();
    let fwd = [
        ((10.0, 45.0), (34.52660659444505, 19.487250662351496)),
        ((-5.0, 52.0), (38.581006168159476, 8.022891577228455)),
        ((30.0, 70.0), (15.904907956945953, -0.27975887410903527)),
        ((-25.0, 35.0), (60.97552586087622, 7.419678910746542)),
        ((179.0, -60.0), (-148.31676404729592, -3.347942802204785)),
        ((-170.5, 10.25), (-91.31139596048061, -59.04874084794677)),
    ];
    let inv = [
        ((-5.67, -5.44), (92.9426159056213, 67.10783612059406)),
        ((-28.375, -23.375), (142.70515980460996, 51.991415045543654)),
        ((20.0, 10.0), (25.5021994865461, 59.10484593149857)),
        ((170.0, -80.0), (-136.1219319855464, 10.723475716765622)),
        ((-179.9, 60.0), (30.574613807958777, -49.03270997284169)),
    ];
    check(&crs, &fwd, &inv, "CF, southern pole");
}

#[test]
fn netcdf_cf_grid_mapping() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/rotated_pole/eur11.nc");
    let ds = terrio::open_raster(path.to_str().unwrap()).unwrap();
    let crs = ds.crs();
    check(&crs, &FWD, &INV, "CF grid mapping");
    // The grid is in rotated degrees, 0.11° apart.
    let gt = ds.geo_transform();
    let expected = [-28.375 - 0.055, 0.11, 0.0, -23.375 + 2.0 * 0.11 + 0.055, 0.0, -0.11];
    assert!(gt.iter().zip(expected).all(|(a, b)| (a - b).abs() < 1e-9), "{gt:?}");
}

#[test]
fn unsupported_forms_raise() {
    for s in [
        "+proj=ob_tran +o_proj=merc +o_lat_p=39.25 +lon_0=18 +R=6371229",
        "+proj=ob_tran +o_proj=longlat +o_lon_c=10 +o_lat_c=40 +o_alpha=5 +R=6371229",
        "+proj=ob_tran +o_proj=longlat +o_lon_1=10 +o_lat_1=40 +o_lon_2=20 +o_lat_2=50",
        "+proj=ob_tran +o_proj=longlat +lon_0=18",
    ] {
        assert!(SpatialRef::parse(s).is_err(), "{s}");
    }
    // A deriving conversion terrio does not apply, or with a parameter it does not use, is not
    // dropped.
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for wkt in [
        wkt2(DATUM, "Some other conversion", &[("o_lat_p", 39.25)]),
        wkt2(DATUM, "PROJ ob_tran o_proj=longlat", &[("o_lat_p", 39.25), ("o_alpha", 5.0)]),
        wkt2(DATUM, CF, &[("Grid north pole longitude (netCDF CF convention)", -162.0)]),
    ] {
        let crs = SpatialRef::parse(&wkt).unwrap();
        assert!(Transformer::new(&wgs84, &crs).is_err(), "{wkt}");
    }
}

#[test]
fn writes_and_reads_back() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/rotated_pole/eur11.nc");
    let ds = terrio::open_raster(path.to_str().unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for name in ["eur11.tif", "eur11.nc"] {
        let out = dir.path().join(name);
        terrio::write_raster_dataset(&ds, out.to_str().unwrap(), &Default::default()).unwrap();
        let back = terrio::open_raster(out.to_str().unwrap()).unwrap();
        check(&back.crs(), &FWD, &INV, name);
        assert_eq!(back.geo_transform(), ds.geo_transform(), "{name}");
    }
}
