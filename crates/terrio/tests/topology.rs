//! Planar topology contracts, measured on known shapes independently of the algorithm.
use terrio::ops::topology::{self, BufferOptions, CleanOptions, OverlayOperation as Op};
use terrio::{Feature, FieldDef, FieldType, FieldValue as V, Geometry, Layer};
use wbvector::Coord;

fn rect(x: f64, y: f64, w: f64, h: f64) -> Geometry {
    Geometry::polygon(vec![Coord::xy(x, y), Coord::xy(x + w, y), Coord::xy(x + w, y + h), Coord::xy(x, y + h), Coord::xy(x, y)], vec![])
}
fn bowtie() -> Geometry {
    Geometry::polygon(vec![Coord::xy(0., 0.), Coord::xy(2., 2.), Coord::xy(0., 2.), Coord::xy(2., 0.), Coord::xy(0., 0.)], vec![])
}
fn layer(shapes: Vec<Option<Geometry>>) -> Layer {
    let mut l = Layer::new("shapes").with_crs_epsg(3857);
    l.schema.add_field(FieldDef::new("kind", FieldType::Text));
    for (i, g) in shapes.into_iter().enumerate() {
        l.features.push(Feature { fid: u64::MAX - i as u64, geometry: g, attributes: vec![V::Text("a".into())] });
    }
    l
}
fn area(g: &Geometry) -> f64 {
    let ring = |r: &wbvector::Ring| {
        r.0.iter().zip(r.0.iter().cycle().skip(1)).take(r.0.len()).map(|(a, b)| a.x * b.y - b.x * a.y).sum::<f64>().abs() / 2.
    };
    match g {
        Geometry::Polygon { exterior, interiors } => ring(exterior) - interiors.iter().map(ring).sum::<f64>(),
        Geometry::MultiPolygon(ps) => ps.iter().map(|(e, i)| ring(e) - i.iter().map(ring).sum::<f64>()).sum(),
        Geometry::GeometryCollection(gs) => gs.iter().map(area).sum(),
        _ => 0.,
    }
}
fn total(l: &Layer) -> f64 {
    l.features.iter().filter_map(|f| f.geometry.as_ref()).map(area).sum()
}
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-6, "{a} != {b}");
}

#[test]
fn buffers_points_lines_polygons_and_nulls_in_crs_units() {
    let input = layer(vec![Some(Geometry::point(5., 5.)), None]);
    let out = topology::buffer(&input, &BufferOptions { distance: 10., ..Default::default() }).unwrap();
    assert!((total(&out) - 100. * std::f64::consts::PI).abs() < 0.2);
    assert_eq!(out.features[0].fid, u64::MAX);
    assert_eq!(out.features[0].attributes, input.features[0].attributes);
    assert!(out.features[1].geometry.is_none());
    assert!(terrio::crs::SpatialRef::from_layer(&out).same_as(&terrio::crs::SpatialRef::from_layer(&input)));
    let out = topology::buffer(&layer(vec![Some(rect(0., 0., 10., 10.))]), &BufferOptions { distance: -1., ..Default::default() }).unwrap();
    close(total(&out), 64.);
    let out = topology::buffer(&input, &BufferOptions { distance: -1., ..Default::default() }).unwrap();
    close(total(&out), 0.);
    let lines = layer(vec![Some(Geometry::line_string(vec![Coord::xy(0., 0.), Coord::xy(10., 0.)]))]);
    let out = topology::buffer(&lines, &BufferOptions { distance: 1., ..Default::default() }).unwrap();
    assert!((total(&out) - (20. + std::f64::consts::PI)).abs() < 0.01);
    let geographic = Layer { crs: Layer::new("x").with_crs_epsg(4326).crs, ..input };
    let out = topology::buffer(&geographic, &BufferOptions { distance: 1., ..Default::default() }).unwrap();
    let coords = out.features[0].geometry.as_ref().unwrap().all_coords();
    close(coords.iter().map(|c| c.x).fold(f64::NEG_INFINITY, f64::max), 6.);
}

#[test]
fn overlay_areas_attributes_ids_and_empty_results() {
    let a = layer(vec![Some(rect(0., 0., 2., 2.))]);
    let b = layer(vec![Some(rect(1., 0., 2., 2.))]);
    for (op, expected, n) in [(Op::Intersection, 2., 1), (Op::Union, 6., 3), (Op::Difference, 2., 1), (Op::SymmetricDifference, 4., 2)] {
        let out = topology::overlay(&a, &b, op).unwrap();
        close(total(&out), expected);
        assert_eq!(out.features.len(), n);
        assert_eq!(out.schema.fields()[0].name, "a_kind");
        assert_eq!(out.schema.fields()[1].name, "b_kind");
        for (i, f) in out.features.iter().enumerate() {
            assert_eq!(f.fid, i as u64 + 1);
            assert_eq!(f.attributes.len(), 4);
        }
        if op == Op::Intersection {
            assert_eq!(out.features[0].attributes[2], V::Text(u64::MAX.to_string()));
        }
        if op == Op::Difference {
            assert_eq!(out.features[0].attributes[1], V::Null);
            assert_eq!(out.features[0].attributes[3], V::Null);
        }
    }
    let touching = layer(vec![Some(rect(2., 0., 2., 2.))]);
    assert!(topology::overlay(&a, &touching, Op::Intersection).unwrap().features.is_empty());
    let empty = layer(vec![None, Some(Geometry::MultiPolygon(vec![]))]);
    close(total(&topology::overlay(&a, &empty, Op::Union).unwrap()), 4.);
    assert!(topology::overlay(&empty, &empty, Op::Intersection).unwrap().features.is_empty());
    let mut wrong = b.clone();
    wrong.crs = Layer::new("wrong").with_crs_epsg(4326).crs;
    assert!(topology::overlay(&a, &wrong, Op::Union).is_err());
    assert!(topology::overlay(&a, &layer(vec![Some(bowtie())]), Op::Union).is_err());
    assert!(topology::overlay(&a, &layer(vec![Some(Geometry::point(0., 0.))]), Op::Union).is_err());
}

#[test]
fn disjoint_overlay_keeps_both_sides_and_their_attributes() {
    let mut a =
        layer(vec![Some(rect(-10., -10., 2., 2.)), None, Some(Geometry::MultiPolygon(vec![])), Some(Geometry::polygon(vec![], vec![]))]);
    let mut b =
        layer(vec![Some(rect(10., 10., 3., 3.)), None, Some(Geometry::MultiPolygon(vec![])), Some(Geometry::polygon(vec![], vec![]))]);
    a.features[0].attributes[0] = V::Text("left".into());
    b.features[0].attributes[0] = V::Text("right".into());
    for (op, expected_area, count) in [(Op::Difference, 4., 1), (Op::Union, 13., 2), (Op::SymmetricDifference, 13., 2)] {
        let out = topology::overlay(&a, &b, op).unwrap();
        close(total(&out), expected_area);
        assert_eq!(out.len(), count);
        assert_eq!(out.features[0].attributes, vec![V::Text("left".into()), V::Null, V::Text(u64::MAX.to_string()), V::Null]);
        if count == 2 {
            assert_eq!(out.features[1].attributes, vec![V::Null, V::Text("right".into()), V::Null, V::Text(u64::MAX.to_string())]);
        }
        assert!(topology::validity(&out).features.iter().all(|f| f.attributes[4] == V::Boolean(true)));
    }
    assert!(topology::overlay(&a, &b, Op::Intersection).unwrap().is_empty());
}

#[test]
fn indexed_overlay_preserves_pair_order_and_unions_candidate_coverage() {
    let a = layer(vec![Some(rect(0., 0., 2., 2.)), Some(rect(20., 0., 2., 2.))]);
    let b = layer(vec![Some(rect(21., 0., 2., 2.)), Some(rect(1., 0., 2., 2.)), Some(rect(-100., 0., 1., 1.))]);
    for (op, expected, count) in [(Op::Intersection, 4., 2), (Op::Union, 13., 7), (Op::Difference, 4., 2), (Op::SymmetricDifference, 9., 5)]
    {
        let out = topology::overlay(&a, &b, op).unwrap();
        close(total(&out), expected);
        assert_eq!(out.len(), count);
        if matches!(op, Op::Intersection | Op::Union) {
            assert_eq!(out.features[0].attributes[2..], [V::Text(u64::MAX.to_string()), V::Text((u64::MAX - 1).to_string())]);
            assert_eq!(out.features[1].attributes[2..], [V::Text((u64::MAX - 1).to_string()), V::Text(u64::MAX.to_string())]);
        }
    }
    let a = layer(vec![Some(rect(0., 0., 10., 1.))]);
    let b = layer(vec![Some(rect(2., 0., 4., 1.)), Some(rect(4., 0., 4., 1.)), Some(rect(100., 0., 1., 1.))]);
    close(total(&topology::overlay(&a, &b, Op::Difference).unwrap()), 4.);
}

#[test]
fn dissolve_groups_and_nulls_preserve_only_group_attributes() {
    let mut l = layer(vec![Some(rect(0., 0., 2., 2.)), Some(rect(1., 0., 2., 2.)), None]);
    l.features[2].attributes[0] = V::Null;
    let out = topology::dissolve(&l, &["kind".into()]).unwrap();
    assert_eq!(out.features.len(), 2);
    close(total(&out), 6.);
    assert_eq!(out.features[0].attributes, vec![V::Text("a".into()), V::Integer(2)]);
    assert_eq!(out.features[1].attributes, vec![V::Null, V::Integer(1)]);
    assert!(out.features[1].geometry.is_none());
    let all = topology::dissolve(&l, &[]).unwrap();
    assert_eq!(all.schema.len(), 1);
    assert_eq!(all.features[0].attributes, vec![V::Integer(3)]);
    assert!(topology::dissolve(&l, &["missing".into()]).is_err());
    assert!(topology::dissolve(&l, &["kind".into(), "kind".into()]).is_err());
    assert!(topology::dissolve(&layer(vec![]), &[]).unwrap().features.is_empty());
    assert!(topology::dissolve(&layer(vec![Some(Geometry::point(0., 0.))]), &[]).is_err());
}

#[test]
fn validity_reports_and_repairs_type_changes_without_losing_features() {
    let mut l = layer(vec![Some(bowtie()), Some(rect(4., 0., 2., 2.)), None, Some(Geometry::LineString(vec![]))]);
    l.schema = Default::default();
    l.schema.add_field(FieldDef::new("is_valid", FieldType::Text));
    let report = topology::validity(&l);
    assert_eq!(report.schema.fields()[1].name, "is_valid_2");
    assert_eq!(report.features[0].attributes[1], V::Boolean(false));
    assert_eq!(report.features[1].attributes[1], V::Boolean(true));
    assert_eq!(report.features[2].attributes[1], V::Null);
    assert_eq!(report.features[3].attributes[1], V::Boolean(true));
    assert!(report.features[0].attributes[2].as_str().is_some_and(|s| !s.is_empty()));
    let fixed = topology::make_valid(&l).unwrap();
    close(total(&fixed), 6.);
    assert!(matches!(fixed.features[0].geometry, Some(Geometry::MultiPolygon(_))));
    for (a, b) in l.features.iter().zip(&fixed.features) {
        assert_eq!(a.fid, b.fid);
        assert_eq!(a.attributes, b.attributes);
    }
    assert_eq!(topology::validity(&fixed).features[0].attributes[1], V::Boolean(true));
    let mut c = Coord::xyz(0., 0., 3.);
    c.m = Some(4.);
    let point = layer(vec![Some(Geometry::Point(c))]);
    assert_eq!(topology::make_valid(&point).unwrap().features[0].geometry, point.features[0].geometry);
    let collapsed = Geometry::polygon(vec![Coord::xy(0., 0.), Coord::xy(1., 0.), Coord::xy(2., 0.), Coord::xy(0., 0.)], vec![]);
    let fixed = topology::make_valid(&layer(vec![Some(collapsed)])).unwrap();
    close(total(&fixed), 0.);
    assert!(topology::make_valid(&layer(vec![Some(Geometry::point(f64::NAN, 0.))])).is_err());
}

#[test]
fn cleaning_snaps_deduplicates_closes_and_drops_collapsed_parts() {
    let mut a = Coord::xyz(0.1, 0.1, 3.);
    a.m = Some(7.);
    let l =
        layer(vec![Some(Geometry::LineString(vec![a, Coord::xyz(0.2, 0.2, 9.), Coord::xy(0.9, 0.1)])), Some(rect(0., 0., 0.1, 0.1)), None]);
    let out = topology::clean(&l, &CleanOptions { snap_grid: 0.5 }).unwrap();
    let Some(Geometry::LineString(cs)) = &out.features[0].geometry else { panic!("line") };
    assert_eq!(cs.len(), 2);
    assert_eq!((cs[0].x, cs[0].y, cs[0].z, cs[0].m), (0., 0., Some(3.), Some(7.)));
    assert!(out.features[1].geometry.is_none());
    assert!(out.features[2].geometry.is_none());
    let mut point = Coord::xyz(0.25, -0.25, 6.);
    point.m = Some(8.);
    let snapped = topology::clean(&layer(vec![Some(Geometry::Point(point))]), &CleanOptions { snap_grid: 0.5 }).unwrap();
    let Some(Geometry::Point(c)) = &snapped.features[0].geometry else { panic!("point") };
    assert_eq!((c.x, c.y, c.z, c.m), (0.5, -0.5, Some(6.), Some(8.)));
    // The empty exterior drops its holes during cleanup, so check all coordinates first.
    let bad_hole = Geometry::Polygon { exterior: wbvector::Ring(vec![]), interiors: vec![wbvector::Ring(vec![Coord::xy(f64::NAN, 0.)])] };
    let error = topology::clean(&layer(vec![Some(bad_hole)]), &Default::default()).unwrap_err();
    assert!(matches!(error, terrio::Error::InvalidArgument(_)));
    let unclosed = Geometry::polygon(vec![Coord::xy(0., 0.), Coord::xy(1., 0.), Coord::xy(1., 1.), Coord::xy(0., 1.)], vec![]);
    let out = topology::clean(&layer(vec![Some(unclosed)]), &Default::default()).unwrap();
    let Some(Geometry::Polygon { exterior, .. }) = &out.features[0].geometry else { panic!("polygon") };
    assert_eq!(exterior.0.first(), exterior.0.last());
    for grid in [-1., f64::NAN, f64::INFINITY] {
        assert!(topology::clean(&l, &CleanOptions { snap_grid: grid }).is_err());
    }
    for distance in [f64::NAN, f64::INFINITY] {
        assert!(topology::buffer(&l, &BufferOptions { distance, ..Default::default() }).is_err());
    }
    assert!(topology::buffer(&l, &BufferOptions { distance: 1., quadrant_segments: 0 }).is_err());
}

#[test]
fn polygon_holes_collections_and_typed_groups() {
    let mut donut = rect(0., 0., 10., 10.);
    let Geometry::Polygon { interiors, .. } = &mut donut else { unreachable!() };
    let Geometry::Polygon { exterior, .. } = rect(3., 3., 4., 4.) else { unreachable!() };
    interiors.push(exterior);
    let l = layer(vec![Some(donut.clone())]);
    close(total(&topology::buffer(&l, &Default::default()).unwrap()), 84.);
    assert!(topology::overlay(&l, &layer(vec![Some(rect(4., 4., 2., 2.))]), Op::Intersection).unwrap().is_empty());
    close(total(&topology::dissolve(&layer(vec![Some(donut), Some(rect(3., 3., 4., 4.))]), &[]).unwrap()), 100.);
    let collapsed = Geometry::polygon(vec![Coord::xy(0., 0.), Coord::xy(1., 0.), Coord::xy(2., 0.), Coord::xy(0., 0.)], vec![]);
    let collection = layer(vec![Some(Geometry::GeometryCollection(vec![collapsed, Geometry::point(3., 4.)]))]);
    let fixed = topology::clean(&collection, &Default::default()).unwrap();
    let Some(Geometry::GeometryCollection(gs)) = &fixed.features[0].geometry else { panic!("collection") };
    assert_eq!(gs.len(), 1);
    let mut groups = layer(vec![Some(rect(0., 0., 1., 1.)), Some(rect(1., 0., 1., 1.))]);
    groups.schema = Default::default();
    groups.schema.add_field(FieldDef::new("number", FieldType::Float));
    groups.features[0].attributes = vec![V::Float(-0.)];
    groups.features[1].attributes = vec![V::Float(0.)];
    assert_eq!(topology::dissolve(&groups, &["number".into()]).unwrap().len(), 1);
    groups.features[0].attributes[0] = V::Float(f64::NAN);
    assert!(topology::dissolve(&groups, &["number".into()]).is_err());
    let invalid = topology::validity(&layer(vec![Some(Geometry::point(f64::INFINITY, 0.))]));
    assert_eq!(invalid.features[0].attributes[1], V::Boolean(false));
}
