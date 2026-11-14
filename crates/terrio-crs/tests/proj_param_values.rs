//! A PROJ string with a parameter value PROJ cannot read is refused as PROJ 9.8.1 refuses it,
//! rather than read with the parameter's default: `+lat_ts=abc` for `eqc`, an unreadable
//! `+towgs84` or `+pm`, a scale factor or `+to_meter` that is not positive. `0.5r` is in radians.
//!
//! `data/proj_param_values.tsv` (`scripts/gen_proj_param_values.py`) holds whether PROJ 9.8.1
//! (pyproj 3.8.0, `PROJ_NETWORK=OFF`) accepts each string, with `+type=crs`, on WGS 84 unless
//! the string gives its own ellipsoid. Remaining differences are listed in [`known`].

use terrio_crs::SpatialRef;

/// Whether the fixture supplies its own ellipsoid rather than the default WGS 84.
fn own_ellipsoid(definition: &str) -> bool {
    [" +a=", " +rf=", " +R="].iter().any(|k| definition.contains(k))
}

#[test]
fn unchecked_parameters_have_proj_accepted_fixture_rows() {
    // PROJ 9.8.1 accepts each exemption in Def's unchecked method rules.
    for definition in [
        "+proj=murd2 +lat_2=50 +lat_1=abc",
        "+proj=murd2 +lat_1=30 +lat_2=abc",
        "+proj=tissot +lat_2=50 +lat_1=abc",
        "+proj=tissot +lat_1=30 +lat_2=abc",
        "+proj=isea +azi=abc",
        "+proj=ortho +alpha=abc",
        "+proj=ups +lat_ts=abc",
        "+proj=stere +lat_ts=abc",
    ] {
        assert!(include_str!("data/proj_param_values.tsv").lines().any(|line| line == format!("{definition}\tOK")), "{definition}");
    }
}

/// Where terrio and PROJ still differ:
/// - the parameters every method reads (`lat_0`, `lon_0`, `x_0`, `y_0`, and a scale factor that
///   starts with a positive number, such as `0.5r`): PROJ checks them only for the methods its CRS
///   parser maps them for, and terrio reads an unreadable one as absent (a scale factor of 1), as
///   before, so it accepts some strings PROJ refuses; likewise `+f`, `+es` and `+e` without `+a`,
///   which PROJ ignores when unreadable but for `etmerc`, `utm` and `ups`;
/// - checks of terrio's own for methods PROJ keeps as PROJ strings (`isea`, `murd2`, `tissot`,
///   `tpers`), and of a geocentric CRS's ellipsoid, which refuse an unreadable value PROJ ignores;
/// - Krovak's azimuth, which terrio does not read;
/// - rotated-pole definitions keep terrio's strict check of `o_lat_p`, which PROJ's CRS parser
///   leaves unchecked.
fn known(definition: &str, proj_accepts: bool) -> bool {
    let key = definition.rsplit(" +").next().and_then(|p| p.split_once('=')).map_or("", |(k, _)| k);
    let method = definition.split_whitespace().next().unwrap_or("").trim_start_matches("+proj=");
    let lenient =
        matches!(key, "lat_0" | "lon_0" | "x_0" | "y_0" | "k_0" | "k") || (matches!(key, "f" | "es" | "e") && !own_ellipsoid(definition));
    let own = match method {
        "isea" | "tpers" => matches!(key, "azi" | "lat_0" | "lon_0"),
        "murd2" | "tissot" => matches!(key, "lat_1" | "lat_2"),
        "geocent" => matches!(key, "f" | "es" | "e"),
        "ob_tran" => definition.contains("+o_lat_p=abc"),
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
        let full = if own_ellipsoid(definition) { definition.to_string() } else { format!("{definition} +ellps=WGS84") };
        let accepted = SpatialRef::parse(&full);
        let proj_accepts = proj == "OK";
        if accepted.is_ok() != proj_accepts {
            assert!(known(definition, proj_accepts), "{full}: PROJ {proj}, terrio {:?}", accepted.err());
            differing += 1;
        }
    }
    // A change in the strings that still differ shows here.
    assert_eq!(differing, 320, "strings that still differ from PROJ");
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
    // A scale factor `+k_0` that is not a number gives way to `+k`, in the projection as in the
    // parameters written for it.
    assert_eq!(params("+proj=tmerc +k_0=0.5r +k=0.9996 +ellps=WGS84").k_0, 0.9996);
    // PROJ 9.8.1 refuses a nonpositive fallback too, while NaN still passes.
    for k in ["0", "-1"] {
        assert!(SpatialRef::parse(&format!("+proj=tmerc +k_0=0.5r +k={k} +ellps=WGS84")).is_err());
    }
}

/// A WKT's `PROJ4` extension is taken as PROJ takes it, its unreadable values as their defaults:
/// PROJ 9.8.1 (pyproj 3.8.0) transforms (10°E, 10°N) to (1113194.9079327357, 1105854.833234372)
/// with `+lat_ts=abc` read as 0, not the WKT's standard parallel of 10°.
#[test]
fn a_wkt_s_proj4_extension_is_read_as_proj_reads_it() {
    let wkt = r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Equirectangular"],PARAMETER["standard_parallel_1",10],PARAMETER["central_meridian",0],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=eqc +lat_ts=abc +ellps=WGS84 +units=m"]]"#;
    let t = terrio_crs::Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::parse(wkt).unwrap()).unwrap();
    let (x, y) = t.transform(10.0, 10.0).unwrap();
    assert!((x - 1113194.9079327357).abs() < 1e-6 && (y - 1105854.833234372).abs() < 1e-6, "{x} {y}");
}
