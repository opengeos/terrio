//! Dimension names: PDAL's (`ReturnNumber`, `GpsTime`, `NormalX`) and terrio's, which are laspy's
//! for the LAS dimensions (`return_number`, `gps_time`) and PDAL's in snake case for the others
//! (`normal_x`, `origin_id`).

/// PDAL's names of the LAS dimensions, and terrio's.
const LAS_NAMES: &[(&str, &str)] = &[
    ("X", "x"),
    ("Y", "y"),
    ("Z", "z"),
    ("Intensity", "intensity"),
    ("ReturnNumber", "return_number"),
    ("NumberOfReturns", "number_of_returns"),
    ("ScanDirectionFlag", "scan_direction_flag"),
    ("EdgeOfFlightLine", "edge_of_flight_line"),
    ("Classification", "classification"),
    ("Synthetic", "synthetic"),
    ("KeyPoint", "key_point"),
    ("Withheld", "withheld"),
    ("Overlap", "overlap"),
    ("ScanChannel", "scanner_channel"),
    ("ScanAngleRank", "scan_angle_rank"),
    ("UserData", "user_data"),
    ("PointSourceId", "point_source_id"),
    ("GpsTime", "gps_time"),
    ("Red", "red"),
    ("Green", "green"),
    ("Blue", "blue"),
    ("Infrared", "nir"),
];

/// PDAL's other standard dimensions, which PDAL's readers recognize whatever their case.
const PDAL_NAMES: &[&str] = &[
    "W",
    "Amplitude",
    "Reflectance",
    "InternalTime",
    "OffsetTime",
    "IsPpsLocked",
    "StartPulse",
    "ReflectedPulse",
    "Pdop",
    "Pitch",
    "Roll",
    "PulseWidth",
    "Deviation",
    "PassiveSignal",
    "BackgroundRadiation",
    "PassiveX",
    "PassiveY",
    "PassiveZ",
    "XVelocity",
    "YVelocity",
    "ZVelocity",
    "Azimuth",
    "WanderAngle",
    "Flag",
    "Mark",
    "Alpha",
    "EchoRange",
    "HeightAboveGround",
    "ClassFlags",
    "PointId",
    "OriginId",
    "NormalX",
    "NormalY",
    "NormalZ",
    "Curvature",
    "Density",
    "ClusterID",
    "NNDistance",
    "Linearity",
    "Planarity",
    "Scattering",
    "Verticality",
    "Omnivariance",
    "Anisotropy",
    "Eigenentropy",
    "Coplanar",
    "Rank",
    "SphericalRange",
    "SphericalAzimuth",
    "SphericalElevation",
];

/// terrio's name of a PDAL dimension: the laspy name of a LAS dimension, otherwise the name in
/// snake case (`NormalX` is `normal_x`).
pub fn from_pdal(name: &str) -> String {
    if let Some((_, t)) = LAS_NAMES.iter().find(|(p, _)| *p == name) {
        return (*t).to_string();
    }
    snake_case(name)
}

/// PDAL's name of a terrio dimension: `return_number` is `ReturnNumber`, `normal_x` is `NormalX`.
pub fn to_pdal(name: &str) -> String {
    if let Some((p, _)) = LAS_NAMES.iter().find(|(_, t)| *t == name) {
        return (*p).to_string();
    }
    if let Some(p) = PDAL_NAMES.iter().find(|p| snake_case(p) == name) {
        return (*p).to_string();
    }
    name.split('_').map(|w| w.chars().take(1).flat_map(char::to_uppercase).chain(w.chars().skip(1)).collect::<String>()).collect()
}

/// Other names PDAL recognizes for its dimensions.
const ALIASES: &[(&str, &str)] = &[
    ("class", "Classification"),
    ("cls", "Classification"),
    ("diffuse_red", "Red"),
    ("diffuse_green", "Green"),
    ("diffuse_blue", "Blue"),
    ("NearInfrared", "Infrared"),
    ("nx", "NormalX"),
    ("normal_x", "NormalX"),
    ("normal x", "NormalX"),
    ("ny", "NormalY"),
    ("normal_y", "NormalY"),
    ("normal y", "NormalY"),
    ("nz", "NormalZ"),
    ("normal_z", "NormalZ"),
    ("normal z", "NormalZ"),
    ("HAG", "HeightAboveGround"),
];

/// terrio's name of a dimension a file names freely (a PLY property): a PDAL dimension named in
/// any case (`red`, `ReturnNumber`, `returnnumber`) takes terrio's name, other names are kept.
pub fn from_any(name: &str) -> String {
    if super::las::STANDARD_DIMENSIONS.contains(&name) {
        return name.to_string();
    }
    if let Some((_, p)) = ALIASES.iter().find(|(a, _)| a.eq_ignore_ascii_case(name)) {
        return from_pdal(p);
    }
    if let Some((_, t)) = LAS_NAMES.iter().find(|(p, t)| p.eq_ignore_ascii_case(name) || t.eq_ignore_ascii_case(name)) {
        return (*t).to_string();
    }
    if let Some(p) = PDAL_NAMES.iter().find(|p| p.eq_ignore_ascii_case(name)) {
        return snake_case(p);
    }
    name.to_string()
}

/// The name PDAL's `writers.ply` gives a dimension: PDAL's name in lower case (`returnnumber`,
/// `gpstime`, `normalx`) for PDAL's dimensions, the name itself for others.
pub fn ply_name(name: &str) -> String {
    if let Some((p, _)) = LAS_NAMES.iter().find(|(_, t)| *t == name) {
        return p.to_ascii_lowercase();
    }
    if let Some(p) = PDAL_NAMES.iter().find(|p| snake_case(p) == name) {
        return p.to_ascii_lowercase();
    }
    name.to_string()
}

/// A name in snake case: an underscore before each capital that follows a lower-case letter or a
/// digit, then everything in lower case (`NormalX` is `normal_x`, `ClusterID` is `cluster_id`).
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev: Option<char> = None;
    for c in name.chars() {
        if c.is_ascii_uppercase() && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
        prev = Some(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_pdal_names() {
        assert_eq!(from_pdal("ReturnNumber"), "return_number");
        assert_eq!(from_pdal("Infrared"), "nir");
        assert_eq!(from_pdal("NormalX"), "normal_x");
        assert_eq!(from_pdal("OriginId"), "origin_id");
        assert_eq!(from_pdal("ClusterID"), "cluster_id");
        assert_eq!(to_pdal("scanner_channel"), "ScanChannel");
        assert_eq!(to_pdal("normal_x"), "NormalX");
        assert_eq!(to_pdal("cluster_id"), "ClusterID");
        assert_eq!(to_pdal("my_dim"), "MyDim");
        assert_eq!(from_any("returnnumber"), "return_number");
        assert_eq!(from_any("Red"), "red");
        assert_eq!(from_any("normalx"), "normal_x");
        assert_eq!(from_any("nx"), "normal_x");
        assert_eq!(from_any("scan_angle"), "scan_angle");
        assert_eq!(from_any("omg"), "omg");
    }
}
