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
    let mut differences = Vec::new();
    let mut definitions = std::collections::HashSet::new();
    for line in include_str!("data/proj_param_values.tsv").lines() {
        let (definition, proj) = line.split_once('\t').unwrap();
        assert!(definitions.insert(definition), "duplicate definition: {definition}");
        let full = if own_ellipsoid(definition) { definition.to_string() } else { format!("{definition} +ellps=WGS84") };
        let accepted = SpatialRef::parse(&full);
        let proj_accepts = proj == "OK";
        if accepted.is_ok() != proj_accepts {
            assert!(known(definition, proj_accepts), "{full}: PROJ {proj}, terrio {:?}", accepted.err());
            differences.push(line);
            differing += 1;
        }
    }
    // A change in the strings that still differ shows here.
    assert_eq!(differing, 320, "strings that still differ from PROJ");
    differences.sort_unstable();
    // Compare lines, not text: a Windows checkout may give the fixture CRLF line endings.
    let expected: Vec<&str> = include_str!("data/proj_param_value_differences.tsv").lines().collect();
    assert_eq!(differences, expected);
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

#[test]
fn scale_factors_retain_the_documented_numeric_alias_policy() {
    let params = |d: &str| SpatialRef::parse(d).unwrap().projection_params().unwrap();
    // Known difference from PROJ 9.8.1: Terrio does not parse a scale's `r` suffix.
    // It retains the first fully numeric alias, or 1 without one; PROJ may use the numeric
    // prefix or reject the CRS, depending on the method.
    assert_eq!(params("+proj=tmerc +k_0=0.5r +ellps=WGS84").k_0, 1.0);
    assert_eq!(params("+proj=tmerc +k_0=0.5r +k=0.9996 +ellps=WGS84").k_0, 0.9996);
    // PROJ 9.8.1 refuses a nonpositive fallback too, while NaN still passes.
    for k in ["0", "-1"] {
        assert!(SpatialRef::parse(&format!("+proj=tmerc +k_0=0.5r +k={k} +ellps=WGS84")).is_err());
    }
}

/// GeoKey prime meridians follow GDAL 3.12.1 and PROJ 9.8.1's angular parsing.
#[test]
fn angular_prime_meridians_survive_geokey_round_trips() {
    // PROJ 9.8.1 reads these angular values in degrees; GeoKey 2061 stores degrees too.
    for (value, degrees) in
        [("0.5r", 0.5f64.to_degrees()), ("0.5R", 0.5f64.to_degrees()), ("2d20'14.025\"E", 2.0 + 20.0 / 60.0 + 14.025 / 3600.0)]
    {
        let crs = SpatialRef::parse(&format!("+proj=longlat +ellps=WGS84 +pm={value}")).unwrap();
        let keys = crs.to_geokeys().unwrap();
        let pm = keys.doubles.iter().find(|(key, _)| *key == 2061).unwrap().1;
        assert!((pm - degrees).abs() < 1e-12, "{value}: {pm}");
        let tags = keys.encode().unwrap();
        let back = SpatialRef::from_geokeys(&tags.directory, &tags.doubles, tags.ascii.as_bytes()).unwrap();
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        let expected = terrio_crs::Transformer::new(&wgs84, &crs).unwrap().transform(10.0, 10.0).unwrap();
        let actual = terrio_crs::Transformer::new(&wgs84, &back).unwrap().transform(10.0, 10.0).unwrap();
        assert!((actual.0 - expected.0).abs() < 1e-12 && (actual.1 - expected.1).abs() < 1e-12, "{value}: {actual:?} != {expected:?}");
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
    // The lenient extension also decides whether a declared authority matches: EPSG:4087
    // has this conversion; EPSG:3857 does not and must not replace it.
    for code in [4087, 3857] {
        let declared = format!(r#"{},AUTHORITY["EPSG","{code}"]]"#, &wkt[..wkt.len() - 1]);
        let crs = SpatialRef::parse(&declared).unwrap();
        assert_eq!(crs.epsg, (code == 4087).then_some(code));
        let t = terrio_crs::Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).unwrap();
        let (x, y) = t.transform(10.0, 10.0).unwrap();
        assert!((x - 1113194.9079327357).abs() < 1e-6 && (y - 1105854.833234372).abs() < 1e-6, "{code}: {x} {y}");
    }
    // A refused extension with an unsupported method and unusable WKT nodes must not be
    // silently repaired from a declared authority either.
    let unsupported = wkt
        .replace("+proj=eqc +lat_ts=abc +ellps=WGS84 +units=m", "+proj=not_real +k_0=0 +ellps=WGS84")
        .replace("Equirectangular", "not_real");
    let declared = format!(r#"{},AUTHORITY["EPSG","4087"]]"#, &unsupported[..unsupported.len() - 1]);
    let crs = SpatialRef::parse(&declared).unwrap();
    assert_eq!(crs.epsg, None);
    assert!(terrio_crs::Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).is_err());
    // Unsupported extension methods retain the WKT-node fallback with either feature set.
    let fallback = wkt
        .replace("+proj=eqc +lat_ts=abc +ellps=WGS84 +units=m", "+proj=not_real +ellps=WGS84")
        .replace(r#"PARAMETER["standard_parallel_1",10]"#, r#"PARAMETER["standard_parallel_1",0]"#);
    let crs = SpatialRef::parse(&fallback).unwrap();
    let t = terrio_crs::Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).unwrap();
    let (x, y) = t.transform(10.0, 10.0).unwrap();
    assert!((x - 1113194.9079327357).abs() < 1e-6 && (y - 1105854.833234372).abs() < 1e-6);
    // Legacy limitation: an unreadable list in an extension is ignored as a whole. PROJ
    // 9.8.1 accepts it too, but treats abc as zero and applies the remaining datum shift.
    let ignored = wkt.replace("+proj=eqc +lat_ts=abc +ellps=WGS84 +units=m", "+proj=eqc +ellps=WGS84 +towgs84=1,abc,3");
    let crs = SpatialRef::parse(&ignored).unwrap();
    let t = terrio_crs::Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).unwrap();
    let (x, y) = t.transform(10.0, 10.0).unwrap();
    assert!((x - 1113194.9079327357).abs() < 1e-6 && (y - 1105854.833234372).abs() < 1e-6);
    // PROJ 9.8.1 cannot construct a transformation from any of these extensions; the WKT
    // nodes must not silently replace the refused conversion.
    for extension in [
        "+proj=eqc +k_0=0 +ellps=WGS84",
        "+proj=eqc +to_meter=-1 +ellps=WGS84",
        "+proj=eqc +geoidgrids=@null +vto_meter=0 +ellps=WGS84",
        "+proj=eqc +ellps=WGS84 +towgs84=1,2,3,4,5,6",
        "+proj=eqc +ellps=WGS84 +towgs84=1,2",
    ] {
        let refused = wkt.replace("+proj=eqc +lat_ts=abc +ellps=WGS84 +units=m", extension);
        let crs = SpatialRef::parse(&refused).unwrap();
        assert!(terrio_crs::Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).is_err(), "{extension}");
        let declared = format!(r#"{},AUTHORITY["EPSG","4087"]]"#, &refused[..refused.len() - 1]);
        let crs = SpatialRef::parse(&declared).unwrap();
        assert_eq!(crs.epsg, None, "{extension}");
        assert!(terrio_crs::Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).is_err(), "{extension}");
    }
}
