// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Boolean CRS flags use explicit values; canonical bare flags follow PROJ 9.8.1.

use terrio_crs::{SpatialRef, Transformer};

#[test]
fn boolean_metadata_uses_the_same_values_and_precedence_as_the_parser() {
    let parameters = terrio_crs::proj_boolean_parameters(r#"+proj=utm +south=False +south=True +no_defs +over="YES" +lat_0=0"#).unwrap();
    assert_eq!(parameters, [("south".into(), false), ("no_defs".into(), true), ("over".into(), true)]);
    assert!(terrio_crs::proj_boolean_parameters("+south=maybe").is_err());
    assert!(terrio_crs::proj_boolean_parameters("+no_defs=\"true").is_err());
}

#[test]
fn false_south_flags_preserve_northern_utm_and_ups() {
    let source = SpatialRef::from_epsg(4326).unwrap();
    for (proj, north, south, point) in [("utm +zone=33", 32633, 32733, (15.0, -30.0)), ("ups", 32661, 32761, (0.0, 85.0))] {
        for (value, enabled) in [("False", false), ("f", false), ("F", false), ("0", false), ("True", true), ("1", true)] {
            let code = if enabled { south } else { north };
            let expected = Transformer::new(&source, &SpatialRef::from_epsg(code).unwrap()).unwrap().transform(point.0, point.1).unwrap();
            let definition = format!("+proj={proj} +south={value} +datum=WGS84 +units=m");
            let crs = SpatialRef::from_proj4(&definition).unwrap();
            for parsed in [crs.clone(), SpatialRef::from_wkt(&crs.to_wkt().unwrap())] {
                let actual = Transformer::new(&source, &parsed).unwrap().transform(point.0, point.1).unwrap();
                assert!(
                    (actual.0 - expected.0).abs() < 1e-7 && (actual.1 - expected.1).abs() < 1e-7,
                    "{definition}: {actual:?} != {expected:?}"
                );
            }
        }
    }
    for value in ["", "invalid", "2", "NaN"] {
        assert!(SpatialRef::from_proj4(&format!("+proj=utm +zone=33 +south={value}")).is_err());
        assert!(SpatialRef::from_proj4(&format!("+init=epsg:32633 +south={value}")).is_err());
    }
}

#[test]
fn init_boolean_overrides_fail_instead_of_silently_using_registry_defaults() {
    for flag in ["+south", "+south=True", "+south=False", "+over=True"] {
        let error = SpatialRef::from_proj4(&format!("+init=epsg:32633 {flag}")).unwrap_err();
        assert!(error.to_string().contains("overrides combined with +init"), "{error}");
    }
    assert!(SpatialRef::from_proj4("+init=epsg:32633 +no_defs=True +wktext=False").is_ok());
}
