//! Polar stereographic CRSs at their poles. In the formulas of IOGP's Guidance Note 7-2 (Polar
//! Stereographic, EPSG methods 9810, 9829 and 9830), a point lies at ρ = 2 a k₀ t / √((1 + e)^(1 +
//! e) (1 − e)^(1 − e)) from the pole the CRS is centred on, with t = tan(π/4 − φ/2) [(1 + e sin φ)
//! / (1 − e sin φ)]^(e/2) for the north pole case and φ's sign reversed for the south pole case: t
//! is 0 at that pole, which is the false origin, and grows without bound towards the other pole,
//! which no coordinates represent. A sphere is the case e = 0. Variant C (9830) is variant B with
//! its false origin on the standard parallel φF rather than at the pole, which lies at the northing
//! at the false origin NF minus ρF = a cos φF / (1 − e² sin² φF)^0.5 for the south pole case (plus
//! ρF for the north). Each CRS is checked from the longitude and latitude CRS on its own datum or
//! figure, so that no datum transformation is involved.

use terrio_crs::{SpatialRef, Transformer};

/// A polar stereographic CRS, the longitude and latitude CRS on the same datum, the latitude of
/// the pole it is centred on, its central meridian and the coordinates of that pole: its false
/// easting and northing, but for variant C.
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

/// A WKT 1 projected CRS on `Polar_Stereographic_(variant_C)`, with the EPSG names of its
/// parameters (latitude of standard parallel, longitude of origin, easting and northing at false
/// origin), on a datum of its own with the ellipsoid `spheroid` (`figure` in PROJ terms).
fn wkt1_variant_c(spheroid: &str, figure: &str, parameters: [f64; 4], pole: f64, origin: (f64, f64)) -> Case {
    let [lat_ts, lon_0, fe, fn_] = parameters;
    let wkt = format!(
        r#"PROJCS["unknown",GEOGCS["unknown",DATUM["unknown",{spheroid}],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic_(variant_C)"],PARAMETER["Latitude of standard parallel",{lat_ts}],PARAMETER["Longitude of origin",{lon_0}],PARAMETER["Easting at false origin",{fe}],PARAMETER["Northing at false origin",{fn_}],UNIT["metre",1]]"#
    );
    Case {
        name: format!("WKT 1 variant C at {lat_ts}° on {figure}"),
        crs: SpatialRef::from_wkt(&wkt),
        geographic: SpatialRef::from_proj4(&format!("+proj=longlat {figure}")).unwrap(),
        pole,
        lon_0,
        origin,
    }
}

/// Variant A (a scale at the pole), variant B (a standard parallel) and variant C (a false origin
/// on the standard parallel), north and south, on ellipsoids and spheres, from EPSG and ESRI
/// codes, WKT 1, WKT 2 and PROJ strings.
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
        // Variant C: Guidance Note 7-2's ρF is 2,499,363.488 m on Petrels 1972 (International
        // 1924) at 67° S.
        epsg(2985, -90.0, 140.0, (300_000.0, 200_000.0 - 2_499_363.487_830_594)),
        epsg(2986, -90.0, 140.0, (300_000.0, 200_000.0 - 2_499_363.487_830_594)),
        wkt2(
            "Polar Stereographic (variant C)",
            9830,
            &[
                ("Latitude of standard parallel", 8832, 70.0, "angle"),
                ("Longitude of origin", 8833, -45.0, "angle"),
                ("Easting at false origin", 8826, 1000.0, "length"),
                ("Northing at false origin", 8827, 2000.0, "length"),
            ],
            90.0,
            -45.0,
            (1000.0, 2000.0 + 2_187_927.649_279_021_7),
        ),
        wkt1_variant_c(
            r#"SPHEROID["International 1924",6378388,297]"#,
            "+ellps=intl",
            [75.0, 10.0, 0.0, -500.0],
            90.0,
            (0.0, -500.0 + 1_656_050.112_980_644_5),
        ),
        // On a sphere, ρF = R cos φF.
        wkt1_variant_c(
            r#"SPHEROID["unknown",6371000,0]"#,
            "+R=6371000",
            [-71.0, 30.0, 500.0, 700.0],
            -90.0,
            (500.0, 700.0 - 2_074_194.712_046_545_7),
        ),
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
/// longitude, rather than giving coordinates. So does a point 1e-7° short of it, which lies within
/// the same tolerance (1e-8 rad) as the pole itself.
#[test]
fn polar_stereographic_refuses_the_pole_opposite_its_centre() {
    for c in cases() {
        let fwd = Transformer::new(&c.geographic, &c.crs).unwrap();
        let near = -c.pole + c.pole.signum() * 1e-7;
        for lon in LONGITUDES {
            for lat in [-c.pole, near] {
                let r = fwd.transform(lon, lat);
                assert!(r.is_err(), "{}: the opposite pole ({lon}, {lat}) went to {r:?}", c.name);
            }
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

/// Petrels 1972 / Terre Adelie Polar Stereographic as a WKT 1 that names the method
/// `Polar_Stereographic_(variant_C)` and its parameters by their EPSG names, and as a WKT 2 and a
/// PROJJSON that give the method and its parameters by their EPSG names and codes.
const TERRE_ADELIE_WKT1: &str = r#"PROJCS["Terre Adelie",GEOGCS["Petrels 1972",DATUM["Petrels_1972",SPHEROID["International 1924",6378388,297]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic_(variant_C)"],PARAMETER["Latitude of standard parallel",-67],PARAMETER["Longitude of origin",140],PARAMETER["Easting at false origin",300000],PARAMETER["Northing at false origin",200000],UNIT["metre",1]]"#;
const TERRE_ADELIE_WKT2: &str = r#"PROJCRS["Terre Adelie",BASEGEOGCRS["Petrels 1972",DATUM["Petrels 1972",ELLIPSOID["International 1924",6378388,297,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Terre Adelie",METHOD["Polar Stereographic (variant C)",ID["EPSG",9830]],PARAMETER["Latitude of standard parallel",-67,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8832]],PARAMETER["Longitude of origin",140,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8833]],PARAMETER["Easting at false origin",300000,LENGTHUNIT["metre",1],ID["EPSG",8826]],PARAMETER["Northing at false origin",200000,LENGTHUNIT["metre",1],ID["EPSG",8827]]],CS[Cartesian,2],AXIS["easting (X)",north,ORDER[1],LENGTHUNIT["metre",1]],AXIS["northing (Y)",north,ORDER[2],LENGTHUNIT["metre",1]]]"#;
const TERRE_ADELIE_PROJJSON: &str = r#"{"$schema":"https://proj.org/schemas/v0.7/projjson.schema.json","type":"ProjectedCRS","name":"Terre Adelie","base_crs":{"type":"GeographicCRS","name":"Petrels 1972","datum":{"type":"GeodeticReferenceFrame","name":"Petrels 1972","ellipsoid":{"name":"International 1924","semi_major_axis":6378388,"inverse_flattening":297}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"},{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"}]}},"conversion":{"name":"Terre Adelie","method":{"name":"Polar Stereographic (variant C)","id":{"authority":"EPSG","code":9830}},"parameters":[{"name":"Latitude of standard parallel","value":-67,"unit":"degree","id":{"authority":"EPSG","code":8832}},{"name":"Longitude of origin","value":140,"unit":"degree","id":{"authority":"EPSG","code":8833}},{"name":"Easting at false origin","value":300000,"unit":"metre","id":{"authority":"EPSG","code":8826}},{"name":"Northing at false origin","value":200000,"unit":"metre","id":{"authority":"EPSG","code":8827}}]},"coordinate_system":{"subtype":"Cartesian","axis":[{"name":"Easting","abbreviation":"E","direction":"east","unit":"metre"},{"name":"Northing","abbreviation":"N","direction":"north","unit":"metre"}]}}"#;

/// Guidance Note 7-2's worked example point, 66°36'18.820"S, 140°04'17.040"E, as longitude and
/// latitude in degrees.
fn worked_example_point() -> (f64, f64) {
    (140.0 + 4.0 / 60.0 + 17.04 / 3600.0, -(66.0 + 36.0 / 60.0 + 18.82 / 3600.0))
}

/// Guidance Note 7-2's worked example of variant C, on Petrels 1972 / Terre Adelie Polar
/// Stereographic: the point is 303169.52 E, 244055.72 N, to the centimetre the note gives, and
/// (303169.522, 244055.721) is the point, to the thousandth of an arc second it gives. From the
/// CRS's EPSG code, from Perroud 1950 / Terre Adelie Polar Stereographic (EPSG:2986, the same
/// projection on another datum), from a WKT 1, a WKT 2 and a PROJJSON that name the method, and
/// from the PROJJSON terrio writes for EPSG:2985 (variant B, with the false northing of the pole).
#[test]
fn polar_stereographic_variant_c_gives_the_guidance_notes_worked_example() {
    let (lon, lat) = worked_example_point();
    let petrels = SpatialRef::from_epsg(4636).unwrap();
    for (name, crs, geographic) in [
        ("EPSG:2985", SpatialRef::from_epsg(2985).unwrap(), petrels.clone()),
        ("EPSG:2986", SpatialRef::from_epsg(2986).unwrap(), SpatialRef::from_epsg(4637).unwrap()),
        ("WKT 1", SpatialRef::from_wkt(TERRE_ADELIE_WKT1), petrels.clone()),
        ("WKT 2", SpatialRef::from_wkt(TERRE_ADELIE_WKT2), petrels.clone()),
        ("PROJJSON", SpatialRef::from_projjson(TERRE_ADELIE_PROJJSON).unwrap(), petrels.clone()),
        // The PROJJSON terrio writes for it, read back.
        (
            "EPSG:2985's PROJJSON",
            SpatialRef::from_projjson(&SpatialRef::from_epsg(2985).unwrap().to_projjson().unwrap()).unwrap(),
            petrels.clone(),
        ),
    ] {
        let (x, y) = Transformer::new(&geographic, &crs).unwrap().transform(lon, lat).unwrap();
        assert!((x - 303_169.52).abs() < 0.005 && (y - 244_055.72).abs() < 0.005, "{name}: the example point went to ({x}, {y})");
        let (back_lon, back_lat) = Transformer::new(&crs, &geographic).unwrap().transform(303_169.522, 244_055.721).unwrap();
        assert!(
            ((back_lon - lon) * 3600.0).abs() < 0.0005 && ((back_lat - lat) * 3600.0).abs() < 0.0005,
            "{name}: (303169.522, 244055.721) went to ({back_lon}, {back_lat}), not ({lon}, {lat})"
        );
    }
}

/// EPSG:2985 and 2986 are CRSs of terrio's own, in either configuration of the crate: with their
/// EPSG names, on their datums, Petrels 1972 and Perroud 1950, with the projection's parameters
/// as EPSG gives them. From WGS 84, the note's example point goes to the same coordinates directly
/// as through the geographic CRS: the CRS's datum transformation is its geographic CRS's.
/// `to_gdal_projjson`, which has no form for the method, refuses it rather than writing another.
#[test]
fn polar_stereographic_variant_c_codes_keep_their_datum_and_parameters() {
    let (lon, lat) = worked_example_point();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (code, geographic, datum, name) in [
        (2985, 4636, "Petrels_1972", "Petrels 1972 / Terre Adelie Polar Stereographic"),
        (2986, 4637, "Pointe_Geologie_Perroud_1950", "Perroud 1950 / Terre Adelie Polar Stereographic"),
    ] {
        let crs = SpatialRef::from_epsg(code).unwrap();
        assert_eq!(crs.name().as_deref(), Some(name), "EPSG:{code}");
        assert_eq!(crs.geographic_code(), Some(geographic), "EPSG:{code}");
        let error = crs.to_gdal_projjson(true).err().map(|e| e.to_string());
        assert!(error.as_deref().is_some_and(|e| e.contains("Polar_Stereographic_(variant_C)")), "EPSG:{code}: {error:?}");
        let geographic = SpatialRef::from_epsg(geographic).unwrap();
        let (x, y) = Transformer::new(&wgs84, &crs).unwrap().transform(lon, lat).unwrap();
        let (glon, glat) = Transformer::new(&wgs84, &geographic).unwrap().transform(lon, lat).unwrap();
        let (gx, gy) = Transformer::new(&geographic, &crs).unwrap().transform(glon, glat).unwrap();
        assert!(
            ((x - gx).powi(2) + (y - gy).powi(2)).sqrt() < 0.001,
            "EPSG:{code}: ({x}, {y}) directly, ({gx}, {gy}) through its geographic CRS"
        );
        let wkt = crs.to_wkt().unwrap();
        for part in [
            format!(r#"DATUM["{datum}""#),
            r#"PROJECTION["Polar_Stereographic_(variant_C)"]"#.to_string(),
            r#"PARAMETER["Latitude of standard parallel",-67]"#.to_string(),
            r#"PARAMETER["Longitude of origin",140]"#.to_string(),
            r#"PARAMETER["Easting at false origin",300000]"#.to_string(),
            r#"PARAMETER["Northing at false origin",200000]"#.to_string(),
            format!(r#"AUTHORITY["EPSG","{code}"]]"#),
        ] {
            assert!(wkt.contains(&part), "EPSG:{code}: {part} not in {wkt}");
        }
    }
}

/// A WKT 1 or WKT 2 of variant C without its standard parallel is refused, in either configuration
/// of the crate, rather than read as another variant.
#[test]
fn polar_stereographic_variant_c_without_its_standard_parallel_is_refused() {
    let petrels = SpatialRef::from_epsg(4636).unwrap();
    for (name, wkt, parameter) in [
        ("WKT 1", TERRE_ADELIE_WKT1, r#"PARAMETER["Latitude of standard parallel",-67],"#),
        (
            "WKT 2",
            TERRE_ADELIE_WKT2,
            r#"PARAMETER["Latitude of standard parallel",-67,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8832]],"#,
        ),
    ] {
        assert!(wkt.contains(parameter), "{name}");
        let error = Transformer::new(&petrels, &SpatialRef::from_wkt(&wkt.replace(parameter, ""))).err().map(|e| e.to_string());
        assert!(error.as_deref().is_some_and(|e| e.contains("Polar Stereographic (variant C)")), "{name}: {error:?}");
    }
}

/// The wbprojection CRS a variant C CRS converts to, from which terrio describes CRSs in formats
/// such as NetCDF's grid mappings, projects the note's example point as terrio does, also when the
/// CRS is the source of a WKT 2 bound CRS.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn polar_stereographic_variant_c_converts_to_wbprojection_as_terrio_reads_it() {
    let (lon, lat) = worked_example_point();
    let bound = format!(
        r#"BOUNDCRS[SOURCECRS[{TERRE_ADELIE_WKT2}],TARGETCRS[GEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["latitude",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["longitude",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]]]],ABRIDGEDTRANSFORMATION["Petrels 1972 to WGS 84",METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],PARAMETER["X-axis translation",365,ID["EPSG",8605]],PARAMETER["Y-axis translation",194,ID["EPSG",8606]],PARAMETER["Z-axis translation",166,ID["EPSG",8607]]]]"#
    );
    for (name, crs) in [
        ("EPSG:2985", SpatialRef::from_epsg(2985).unwrap()),
        ("WKT 1", SpatialRef::from_wkt(TERRE_ADELIE_WKT1)),
        ("WKT 2", SpatialRef::from_wkt(TERRE_ADELIE_WKT2)),
        ("a WKT 2 bound CRS", SpatialRef::from_wkt(&bound)),
    ] {
        let (x, y) = crs.to_wbp().unwrap().forward(lon, lat).unwrap();
        assert!((x - 303_169.52).abs() < 0.005 && (y - 244_055.72).abs() < 0.005, "{name}: the example point went to ({x}, {y})");
    }
}
