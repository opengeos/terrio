//! Integration-side geometry checks against captured executable observations.
//! Kept outside the independently delivered repair module.

use super::repair::{IPt, P, Polygon, try_make_valid};
use serde_json::Value;

type PointKey = (u64, u64);

fn canonical_ring(ring: &[P]) -> Vec<PointKey> {
    let ordinate = |x: f64| if x == 0.0 { 0 } else { x.to_bits() };
    let mut points = ring.iter().map(|p| (ordinate(p.0), ordinate(p.1))).collect::<Vec<_>>();
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    let n = points.len();
    (0..n)
        .flat_map(|start| {
            let points = &points;
            [false, true]
                .map(move |reverse| (0..n).map(|i| points[if reverse { (start + n - i) % n } else { (start + i) % n }]).collect::<Vec<_>>())
        })
        .min()
        .unwrap_or_default()
}

fn canonical_polygons(polygons: &[Polygon<P>]) -> Vec<Vec<Vec<PointKey>>> {
    let mut result = polygons
        .iter()
        .map(|polygon| {
            let mut rings = polygon.iter().map(|ring| canonical_ring(ring)).collect::<Vec<_>>();
            // Keep the exterior first and its holes attached to this polygon.
            if let Some(holes) = rings.get_mut(1..) {
                holes.sort();
            }
            rings
        })
        .collect::<Vec<_>>();
    result.sort();
    result
}

#[test]
fn repair_fixture_ring_connectivity_matches_observations() {
    let fixture: Value = serde_json::from_str(include_str!("../../tests/data/make-valid.json")).unwrap();
    let cases = fixture["observations"].as_array().unwrap();
    assert_eq!(cases.len(), 51);
    for case in cases {
        // Valid inputs may acquire extra contact nodes under the agreed contract.
        if case["input_valid"].as_bool() == Some(true) {
            continue;
        }
        let input: Vec<Polygon<IPt>> = serde_json::from_value(case["input_polygons"].clone()).unwrap();
        let expected: Vec<Polygon<P>> = serde_json::from_value(case["polygonal_coordinates"].clone()).unwrap();
        let actual = try_make_valid(&input).unwrap();
        assert_eq!(canonical_polygons(&actual), canonical_polygons(&expected), "{}: ring connectivity", case["id"]);
    }
}
