// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The projection parameters and world projections terrio reads and writes, once (#1058 step 5).
//! A parameter has its EPSG code and name, the normalized names a WKT reader also takes for it, and
//! the kind of its value. The WKT 2 reader (`wkt2`) and the PROJJSON writers (`projjson_write`,
//! `projjson_gdal`) share them, as PROJ shares its `paramMappings` (`parammappings.cpp`). A world
//! projection has its PROJ name and the names the WKT 1 and WKT 2 readers and the WKT 1 writer
//! take for it.

/// The kind of a parameter's value: an angle (in degrees), a length (in metres) or a scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Angle,
    Length,
    Scale,
}

/// A projection parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Param {
    /// Its EPSG code; PROJ's parameters without one have codes down from `u32::MAX`, which match
    /// no `ID`.
    pub code: u32,
    /// Its EPSG name (PROJ's, without an EPSG code).
    pub name: &'static str,
    /// Its name normalized as [`crate::wkt1::norm`] normalizes it (checked by a test).
    pub key: &'static str,
    /// Other normalized names a WKT 2 reader takes for it (GDAL's WKT 1 names, older EPSG names).
    pub aliases: &'static [&'static str],
    pub kind: Kind,
}

impl Param {
    /// Whether a normalized name is the parameter's.
    pub(crate) fn named(&self, key: &str) -> bool {
        self.key == key || self.aliases.contains(&key)
    }
}

const fn param(code: u32, name: &'static str, key: &'static str, aliases: &'static [&'static str], kind: Kind) -> Param {
    Param { code, name, key, aliases, kind }
}

use Kind::{Angle, Length, Scale};

pub(crate) const LAT_NAT: Param = param(8801, "Latitude of natural origin", "latitudeofnaturalorigin", &["latitudeoforigin"], Angle);
pub(crate) const LON_NAT: Param = param(8802, "Longitude of natural origin", "longitudeofnaturalorigin", &["centralmeridian"], Angle);
pub(crate) const K_NAT: Param = param(8805, "Scale factor at natural origin", "scalefactoratnaturalorigin", &[], Scale);
pub(crate) const FE: Param = param(8806, "False easting", "falseeasting", &[], Length);
pub(crate) const FN: Param = param(8807, "False northing", "falsenorthing", &[], Length);
pub(crate) const LAT_CENTRE: Param = param(8811, "Latitude of projection centre", "latitudeofprojectioncentre", &[], Angle);
pub(crate) const LON_CENTRE: Param = param(8812, "Longitude of projection centre", "longitudeofprojectioncentre", &[], Angle);
pub(crate) const AZIMUTH: Param =
    param(8813, "Azimuth at projection centre", "azimuthatprojectioncentre", &["azimuthofinitialline"], Angle);
pub(crate) const GAMMA: Param = param(8814, "Angle from Rectified to Skew Grid", "anglefromrectifiedtoskewgrid", &[], Angle);
pub(crate) const K_CENTRE: Param =
    param(8815, "Scale factor at projection centre", "scalefactoratprojectioncentre", &["scalefactoroninitialline"], Scale);
pub(crate) const FE_CENTRE: Param = param(8816, "Easting at projection centre", "eastingatprojectioncentre", &[], Length);
pub(crate) const FN_CENTRE: Param = param(8817, "Northing at projection centre", "northingatprojectioncentre", &[], Length);
pub(crate) const LAT_PSEUDO_SP: Param = param(8818, "Latitude of pseudo standard parallel", "latitudeofpseudostandardparallel", &[], Angle);
pub(crate) const K_PSEUDO_SP: Param =
    param(8819, "Scale factor on pseudo standard parallel", "scalefactoronpseudostandardparallel", &[], Scale);
pub(crate) const LAT_FALSE: Param = param(8821, "Latitude of false origin", "latitudeoffalseorigin", &[], Angle);
pub(crate) const LON_FALSE: Param = param(8822, "Longitude of false origin", "longitudeoffalseorigin", &[], Angle);
pub(crate) const SP1: Param = param(8823, "Latitude of 1st standard parallel", "latitudeof1ststandardparallel", &[], Angle);
pub(crate) const SP2: Param = param(8824, "Latitude of 2nd standard parallel", "latitudeof2ndstandardparallel", &[], Angle);
pub(crate) const FE_FALSE: Param = param(8826, "Easting at false origin", "eastingatfalseorigin", &[], Length);
pub(crate) const FN_FALSE: Param = param(8827, "Northing at false origin", "northingatfalseorigin", &[], Length);
pub(crate) const LON_INITIAL: Param = param(8830, "Initial longitude", "initiallongitude", &[], Angle);
pub(crate) const ZONE_WIDTH: Param = param(8831, "Zone width", "zonewidth", &[], Angle);
pub(crate) const LAT_TS: Param = param(8832, "Latitude of standard parallel", "latitudeofstandardparallel", &[], Angle);
pub(crate) const LON_ORIGIN: Param = param(8833, "Longitude of origin", "longitudeoforigin", &[], Angle);
pub(crate) const LAT_TOPO: Param = param(8834, "Latitude of topocentric origin", "latitudeoftopocentricorigin", &[], Angle);
pub(crate) const LON_TOPO: Param = param(8835, "Longitude of topocentric origin", "longitudeoftopocentricorigin", &[], Angle);
pub(crate) const HEIGHT_TOPO: Param =
    param(8836, "Ellipsoidal height of topocentric origin", "ellipsoidalheightoftopocentricorigin", &[], Length);
pub(crate) const VIEW_HEIGHT: Param = param(8840, "Viewpoint height", "viewpointheight", &[], Length);
pub(crate) const CO_LAT_CONE: Param = param(1036, "Co-latitude of cone axis", "colatitudeofconeaxis", &[], Angle);
pub(crate) const K_ELLIPSOID: Param = param(1038, "Ellipsoid scaling factor", "ellipsoidscalingfactor", &[], Scale);

// PROJ's parameters without an EPSG code.
pub(crate) const SAT_HEIGHT: Param = param(u32::MAX, "Satellite Height", "satelliteheight", &[], Length);
pub(crate) const LAT_P1: Param = param(u32::MAX - 1, "Latitude of 1st point", "latitudeof1stpoint", &[], Angle);
pub(crate) const LON_P1: Param = param(u32::MAX - 2, "Longitude of 1st point", "longitudeof1stpoint", &[], Angle);
pub(crate) const LAT_P2: Param = param(u32::MAX - 3, "Latitude of 2nd point", "latitudeof2ndpoint", &[], Angle);
pub(crate) const LON_P2: Param = param(u32::MAX - 4, "Longitude of 2nd point", "longitudeof2ndpoint", &[], Angle);
pub(crate) const LAT_TRUE_SCALE: Param = param(u32::MAX - 5, "Latitude of true scale", "latitudeoftruescale", &[], Angle);

/// A world projection of a central meridian: its PROJ name, the normalized method name the WKT 2
/// reader takes for it, and its GDAL WKT 1 method, which the WKT 1 reader and writer take.
#[derive(Clone, Copy, Debug)]
pub(crate) struct World {
    pub proj: &'static str,
    pub wkt2: Option<&'static str>,
    pub wkt1: Option<&'static str>,
}

/// The world projections, in the WKT 2 reader's order of old.
pub(crate) const WORLD: [World; 28] = [
    World { proj: "moll", wkt2: Some("mollweide"), wkt1: Some("Mollweide") },
    World { proj: "robin", wkt2: Some("robinson"), wkt1: None },
    World { proj: "sinu", wkt2: Some("sinusoidal"), wkt1: None },
    World { proj: "eck1", wkt2: Some("eckerti"), wkt1: Some("Eckert_I") },
    World { proj: "eck2", wkt2: Some("eckertii"), wkt1: Some("Eckert_II") },
    World { proj: "eck3", wkt2: Some("eckertiii"), wkt1: Some("Eckert_III") },
    World { proj: "eck4", wkt2: Some("eckertiv"), wkt1: Some("Eckert_IV") },
    World { proj: "eck5", wkt2: Some("eckertv"), wkt1: Some("Eckert_V") },
    World { proj: "eck6", wkt2: Some("eckertvi"), wkt1: Some("Eckert_VI") },
    World { proj: "mill", wkt2: Some("millercylindrical"), wkt1: None },
    World { proj: "vandg", wkt2: Some("vandergrinten"), wkt1: Some("VanDerGrinten") },
    World { proj: "natearth", wkt2: Some("naturalearth"), wkt1: Some("Natural_Earth") },
    World { proj: "qua_aut", wkt2: Some("quarticauthalic"), wkt1: None },
    // PROJ's uninterrupted goode, which terrio refuses; it is not igh.
    World { proj: "goode", wkt2: Some("goodehomolosine"), wkt1: None },
    World { proj: "igh", wkt2: Some("interruptedgoodehomolosine"), wkt1: Some("Interrupted_Goode_Homolosine") },
    World { proj: "aitoff", wkt2: Some("aitoff"), wkt1: Some("Aitoff") },
    World { proj: "crast", wkt2: Some("crasterparabolic"), wkt1: None },
    World { proj: "natearth2", wkt2: Some("naturalearthii"), wkt1: None },
    World { proj: "patterson", wkt2: Some("patterson"), wkt1: None },
    World { proj: "times", wkt2: Some("times"), wkt1: None },
    World { proj: "mbtfpq", wkt2: Some("flatpolarquartic"), wkt1: None },
    World { proj: "wag1", wkt2: Some("wagneri"), wkt1: None },
    World { proj: "wag2", wkt2: Some("wagnerii"), wkt1: None },
    World { proj: "wag4", wkt2: Some("wagneriv"), wkt1: None },
    World { proj: "wag5", wkt2: Some("wagnerv"), wkt1: None },
    World { proj: "wag6", wkt2: Some("wagnervi"), wkt1: None },
    World { proj: "gall", wkt2: Some("gallstereographic"), wkt1: Some("Gall_Stereographic") },
    World { proj: "eqearth", wkt2: None, wkt1: Some("Equal_Earth") },
];

/// Every parameter of the table.
#[cfg(test)]
const ALL: [Param; 36] = [
    LAT_NAT,
    LON_NAT,
    K_NAT,
    FE,
    FN,
    LAT_CENTRE,
    LON_CENTRE,
    AZIMUTH,
    GAMMA,
    K_CENTRE,
    FE_CENTRE,
    FN_CENTRE,
    LAT_PSEUDO_SP,
    K_PSEUDO_SP,
    LAT_FALSE,
    LON_FALSE,
    SP1,
    SP2,
    FE_FALSE,
    FN_FALSE,
    LON_INITIAL,
    ZONE_WIDTH,
    LAT_TS,
    LON_ORIGIN,
    LAT_TOPO,
    LON_TOPO,
    HEIGHT_TOPO,
    VIEW_HEIGHT,
    CO_LAT_CONE,
    K_ELLIPSOID,
    SAT_HEIGHT,
    LAT_P1,
    LON_P1,
    LAT_P2,
    LON_P2,
    LAT_TRUE_SCALE,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Each parameter's key is its name normalized, and its code and names are its own. (The EPSG
    /// names and codes are those of PROJ 9.9.0's `proj.db`, `conversion_param`.)
    #[test]
    fn the_parameter_table_is_consistent() {
        for p in ALL {
            assert_eq!(p.key, crate::wkt1::norm(p.name), "{p:?}");
            let same = |q: &&Param| q.code == p.code || q.named(p.key) || p.aliases.iter().any(|a| q.named(a));
            assert_eq!(ALL.iter().filter(same).count(), 1, "{p:?}");
        }
    }
}
