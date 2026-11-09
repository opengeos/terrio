//! Polar stereographic CRSs at their poles. In the formulas of IOGP's Guidance Note 7-2 (Polar
//! Stereographic, EPSG methods 9810 and 9829), a point lies at ρ = 2 a k₀ t / √((1 + e)^(1 + e)
//! (1 − e)^(1 − e)) from the pole the CRS is centred on, with t = tan(π/4 − φ/2) [(1 + e sin φ) /
//! (1 − e sin φ)]^(e/2) for the north pole case and φ's sign reversed for the south pole case: t is
//! 0 at that pole, which is the false origin, and grows without bound towards the other pole,
//! which no coordinates represent. A sphere is the case e = 0. Each CRS is checked from the
//! longitude and latitude CRS on its own datum or figure, so that no datum transformation is
//! involved.

use terrio_crs::{SpatialRef, Transformer};

/// A polar stereographic CRS, the longitude and latitude CRS on the same datum, the latitude of
/// the pole it is centred on, its central meridian and its false easting and northing.
struct Case {
    name: String,
    crs: SpatialRef,
    geographic: SpatialRef,
    pole: f64,
    lon_0: f64,
    origin: (f64, f64),
}

fn epsg(code: u32, pole: f64, lon_0: f64, origin: (f64, f64)) -> Case {
    let crs = SpatialRef::from_epsg(code).unwrap();
    let geographic = SpatialRef::from_epsg(crs.geographic_code().unwrap()).unwrap();
    Case { name: format!("EPSG:{code}"), crs, geographic, pole, lon_0, origin }
}

fn esri(code: u32, pole: f64) -> Case {
    let crs = SpatialRef::from_esri(code).unwrap();
    Case { name: format!("ESRI:{code}"), crs, geographic: SpatialRef::from_epsg(4326).unwrap(), pole, lon_0: 0.0, origin: (0.0, 0.0) }
}

fn proj(definition: &str, figure: &str, pole: f64, lon_0: f64, origin: (f64, f64)) -> Case {
    Case {
        name: definition.to_string(),
        crs: SpatialRef::from_proj4(&format!("{definition} {figure} +units=m")).unwrap(),
        geographic: SpatialRef::from_proj4(&format!("+proj=longlat {figure}")).unwrap(),
        pole,
        lon_0,
        origin,
    }
}

/// A WKT 2 projected CRS on WGS 84 with the method `method` (EPSG code `code`) and `parameters`.
fn wkt2(method: &str, code: u32, parameters: &[(&str, u32, f64, &str)], pole: f64, lon_0: f64, origin: (f64, f64)) -> Case {
    let unit = |kind: &str| match kind {
        "angle" => r#"ANGLEUNIT["degree",0.0174532925199433]"#,
        "scale" => r#"SCALEUNIT["unity",1]"#,
        _ => r#"LENGTHUNIT["metre",1]"#,
    };
    let parameters: Vec<String> =
        parameters.iter().map(|(name, id, value, kind)| format!(r#"PARAMETER["{name}",{value},{},ID["EPSG",{id}]]"#, unit(kind))).collect();
    let wkt = format!(
        r#"PROJCRS["{method}",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["{method}",METHOD["{method}",ID["EPSG",{code}]],{}],CS[Cartesian,2],AXIS["easting (E)",east,ORDER[1],LENGTHUNIT["metre",1]],AXIS["northing (N)",north,ORDER[2],LENGTHUNIT["metre",1]]]"#,
        parameters.join(",")
    );
    let crs = SpatialRef::from_wkt(&wkt);
    Case {
        name: format!("WKT 2 {method}"),
        crs,
        geographic: SpatialRef::from_proj4("+proj=longlat +datum=WGS84").unwrap(),
        pole,
        lon_0,
        origin,
    }
}

/// Variant A (a scale at the pole) and variant B (a standard parallel), north and south, on
/// ellipsoids and spheres, from EPSG and ESRI codes, WKT 2 and PROJ strings.
fn cases() -> Vec<Case> {
    let ups = (2_000_000.0, 2_000_000.0);
    vec![
        // Variant A.
        epsg(32661, 90.0, 0.0, ups),
        epsg(32761, -90.0, 0.0, ups),
        epsg(5041, 90.0, 0.0, ups),
        epsg(5042, -90.0, 0.0, ups),
        epsg(5482, -90.0, 180.0, (5_000_000.0, 1_000_000.0)),
        epsg(5936, 90.0, -150.0, ups),
        esri(102018, 90.0),
        esri(102021, -90.0),
        wkt2(
            "Polar Stereographic (variant A)",
            9810,
            &[
                ("Latitude of natural origin", 8801, -90.0, "angle"),
                ("Longitude of natural origin", 8802, 30.0, "angle"),
                ("Scale factor at natural origin", 8805, 0.994, "scale"),
                ("False easting", 8806, 1000.0, "length"),
                ("False northing", 8807, -2000.0, "length"),
            ],
            -90.0,
            30.0,
            (1000.0, -2000.0),
        ),
        proj("+proj=ups", "+datum=WGS84", 90.0, 0.0, ups),
        proj("+proj=ups +south", "+datum=WGS84", -90.0, 0.0, ups),
        // Variant B.
        epsg(3413, 90.0, -45.0, (0.0, 0.0)),
        epsg(3995, 90.0, 0.0, (0.0, 0.0)),
        epsg(3411, 90.0, -45.0, (0.0, 0.0)),
        epsg(3031, -90.0, 0.0, (0.0, 0.0)),
        epsg(3976, -90.0, 0.0, (0.0, 0.0)),
        epsg(3412, -90.0, 0.0, (0.0, 0.0)),
        epsg(3032, -90.0, 70.0, (6_000_000.0, 6_000_000.0)),
        wkt2(
            "Polar Stereographic (variant B)",
            9829,
            &[
                ("Latitude of standard parallel", 8832, 71.0, "angle"),
                ("Longitude of origin", 8833, -100.0, "angle"),
                ("False easting", 8806, 3000.0, "length"),
                ("False northing", 8807, 4000.0, "length"),
            ],
            90.0,
            -100.0,
            (3000.0, 4000.0),
        ),
        proj("+proj=stere +lat_0=-90 +lat_ts=-60 +lon_0=120 +x_0=500 +y_0=700", "+ellps=GRS80", -90.0, 120.0, (500.0, 700.0)),
        // Spheres.
        proj("+proj=stere +lat_0=90 +lat_ts=60 +lon_0=-32 +x_0=400000 +y_0=5500000", "+R=6370000", 90.0, -32.0, (400_000.0, 5_500_000.0)),
        proj("+proj=stere +lat_0=-90 +lon_0=40 +k=0.994 +x_0=2000000 +y_0=2000000", "+R=6371000", -90.0, 40.0, ups),
        proj("+proj=ups +south", "+R=6371000", -90.0, 0.0, ups),
    ]
}

const LONGITUDES: [f64; 7] = [-180.0, -135.0, -45.0, 0.0, 30.0, 90.0, 180.0];

/// The pole a CRS is centred on projects to its false origin, from any longitude, and the false
/// origin is that pole, on the CRS's central meridian. In binary floating point, 90° is 6.1e-17
/// radians short of the pole (under a nanometre on the ground), so the pole lands within 1e-6 m.
#[test]
fn polar_stereographic_projects_its_pole_to_the_false_origin() {
    for c in cases() {
        let fwd = Transformer::new(&c.geographic, &c.crs).unwrap();
        let inv = Transformer::new(&c.crs, &c.geographic).unwrap();
        for lon in LONGITUDES {
            let (x, y) = fwd.transform(lon, c.pole).unwrap_or_else(|e| panic!("{}: ({lon}, {}) failed: {e}", c.name, c.pole));
            assert!(
                (x - c.origin.0).abs() < 1e-6 && (y - c.origin.1).abs() < 1e-6,
                "{}: ({lon}, {}) went to ({x}, {y}), not the false origin {:?}",
                c.name,
                c.pole,
                c.origin
            );
            let (back_lon, back_lat) = inv.transform(x, y).unwrap();
            assert!((back_lat - c.pole).abs() < 1e-12, "{}: ({x}, {y}) went back to ({back_lon}, {back_lat})", c.name);
        }
        let (lon, lat) = inv.transform(c.origin.0, c.origin.1).unwrap();
        let dlon = (lon - c.lon_0 + 540.0).rem_euclid(360.0) - 180.0;
        assert!(lat == c.pole && dlon.abs() < 1e-12, "{}: the false origin went to ({lon}, {lat})", c.name);
    }
}

/// The pole opposite the one a CRS is centred on lies at infinity: it fails to project, from any
/// longitude, rather than giving coordinates.
#[test]
fn polar_stereographic_refuses_the_pole_opposite_its_centre() {
    for c in cases() {
        let fwd = Transformer::new(&c.geographic, &c.crs).unwrap();
        for lon in LONGITUDES {
            let r = fwd.transform(lon, -c.pole);
            assert!(r.is_err(), "{}: the opposite pole ({lon}, {}) went to {r:?}", c.name, -c.pole);
        }
    }
}

/// The worked examples of Guidance Note 7-2: variant A on WGS 84 / UPS North, and variant B on
/// WGS 84 / Australian Antarctic Polar Stereographic, to the centimetre the note gives, and back.
#[test]
fn polar_stereographic_gives_the_guidance_notes_worked_examples() {
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (code, (lon, lat), (e, n)) in
        [(5041, (44.0, 73.0), (3_320_416.75, 632_668.43)), (3032, (120.0, -75.0), (7_255_380.79, 7_053_389.56))]
    {
        let crs = SpatialRef::from_epsg(code).unwrap();
        let (x, y) = Transformer::new(&wgs84, &crs).unwrap().transform(lon, lat).unwrap();
        assert!((x - e).abs() < 0.005 && (y - n).abs() < 0.005, "EPSG:{code}: ({lon}, {lat}) went to ({x}, {y}), not ({e}, {n})");
        let (back_lon, back_lat) = Transformer::new(&crs, &wgs84).unwrap().transform(e, n).unwrap();
        assert!((back_lon - lon).abs() < 1e-7 && (back_lat - lat).abs() < 1e-7, "EPSG:{code}: ({e}, {n}) went to ({back_lon}, {back_lat})");
    }
}
