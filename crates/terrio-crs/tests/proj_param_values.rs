//! A PROJ string with a parameter value PROJ cannot read is refused as PROJ 9.8.1 refuses it,
//! rather than read with the parameter's default: `+lat_ts=abc` for `eqc`, an unreadable
//! `+towgs84` or `+pm`, a scale factor or `+to_meter` that is not positive. `0.5r` is in radians.
//!
//! `data/proj_param_values.tsv` (`scripts/gen_proj_param_values.py`) holds whether PROJ 9.8.1
//! (pyproj 3.8.0, `PROJ_NETWORK=OFF`) accepts each string, with `+type=crs`, on WGS 84 unless
//! the string gives its own ellipsoid. Two kinds of string still differ, listed in [`known`].

use terrio_crs::SpatialRef;

/// Where terrio and PROJ still differ:
/// - the parameters every method reads (`lat_0`, `lon_0`, `x_0`, `y_0`, and the scale factor's
///   number): PROJ checks them only for the methods its CRS parser maps them for, and terrio reads
///   an unreadable one as absent, as before, so it accepts some strings PROJ refuses;
/// - checks of terrio's own for methods PROJ keeps as PROJ strings (`isea`, `murd2`, `tissot`,
///   `tpers`), which refuse an unreadable value PROJ ignores, and Krovak's azimuth, which terrio
///   does not read.
fn known(definition: &str, proj_accepts: bool) -> bool {
    let key = definition.rsplit(" +").next().and_then(|p| p.split_once('=')).map_or("", |(k, _)| k);
    let method = definition.split_whitespace().next().unwrap_or("").trim_start_matches("+proj=");
    let lenient = matches!(key, "lat_0" | "lon_0" | "x_0" | "y_0" | "k_0" | "k");
    let own = match method {
        "isea" | "tpers" => matches!(key, "azi" | "lat_0" | "lon_0"),
        "murd2" | "tissot" => matches!(key, "lat_1" | "lat_2"),
        _ => false,
    };
    let krovak_azimuth = matches!(method, "krovak" | "mod_krovak") && key == "alpha";
    (lenient && !proj_accepts) || (own && proj_accepts) || (krovak_azimuth && !proj_accepts)
}

#[test]
fn unreadable_parameter_values_are_refused_as_proj_refuses_them() {
    let mut differing = 0;
    for line in include_str!("data/proj_param_values.tsv").lines() {
        let (definition, proj) = line.split_once('\t').unwrap();
        let own_ellipsoid = [" +a=", " +rf=", " +R="].iter().any(|k| definition.contains(k));
        let full = if own_ellipsoid { definition.to_string() } else { format!("{definition} +ellps=WGS84") };
        let accepted = SpatialRef::parse(&full);
        let proj_accepts = proj == "OK";
        if accepted.is_ok() != proj_accepts {
            assert!(known(definition, proj_accepts), "{full}: PROJ {proj}, terrio {:?}", accepted.err());
            differing += 1;
        }
    }
    // A change in the strings that still differ shows here.
    assert_eq!(differing, 301, "strings that still differ from PROJ");
}

#[test]
fn radians_and_units_are_read_as_proj_reads_them() {
    let params = |d: &str| SpatialRef::parse(d).unwrap().projection_params().unwrap();
    // `0.5r` is 0.5 radians (PROJ: `+lat_ts=28.6478897565412`); `30d30r` is refused.
    let lat_ts = SpatialRef::parse("+proj=eqc +lat_ts=0.5r +ellps=WGS84").unwrap();
    let proj = lat_ts.to_proj4().unwrap();
    assert!(proj.contains("+lat_ts=28.6478897565412"), "{proj}");
    assert!(SpatialRef::parse("+proj=eqc +lat_ts=30d30r +ellps=WGS84").is_err());
    assert!((params("+proj=tmerc +lon_0=0.2r +ellps=WGS84").lon_0 - 0.2f64.to_degrees()).abs() < 1e-12);
    // `+units` before `+to_meter`, as PROJ reads them: feet.
    assert!((params("+proj=eqc +units=ft +to_meter=0.5 +ellps=WGS84").to_meter - 0.3048).abs() < 1e-15);
    assert!((params("+proj=eqc +to_meter=0.5 +ellps=WGS84").to_meter - 0.5).abs() < 1e-15);
}
