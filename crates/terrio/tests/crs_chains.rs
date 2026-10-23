//! Transformations between CRSs whose datums no transformation of PROJ's database links
//! directly: terrio chains the transformations PROJ's database registers through intermediate
//! CRSs and datums (one or several, through registered concatenations) as PROJ lists them, and
//! picks among them for each point as PROJ does. The expected coordinates are GDAL's
//! (`osr.CoordinateTransformation`, GDAL 3.12.2 and PROJ 9.8.1, without grids), as
//! `scripts/compare_chains_with_proj.py` computes them.

use terrio::crs::{SpatialRef, Transformer};

/// Moves a point from one EPSG CRS to another, with no grid to be found.
fn moved(src: u32, dst: u32, x: f64, y: f64) -> (f64, f64) {
    static NO_GRIDS: std::sync::Once = std::sync::Once::new();
    NO_GRIDS.call_once(|| {
        // An empty directory of this process, kept for its tests.
        let empty = tempfile::tempdir().unwrap().keep();
        terrio::crs::set_grid_search_paths(Some(vec![empty]));
        terrio::crs::set_network_enabled(Some(false));
    });
    let (s, d) = (SpatialRef::from_epsg(src).unwrap(), SpatialRef::from_epsg(dst).unwrap());
    Transformer::new(&s, &d).unwrap().transform(x, y).unwrap()
}

/// Checks a point against GDAL's, within `tol` CRS units.
fn check(src: u32, dst: u32, (x, y): (f64, f64), (gx, gy): (f64, f64), tol: f64) {
    let (tx, ty) = moved(src, dst, x, y);
    assert!((tx - gx).abs() < tol && (ty - gy).abs() < tol, "EPSG:{src} to EPSG:{dst}: {tx} {ty}, GDAL {gx} {gy}");
}

#[test]
fn chains_through_several_datums() {
    // ED79 through WGS 84, Monte Mario and the registered concatenation Monte Mario (Rome) to
    // WGS 84 (1).
    check(4668, 4806, (15.73007346577983, 38.21059742073942), (3.2777401324798294, 38.21059742073942), 1e-8);
    // AGD66 to ITRF2014 through GDA94 between geographic and geocentric CRSs: PROJ's chains through
    // an intermediate datum, whatever its CRSs, when none goes through an intermediate CRS.
    check(4202, 9000, (136.215, -24.25), (136.21625051127447, -24.248516327248357), 1e-8);
    // NZGD49 to ITRF2014: the chains through NZGD2000 need its deformation model, which terrio
    // does not run, as PROJ without its grid: no shift.
    check(4272, 9000, (177.22336551717348, -34.17339641699847), (177.22336551717348, -34.17339641699847), 1e-8);
    // ONGD14 to WGS 84 (G1762): PROJ also looks for chains through datums when those it found
    // go through its own null transformation from WGS 84 to its realizations, and finds the
    // more accurate one between geocentric CRSs.
    check(7373, 9057, (62.871321271982325, 14.799036093843489), (62.871315221230546, 14.799025324116105), 1e-8);
}

#[test]
fn chains_as_proj_finds_them() {
    // Camacupa 1948 to WGS 84 (4) is replaced by transformations whose areas miss Schwarzeck's:
    // PROJ chains it.
    check(4220, 4293, (9.191499086899565, -16.961215728435572), (9.188414628575138, -16.960824386942015), 1e-8);
    // RGNC15 (lon-lat) through RGNC15, a CRS of its datum of another name.
    check(4642, 10312, (167.57266498944682, -22.680020277969277), (167.57599921128462, -22.67721822228873), 1e-8);
    // CH1903 (Bern) has no transformation of its own to WGS 84 to chain: PROJ's ballpark.
    check(4801, 9053, (10.09981412629195, 47.630871289991404), (17.53939745962195, 47.630871289991404), 1e-8);
    // The heights go through both transformations of a chain, as in PROJ's pipeline (0.5 mm).
    check(4178, 4746, (11.29, 50.925), (11.28958706760497, 50.9258004720906), 1e-9);
}

#[test]
fn chains_picked_as_proj() {
    // Between CRSs of one ellipsoid on other prime meridians, PROJ's ballpark is a longitude
    // rotation of 0 m accuracy, which wins over the chains.
    check(3906, 4815, (22.482025585632165, 41.64261792228423), (-1.2343119143678385, 41.64261792228423), 1e-8);
    // Unless the chains are of unknown accuracy.
    check(4646, 4810, (43.28749648078994, -11.915674212752858), (45.511216223523796, -13.242192530108346), 1e-8);
    // Of equal accuracy and area, the chain whose geocentric translations PROJ merges into one.
    check(4143, 4168, (-3.242700760063462, 1.8436136209706455), (-3.2424719435732903, 1.844780338896485), 1e-8);
    // An intersection of the areas of the two transformations that is neither's has no name,
    // which PROJ takes for an unknown area: the larger chain of known area wins.
    check(4258, 9140, (20.67290451159272, 41.93654080506045), (20.67290451159272, 41.93654080506045), 1e-8);
    // To a projected CRS, PROJ looks for the chains from it, and sorts them reversed.
    check(7877, 7882, (288397.56365986477, 1998519.7023704904), (288397.8122520231, 1998520.134328964), 1e-3);
}
