//! `SpatialRef::to_projjson`: one CRS of each kind against what PROJ 9.9 writes for it, read back
//! by `SpatialRef::from_projjson` where its subset allows, and the refusals. The full comparison
//! with live PROJ and GDAL (52 CRSs by meaning and coordinates, and every IAU_2015 CRS) is the
//! `projjson-export` suite of `scripts/conformance.py`.

use serde_json::{Value, json};
use terrio_crs::{SpatialRef, Transformer};

fn export(definition: &str) -> Value {
    serde_json::from_str(&SpatialRef::parse(definition).unwrap().to_projjson().unwrap()).unwrap()
}

/// The directions of a coordinate system's axes.
fn directions(cs: &Value) -> Vec<&str> {
    cs["axis"].as_array().unwrap().iter().map(|a| a["direction"].as_str().unwrap()).collect()
}

/// A conversion's or transformation's method code and `(parameter code, value)`s, the values
/// rounded to 1e-9 (degrees go through radians).
fn method(c: &Value) -> (u64, Vec<(u64, f64)>) {
    let value = |p: &Value| (p["value"].as_f64().unwrap() * 1e9).round() / 1e9;
    let params = c["parameters"].as_array().unwrap().iter().map(|p| (p["id"]["code"].as_u64().unwrap(), value(p)));
    (c["method"]["id"]["code"].as_u64().unwrap(), params.collect())
}

#[test]
fn geographic_and_geocentric_crss() {
    let v = export("EPSG:4326");
    assert_eq!(v["$schema"], "https://proj.org/schemas/v0.7/projjson.schema.json");
    assert_eq!((&v["type"], &v["id"]), (&json!("GeographicCRS"), &json!({"authority": "EPSG", "code": 4326})));
    let e = &v["datum"]["ellipsoid"];
    assert_eq!((e["semi_major_axis"].as_f64(), e["inverse_flattening"].as_f64()), (Some(6378137.0), Some(298.257223563)));
    assert_eq!(directions(&v["coordinate_system"]), ["north", "east"]);
    // The ensembles are written as their datum, under its official name.
    assert_eq!(v["datum"]["name"], "World Geodetic System 1984");
    assert_eq!(export("EPSG:4258")["datum"]["name"], "European Terrestrial Reference System 1989");
    assert_eq!(directions(&export("EPSG:4979")["coordinate_system"]), ["north", "east", "up"]);
    let v = export("EPSG:4978");
    assert_eq!((&v["type"], &v["coordinate_system"]["subtype"]), (&json!("GeodeticCRS"), &json!("Cartesian")));
    assert_eq!(directions(&v["coordinate_system"]), ["geocentricX", "geocentricY", "geocentricZ"]);
}

#[test]
fn projected_crss() {
    let v = export("EPSG:32632");
    assert_eq!(v["conversion"]["name"], "UTM zone 32N");
    assert_eq!(method(&v["conversion"]), (9807, vec![(8801, 0.0), (8802, 9.0), (8805, 0.9996), (8806, 500000.0), (8807, 0.0)]));
    let v = export("EPSG:2154");
    let (code, mut params) = method(&v["conversion"]);
    params.sort_by_key(|p| p.0);
    assert_eq!((code, params), (9802, vec![(8821, 46.5), (8822, 3.0), (8823, 49.0), (8824, 44.0), (8826, 700000.0), (8827, 6600000.0)]));
    assert_eq!(directions(&v["coordinate_system"]), ["east", "north"]);
}

#[test]
fn vertical_compound_and_bound_crss() {
    let v = export("EPSG:5773");
    assert_eq!((&v["type"], &v["id"]["code"]), (&json!("VerticalCRS"), &json!(5773)));
    assert_eq!(directions(&v["coordinate_system"]), ["up"]);
    let v = export("EPSG:9707");
    let parts: Vec<&str> = v["components"].as_array().unwrap().iter().map(|c| c["type"].as_str().unwrap()).collect();
    assert_eq!((&v["type"], parts), (&json!("CompoundCRS"), vec!["GeographicCRS", "VerticalCRS"]));
    let v = export("+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7");
    assert_eq!((&v["type"], &v["target_crs"]["id"]["code"]), (&json!("BoundCRS"), &json!(4326)));
    let towgs84 = [(8605, 598.1), (8606, 73.7), (8607, 418.2), (8608, 0.202), (8609, 0.045), (8610, -2.455), (8611, 6.7)];
    assert_eq!(method(&v["transformation"]), (9606, towgs84.to_vec()));
}

#[test]
fn planetographic_and_planetocentric_crss() {
    // West-positive longitudes are a west axis; planetocentric latitudes a geodetic CRS with a
    // spherical coordinate system; IAU CRSs keep their versioned identifier.
    let v = export("IAU_2015:49901");
    assert_eq!((&v["type"], &v["id"]), (&json!("GeographicCRS"), &json!({"authority": "IAU", "code": 49901, "version": 2015})));
    assert_eq!(directions(&v["coordinate_system"]), ["north", "west"]);
    let v = export("IAU_2015:49902");
    assert_eq!((&v["type"], &v["coordinate_system"]["subtype"]), (&json!("GeodeticCRS"), &json!("spherical")));
    assert_eq!(v["coordinate_system"]["axis"][0]["name"], "Planetocentric latitude");
    assert_eq!(directions(&export("+proj=longlat +a=2440530 +b=2438260 +axis=nwu")["coordinate_system"]), ["north", "west"]);
    assert_eq!(export("+proj=longlat +a=2440530 +b=2438260 +geoc")["coordinate_system"]["subtype"], "spherical");
    // A semi-minor axis given as such is written as such: Eros's rounded inverse flattening moved
    // an orthographic point 3e-6° from GDAL's.
    let e = &export("IAU_2015:200043371")["base_crs"]["datum"]["ellipsoid"];
    assert_eq!((e["semi_major_axis"].as_f64(), e["semi_minor_axis"].as_f64()), (Some(17000.0), Some(5500.0)));
}

#[test]
fn methods_and_parameters_follow_proj() {
    // `$schema` first, as PROJ writes it.
    assert!(SpatialRef::from_epsg(4326).unwrap().to_projjson().unwrap().starts_with(r#"{"$schema":"#));
    // LCC as PROJ 9.9 reads the PROJ string: with lat_0 and no lat_2, 1SP (variant B away from
    // the origin); else 2SP, Michigan with a scale.
    let lcc = |d: &str| method(&export(&format!("+proj=lcc {d} +ellps=WGS84"))["conversion"]);
    assert_eq!(lcc("+lat_0=40 +lat_1=40").0, 9801);
    assert_eq!(lcc("+lat_1=45 +lat_0=40"), (1102, vec![(8801, 45.0), (8805, 1.0), (8821, 40.0), (8822, 0.0), (8826, 0.0), (8827, 0.0)]));
    assert_eq!(
        lcc("+lat_0=30 +lat_1=40 +lat_2=40"),
        (9802, vec![(8821, 30.0), (8822, 0.0), (8823, 40.0), (8824, 40.0), (8826, 0.0), (8827, 0.0)])
    );
    assert_eq!(lcc("+lat_1=33 +k_0=0.99").0, 1051);
    // eqc's latitude of origin (PROJ's 8801); Krovak's defaults and fixed angles, as terrio and
    // PROJ compute it.
    assert!(method(&export("+proj=eqc +lat_0=10 +lat_ts=30 +ellps=WGS84")["conversion"]).1.contains(&(8801, 10.0)));
    let krovak = vec![(8811, 49.5), (8833, 24.833333333), (1036, 30.288139753), (8818, 78.5), (8819, 0.9999), (8806, 0.0), (8807, 0.0)];
    assert_eq!(method(&export("+proj=krovak +ellps=bessel")["conversion"]), (1041, krovak));
    // A projected IAU CRS's identifier.
    assert_eq!(export("IAU_2015:49911")["id"], json!({"authority": "IAU", "code": 49911, "version": 2015}));
}

#[test]
fn datum_shifts_are_bound_as_proj_binds_them() {
    // An EPSG CRS is written as such; a WKT with TOWGS84 (and its code) is bound, the
    // transformation named after its geographic CRS.
    assert_eq!(export("EPSG:31467")["type"], "ProjectedCRS");
    let wkt = r#"PROJCS["DHDN / 3-degree Gauss-Kruger zone 3",GEOGCS["DHDN",DATUM["Deutsches_Hauptdreiecksnetz",SPHEROID["Bessel 1841",6377397.155,299.1528128],TOWGS84[598.1,73.7,418.2,0.202,0.045,-2.455,6.7]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",9],PARAMETER["scale_factor",1],PARAMETER["false_easting",3500000],PARAMETER["false_northing",0],UNIT["metre",1],AUTHORITY["EPSG","31467"]]"#;
    let v: Value = serde_json::from_str(&SpatialRef::from_wkt(wkt).to_projjson().unwrap()).unwrap();
    assert_eq!((&v["type"], &v["source_crs"]["id"]["code"]), (&json!("BoundCRS"), &json!(31467)));
    assert_eq!(v["transformation"]["name"], "Transformation from DHDN to WGS84");
    // EPSG CRSs whose GDAL WKT carries a TOWGS84 are written from their code, unbound.
    for code in ["EPSG:4277", "EPSG:27700", "EPSG:4230"] {
        assert_ne!(export(code)["type"], "BoundCRS", "{code}");
    }
    // As PROJ 9.9 binds a geocentric CRS (geocentric domain, to EPSG:4978) and a 3D one (to 4979).
    let v = export("+proj=geocent +ellps=bessel +towgs84=1,2,3");
    assert_eq!((v["transformation"]["method"]["id"]["code"].as_u64(), v["target_crs"]["id"]["code"].as_u64()), (Some(1031), Some(4978)));
    assert_eq!(v["target_crs"]["coordinate_system"]["subtype"], "Cartesian");
    let v = export("+proj=geocent +ellps=bessel +towgs84=1,2,3,4,5,6,7");
    assert_eq!(v["transformation"]["method"]["id"]["code"], 1033);
    let v = export("+proj=longlat +ellps=bessel +towgs84=1,2,3");
    assert_eq!((v["transformation"]["method"]["id"]["code"].as_u64(), v["target_crs"]["id"]["code"].as_u64()), (Some(9603), Some(4326)));
}

#[test]
fn what_would_be_written_wrongly_is_refused() {
    let refusal = |d: &str| SpatialRef::parse(d).unwrap().to_projjson().unwrap_err().to_string();
    assert!(SpatialRef::from_epsg(9000).unwrap().to_projjson().unwrap_err().to_string().contains("dynamic"));
    assert!(refusal("+proj=robin +datum=WGS84").contains("+proj=robin"));
    assert!(refusal("+proj=ob_tran +o_proj=moll +o_lat_p=45 +lon_0=147 +R=6371000").contains("ob_tran"));
    // UTM zones are 1 to 60 (terrio refuses others before writing, as PROJ does).
    assert!(SpatialRef::parse("+proj=utm +zone=61 +datum=WGS84").and_then(|s| s.to_projjson()).is_err());
    // A projection on the conformal or authalic sphere of an ellipsoid (IAU_2015:19991).
    assert!(refusal("+proj=merc +R_C +lat_0=0 +lon_0=0 +a=2440530 +b=2438260").contains("auxiliary sphere"));
    assert!(refusal("+proj=laea +R_A +lat_0=52 +lon_0=10 +ellps=GRS80").contains("auxiliary sphere"));
    // With the ellipsoid of a datum, or WGS 84 by default, too.
    for d in ["+proj=laea +R_A +lat_0=52 +lon_0=10 +datum=WGS84", "+proj=laea +R_A +lat_0=52 +lon_0=10"] {
        assert!(refusal(d).contains("auxiliary sphere"), "{d}");
    }
    // On a sphere, the auxiliary sphere is the sphere: written.
    assert!(SpatialRef::parse("+proj=merc +R_C +R=696000000").unwrap().to_projjson().is_ok());
    assert!(refusal("+proj=longlat +ellps=bessel +towgs84=1,2,3,4,5,6").contains("+towgs84 of 6 values"));
    // A prime meridian it cannot read is not written as Greenwich.
    assert!(refusal("+proj=longlat +datum=WGS84 +pm=foo").contains("+pm=foo"));
    // JSON has no NaN: a non-finite parameter is refused rather than written as null.
    assert!(refusal("+proj=tmerc +k_0=nan +ellps=WGS84").contains("non-finite"));
    // A WKT 2 dynamic CRS without code (its frame epoch would be lost).
    let wkt = r#"GEOGCRS["ITRF2014",DYNAMIC[FRAMEEPOCH[2010]],DATUM["International Terrestrial Reference Frame 2014",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["geodetic latitude (Lat)",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["geodetic longitude (Lon)",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]]]"#;
    assert!(SpatialRef::from_wkt(wkt).to_projjson().unwrap_err().to_string().contains("dynamic"));
    // Krovak's co-latitude of the cone axis is fixed: another +alpha (which PROJ reads) is refused;
    // its +lat_1 and +lat_ts, which PROJ ignores, are not.
    assert!(refusal("+proj=krovak +alpha=31 +ellps=bessel").contains("+alpha=31"));
    assert!(SpatialRef::parse("+proj=krovak +alpha=30.28813972222222 +lat_ts=70 +ellps=bessel").unwrap().to_projjson().is_ok());
}

#[test]
fn degrees_are_written_as_given() {
    // Through radians and back, as PROJ strings are read: the usual values stay exact.
    for (d, key, code) in [
        ("+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +datum=WGS84", "Latitude of standard parallel", 70.0),
        ("+proj=tmerc +lat_0=49.5 +lon_0=-2.1 +ellps=WGS84", "Latitude of natural origin", 49.5),
        ("+proj=tmerc +lat_0=49.5 +lon_0=-2.1 +ellps=WGS84", "Longitude of natural origin", -2.1),
        ("+proj=lcc +lat_0=46.5 +lat_1=49 +lat_2=44 +lon_0=3 +ellps=GRS80", "Latitude of 2nd standard parallel", 44.0),
    ] {
        let v = export(d);
        let p = v["conversion"]["parameters"].as_array().unwrap().iter().find(|p| p["name"] == key).unwrap();
        assert_eq!(p["value"].as_f64(), Some(code), "{d}");
    }
}

#[test]
fn parameters_it_would_not_write_are_refused() {
    // Any parameter of the PROJ definition the PROJJSON does not carry is named and refused,
    // unless it has the value used when it is absent.
    let refusal = |d: &str| SpatialRef::parse(d).unwrap().to_projjson().unwrap_err().to_string();
    assert!(refusal("+proj=tmerc +ellps=WGS84 +foo=3").contains("the parameter +foo=3"));
    assert!(refusal("+proj=longlat +lon_0=10 +datum=WGS84").contains("the parameter +lon_0=10"));
    assert!(refusal("+proj=cea +k_0=0.5 +ellps=WGS84").contains("the parameter +k_0=0.5"));
    assert!(refusal("+proj=krovak +czech +ellps=bessel").contains("the parameter +czech"));
    assert!(SpatialRef::parse("+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=0 +y_0=0 +ellps=WGS84").unwrap().to_projjson().is_ok());
}

#[test]
fn projjson_reads_back_as_the_same_crs() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (definition, (x, y)) in [
        ("EPSG:4326", (10.0, 50.0)),
        ("EPSG:32632", (500000.0, 5500000.0)),
        ("EPSG:2154", (650000.0, 6860000.0)),
        ("+proj=lcc +lat_0=30 +lat_1=40 +lat_2=40 +ellps=WGS84", (100000.0, 200000.0)),
        ("+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7", (8.0, 47.0)),
        // West-positive projected axes, as IAU_2015 planetographic projected CRSs have them.
        ("+proj=tmerc +lat_0=0 +lon_0=10 +k=1 +ellps=WGS84 +axis=wnu", (-100000.0, 5000000.0)),
    ] {
        let original = SpatialRef::parse(definition).unwrap();
        let back = SpatialRef::from_projjson(&original.to_projjson().unwrap()).unwrap();
        let a = Transformer::new(&original, &wgs84).unwrap().transform(x, y).unwrap();
        let b = Transformer::new(&back, &wgs84).unwrap().transform(x, y).unwrap();
        assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9, "{definition}: {a:?} {b:?}");
    }
}

/// For `scripts/conformance.py` (`projjson-export`): writes the PROJJSON of each definition of
/// `TERRIO_PROJJSON_EXPORT_CASES` (one per line) to `TERRIO_PROJJSON_EXPORT_REPORT`, as JSON
/// lines of `{definition, projjson}` or `{definition, error}`. Does nothing without them.
#[test]
fn export_report() {
    let (Ok(cases), Ok(report)) = (std::env::var("TERRIO_PROJJSON_EXPORT_CASES"), std::env::var("TERRIO_PROJJSON_EXPORT_REPORT")) else {
        return;
    };
    let mut out = String::new();
    for definition in std::fs::read_to_string(cases).unwrap().lines().filter(|l| !l.trim().is_empty()) {
        let line = match SpatialRef::parse(definition).and_then(|s| s.to_projjson()) {
            Ok(text) => json!({"definition": definition, "projjson": serde_json::from_str::<Value>(&text).unwrap()}),
            Err(e) => json!({"definition": definition, "error": e.to_string()}),
        };
        out.push_str(&line.to_string());
        out.push('\n');
    }
    std::fs::write(report, out).unwrap();
}
