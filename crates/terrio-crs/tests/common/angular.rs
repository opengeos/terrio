// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Compares transformed geographic coordinates with frozen PROJ references.

/// The larger of the east-west (scaled by the cosine of latitude, with longitude wrapped) and
/// north-south differences, in degrees, between `actual` and the `[lon, lat]` in `expected`.
pub fn angular_error(actual: (f64, f64), expected: &serde_json::Value) -> f64 {
    let lon = expected[0].as_f64().unwrap();
    let lat = expected[1].as_f64().unwrap();
    let dlon = (actual.0 - lon + 180.0).rem_euclid(360.0) - 180.0;
    (dlon * lat.to_radians().cos()).abs().max((actual.1 - lat).abs())
}
