// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Boolean CRS flags use explicit values; canonical bare flags follow PROJ 9.8.1.

use terrio_crs::{SpatialRef, Transformer};

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
