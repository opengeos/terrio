//! Geometry operations, exact raster footprints and multi-raster point sampling.

use terrio::ops::footprint::{FootprintOptions, footprint};
use terrio::ops::geometry::{centroids, singleparts, vertices};
use terrio::ops::sample::{SampleInput, SampleOptions, sample, sample_with_options};
use terrio::vector::{Coord, Ring};
use terrio::{DataType, Feature, FieldDef, FieldType, FieldValue as V, Geometry as G, Layer, Raster, RasterConfig, RasterDataset};

fn layer(geometries: Vec<Option<G>>) -> Layer {
    let mut l = Layer::new("input");
    l.set_crs_epsg(Some(4326));
    l.schema.add_field(FieldDef::new("label", FieldType::Text));
    for (i, geometry) in geometries.into_iter().enumerate() {
        l.features.push(Feature { fid: 100 + i as u64, geometry, attributes: vec![V::Text(format!("feature {i}"))] });
    }
    l
}

fn ring(points: &[(f64, f64)]) -> Ring {
    Ring(points.iter().map(|&(x, y)| Coord::xy(x, y)).collect())
}

fn xy(g: &Option<G>) -> (f64, f64) {
    let Some(G::Point(c)) = g else { panic!("expected point: {g:?}") };
    (c.x, c.y)
}

fn raster(cols: usize, rows: usize, bands: usize, values: &[f64], nd: Option<f64>) -> RasterDataset {
    let cfg = RasterConfig { cols, rows, bands, data_type: DataType::F64, nodata: nd.unwrap_or(0.0), ..Default::default() };
    let mut ds = RasterDataset::new(Raster::from_data(cfg, values.to_vec()).unwrap(), nd.is_some());
    ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
    ds
}

#[test]
fn centroids_weight_dimensions_and_subtract_holes_regardless_of_winding() {
    let square = ring(&[(0.0, 0.0), (6.0, 0.0), (6.0, 6.0), (0.0, 6.0), (0.0, 0.0)]);
    let hole = ring(&[(1.0, 1.0), (3.0, 1.0), (3.0, 3.0), (1.0, 3.0), (1.0, 1.0)]);
    let polygon = G::Polygon { exterior: square, interiors: vec![hole] };
    let line = G::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(4.0, 0.0), Coord::xy(4.0, 2.0)]);
    let l = layer(vec![
        Some(polygon.clone()),
        Some(line.clone()),
        Some(G::GeometryCollection(vec![G::point(900.0, 900.0), line, polygon])),
        Some(G::MultiPoint(vec![Coord::xy(0.0, 0.0), Coord::xy(10.0, 4.0)])),
        Some(G::LineString(vec![Coord::xy(3.0, 7.0), Coord::xy(3.0, 7.0)])),
        Some(G::LineString(vec![])),
        None,
    ]);
    let out = centroids(&l).unwrap();
    assert_eq!(xy(&out.features[0].geometry), (3.125, 3.125));
    let (x, y) = xy(&out.features[1].geometry);
    assert!((x - 8.0 / 3.0).abs() < 1e-12 && (y - 1.0 / 3.0).abs() < 1e-12);
    assert_eq!(xy(&out.features[2].geometry), (3.125, 3.125));
    assert_eq!(xy(&out.features[3].geometry), (5.0, 2.0));
    assert_eq!(xy(&out.features[4].geometry), (3.0, 7.0));
    assert!(out.features[5].geometry.is_none() && out.features[6].geometry.is_none());
    assert_eq!(out.crs_epsg(), Some(4326));
    for (a, b) in out.features.iter().zip(&l.features) {
        assert_eq!(a.fid, b.fid);
        assert_eq!(a.attributes, b.attributes);
    }
    let bad = layer(vec![Some(G::point(f64::NAN, 1.0))]);
    assert!(centroids(&bad).unwrap_err().to_string().contains("feature 100"));
}

#[test]
fn centroid_of_small_polygon_at_large_projected_coordinates() {
    let offset = 1e9;
    let l = layer(vec![Some(G::Polygon {
        exterior: ring(&[(offset, offset), (offset + 2.0, offset), (offset + 2.0, offset + 2.0), (offset, offset + 2.0)]),
        interiors: vec![],
    })]);
    assert_eq!(xy(&centroids(&l).unwrap().features[0].geometry), (offset + 1.0, offset + 1.0));
}

#[test]
fn splitting_and_vertices_preserve_dimensions_attributes_and_provenance() {
    let c = Coord { x: 1.0, y: 2.0, z: Some(3.0), m: Some(4.0) };
    let mut l = layer(vec![
        Some(G::GeometryCollection(vec![
            G::MultiPoint(vec![c.clone(), Coord::xy(5.0, 6.0)]),
            G::Polygon { exterior: ring(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]), interiors: vec![] },
        ])),
        None,
    ]);
    l.features[0].fid = u64::MAX;
    l.schema.add_field(FieldDef::new("SOURCE_FID", FieldType::Text));
    l.features[0].attributes.push(V::Text("original".into()));
    let out = singleparts(&l);
    assert_eq!(out.features.iter().map(|f| f.fid).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(out.features[0].attributes, l.features[0].attributes);
    assert_eq!(out.features[3].geometry, None);
    let nodes = vertices(&l);
    assert_eq!(nodes.len(), 6);
    assert_eq!(nodes.schema.fields()[2].name, "source_fid_2");
    let G::Point(first) = nodes.features[0].geometry.as_ref().unwrap() else { panic!() };
    assert_eq!((first.z, first.m), (Some(3.0), Some(4.0)));
    assert_eq!(&nodes.features[0].attributes[2..], &[V::Text(u64::MAX.to_string()), V::Integer(0), V::Null, V::Integer(0)]);
    assert_eq!(&nodes.features[5].attributes[3..], &[V::Integer(2), V::Integer(0), V::Integer(3)]);
    assert_eq!(nodes.crs_epsg(), Some(4326));
    l.schema.add_field(FieldDef::new("missing", FieldType::Text));
    let padded = vertices(&l);
    assert_eq!(padded.len(), 6);
    for (a, b) in padded.features.iter().zip(&nodes.features) {
        assert_eq!(a.geometry, b.geometry);
        assert_eq!(a.attributes[2], V::Null);
        assert_eq!(&a.attributes[..2], &b.attributes[..2]);
        assert_eq!(&a.attributes[3..], &b.attributes[2..]);
    }
}

fn sample_inputs(ds: &RasterDataset) -> [SampleInput<'_>; 1] {
    [SampleInput { field_name: "value".into(), raster: ds, band: 1 }]
}

#[test]
fn sampling_multiple_bands_nulls_boundaries_and_colliding_names() {
    let r = raster(2, 2, 2, &[0.0, -1.0, 3.0, f64::NAN, 10.0, 20.0, 30.0, 40.0], Some(-1.0));
    let mut l = layer(vec![
        Some(G::point(0.5, 1.5)),
        Some(G::point(1.5, 1.5)),
        Some(G::point(1.5, 0.5)),
        Some(G::point(-0.001, 1.5)),
        Some(G::point(2.0, 1.5)),
        Some(G::point(0.0, 2.0)),
        None,
    ]);
    l.crs = None;
    let input =
        [SampleInput { field_name: "label".into(), raster: &r, band: 1 }, SampleInput { field_name: "label".into(), raster: &r, band: 2 }];
    let out = sample(&l, &input).unwrap();
    assert_eq!(out.schema.fields()[1].name, "label_2");
    assert_eq!(out.schema.fields()[2].name, "label_3");
    assert_eq!(out.features[0].attributes[1..], [V::Float(0.0), V::Float(10.0)]);
    assert_eq!(out.features[1].attributes[1..], [V::Null, V::Float(20.0)]);
    assert_eq!(out.features[2].attributes[1..], [V::Null, V::Float(40.0)]);
    for &i in &[3, 4, 6] {
        assert_eq!(out.features[i].attributes[1..], [V::Null, V::Null]);
    }
    assert_eq!(out.features[5].attributes[1..], [V::Float(0.0), V::Float(10.0)]);
    assert_eq!(out.features[0].geometry, l.features[0].geometry);
    let mut r = r;
    r.has_nodata = false;
    assert_eq!(sample(&l, &sample_inputs(&r)).unwrap().features[1].attributes[1], V::Float(-1.0));
}

#[test]
fn sampling_reprojects_and_inverts_rotated_grids() {
    let mut r = raster(2, 2, 1, &[1.0, 2.0, 3.0, 4.0], None);
    r.raster.crs.epsg = Some(3857);
    r.set_geo_transform([0.0, 100000.0, 25000.0, 200000.0, 5000.0, -100000.0]);
    let t = terrio::crs::Transformer::new(&r.crs(), &terrio::crs::SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (x, y) = t.transform(162500.0, 157500.0).unwrap(); // centre of col 1, row 0
    let l = layer(vec![Some(G::point(x, y))]);
    let out = sample(&l, &sample_inputs(&r)).unwrap();
    assert_eq!(out.features[0].attributes[1], V::Float(2.0));
    assert_eq!(out.features[0].geometry, l.features[0].geometry);
    let mut missing = r.clone();
    missing.raster.crs = Default::default();
    assert!(sample(&l, &sample_inputs(&missing)).is_err());
    for band in [0, 2] {
        assert!(sample(&l, &[SampleInput { field_name: "value".into(), raster: &r, band }]).is_err());
    }
    let bad = layer(vec![Some(G::MultiPoint(vec![Coord::xy(x, y)]))]);
    assert!(sample(&bad, &sample_inputs(&r)).is_err());
}

#[test]
fn sampling_projection_failures_are_null_for_only_the_affected_raster() {
    let _ = terrio::warnings::take();
    let mut projected = raster(1, 1, 1, &[42.0], None);
    let ortho = terrio::crs::SpatialRef::parse("+proj=ortho +lat_0=0 +lon_0=0 +datum=WGS84 +units=m").unwrap();
    projected.raster.crs = ortho.to_crs_info();
    projected.set_geo_transform([-1.0, 2.0, 0.0, 1.0, 0.0, -2.0]);
    let mut geographic = raster(1, 1, 1, &[7.0], None);
    geographic.raster.crs.epsg = Some(4326);
    geographic.set_geo_transform([179.0, 2.0, 0.0, 1.0, 0.0, -2.0]);
    let l = layer(vec![Some(G::point(0.0, 0.0)), Some(G::point(180.0, 0.0)), Some(G::point(0.0, 0.0))]);
    let t = terrio::crs::Transformer::new(&terrio::crs::SpatialRef::from_epsg(4326).unwrap(), &ortho).unwrap();
    assert!(t.transform(180.0, 0.0).is_err(), "the far hemisphere is outside this projection");
    let inputs = [
        SampleInput { field_name: "projected".into(), raster: &projected, band: 1 },
        SampleInput { field_name: "geographic".into(), raster: &geographic, band: 1 },
    ];
    let out = sample(&l, &inputs).unwrap();
    let warnings = terrio::warnings::take();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("field 'projected'") && warnings[0].contains("1 point into"));
    assert_eq!(out.features[0].attributes[1..], [V::Float(42.0), V::Null]);
    assert_eq!(out.features[1].attributes[1..], [V::Null, V::Float(7.0)]);
    assert_eq!(out.features[2].attributes[1..], [V::Float(42.0), V::Null]);
    for (a, b) in out.features.iter().zip(&l.features) {
        assert_eq!(a.fid, b.fid);
        assert_eq!(a.geometry, b.geometry);
        assert_eq!(a.attributes[0], b.attributes[0]);
    }
    let all_failed = layer(vec![Some(G::point(180.0, 0.0)), Some(G::point(180.0, 0.0))]);
    let out = sample(&all_failed, &sample_inputs(&projected)).unwrap();
    assert!(out.features.iter().all(|f| f.attributes[1] == V::Null));
    let warnings = terrio::warnings::take();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("2 points into"));
    projected.raster.crs = Default::default();
    projected.raster.crs.epsg = Some(999_999);
    assert!(sample(&l, &sample_inputs(&projected)).is_err(), "CRS setup errors still fail the operation");
}

fn area(layer: &Layer) -> f64 {
    let ring_area = |r: &Ring| r.0.windows(2).map(|w| w[0].x * w[1].y - w[1].x * w[0].y).sum::<f64>().abs() / 2.0;
    layer
        .features
        .iter()
        .map(|f| match &f.geometry {
            Some(G::Polygon { exterior, interiors }) => ring_area(exterior) - interiors.iter().map(ring_area).sum::<f64>(),
            Some(G::MultiPolygon(ps)) => ps.iter().map(|(e, hs)| ring_area(e) - hs.iter().map(ring_area).sum::<f64>()).sum::<f64>(),
            _ => panic!(),
        })
        .sum()
}

#[test]
fn footprints_preserve_holes_and_band_union_intersection() {
    let r = raster(3, 3, 1, &[1.0, 1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0], Some(0.0));
    let out = footprint(&r, &Default::default()).unwrap();
    assert_eq!(area(&out), 8.0);
    let Some(G::MultiPolygon(ps)) = &out.features[0].geometry else { panic!() };
    assert_eq!(ps[0].1.len(), 1);
    let mut south_up = r.clone();
    south_up.set_geo_transform([100.0, 10.0, 2.0, 200.0, 1.0, 10.0]);
    let reflected = footprint(&south_up, &Default::default()).unwrap();
    assert_eq!(area(&reflected), 8.0 * 98.0);
    let Some(G::MultiPolygon(ps)) = &reflected.features[0].geometry else { panic!() };
    assert_eq!(ps[0].1.len(), 1);
    let r = raster(2, 2, 2, &[1.0, 0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 2.0], Some(0.0));
    assert_eq!(area(&footprint(&r, &Default::default()).unwrap()), 3.0);
    assert_eq!(area(&footprint(&r, &FootprintOptions { intersection: true, ..Default::default() }).unwrap()), 1.0);
    assert_eq!(area(&footprint(&r, &FootprintOptions { bands: vec![1], ..Default::default() }).unwrap()), 2.0);
    let empty = raster(1, 1, 1, &[0.0], Some(0.0));
    assert!(footprint(&empty, &Default::default()).unwrap().features.is_empty());
}

#[test]
fn footprint_regions_touching_only_at_a_corner_are_separate_polygons() {
    // GDAL 3.12.2 returns this valid two-member MultiPolygon, with shared corner (1, 1).
    let r = raster(2, 2, 1, &[1.0, 0.0, 0.0, 1.0], Some(0.0));
    let out = footprint(&r, &Default::default()).unwrap();
    let Some(G::MultiPolygon(parts)) = &out.features[0].geometry else { panic!() };
    assert_eq!(parts.len(), 2);
    assert_eq!(area(&out), 2.0);
    for (exterior, holes) in parts {
        assert!(holes.is_empty());
        assert_eq!(exterior.0.len(), 5);
        assert!(exterior.0.iter().any(|c| (c.x, c.y) == (1.0, 1.0)));
    }
}

#[test]
fn footprints_use_exact_affine_edges_and_explicit_masks() {
    let mut r = raster(2, 2, 1, &[1.0, 1.0, 1.0, 1.0], None);
    r.set_geo_transform([100.0, 10.0, 2.0, 200.0, 1.0, 10.0]);
    r.raster.crs.epsg = Some(3857);
    let mut m = raster(2, 2, 1, &[0.0, 0.0, 0.0, 1.0], None);
    m.set_geo_transform(r.geo_transform());
    m.raster.crs = r.raster.crs.clone();
    let options = FootprintOptions { mask: Some(m), ..Default::default() };
    let out = footprint(&r, &options).unwrap();
    assert_eq!(area(&out), 98.0);
    assert_eq!(out.crs_epsg(), Some(3857));
    let bbox = out.features[0].geometry.as_ref().unwrap().bbox().unwrap();
    assert_eq!((bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y), (112.0, 211.0, 124.0, 222.0));
    let bad_mask = FootprintOptions { mask: Some(raster(2, 2, 1, &[1.0; 4], None)), ..Default::default() };
    assert!(footprint(&r, &bad_mask).is_err());
    assert!(footprint(&r, &FootprintOptions { bands: vec![0], ..Default::default() }).is_err());
    let mut tiny = raster(1, 1, 1, &[1.0], None);
    tiny.set_geo_transform([1e9, 1.0, 0.0, 1e9, 0.0, -1.0]);
    assert_eq!(footprint(&tiny, &Default::default()).unwrap().len(), 1);
}

#[test]
fn real_world_values_from_gdal() {
    // Expected values from GDAL 3.12.2 in the geo conda environment. The fixtures are fetched
    // by scripts/fetch_test_data.sh; the full independent comparison is in
    // scripts/compare_processing_with_gdal.py.
    let Some(dir) = std::env::var_os("TERRIO_TEST_DATA") else { return };
    let dir = std::path::PathBuf::from(dir);
    let l = terrio::open_vector(dir.join("countries.geojson").to_str().unwrap(), &Default::default()).unwrap().layers.remove(0);
    let centres = centroids(&l).unwrap();
    assert_eq!(centres.len(), 176);
    let (x, y) = xy(&centres.features[0].geometry);
    assert!((x - 66.08669022192834).abs() < 1e-10 && (y - 33.85639928169076).abs() < 1e-10);
    assert_eq!(singleparts(&l).len(), 280);
    assert_eq!(vertices(&l).len(), 9993);
    let dem = terrio::open_raster(dir.join("dem.tif").to_str().unwrap()).unwrap();
    let gt = dem.geo_transform();
    let mut sites = layer(vec![Some(G::point(gt[0] + 100.5 * gt[1], gt[3] + 100.5 * gt[5]))]);
    dem.crs().apply_to_layer(&mut sites);
    let sampled = sample(&sites, &sample_inputs(&dem)).unwrap();
    assert_eq!(sampled.features[0].attributes[1], V::Float(69.76741790771484));
}

#[test]
fn footprint_singleparts_keep_holes_attributes_and_empty_schema() {
    let mut r = raster(5, 3, 1, &[1., 1., 1., 0., 1., 1., 0., 1., 0., 0., 1., 1., 1., 0., 0.], Some(0.));
    r.raster.crs.epsg = Some(4326);
    let options = FootprintOptions {
        singlepart: true,
        attributes: vec![("location".into(), "scene.tif".into()), ("label".into(), "a=b".into())],
        ..Default::default()
    };
    let out = footprint(&r, &options).unwrap();
    assert_eq!(out.geom_type, Some(terrio::GeometryType::Polygon));
    assert_eq!(out.len(), 2);
    assert_eq!(area(&out), 9.);
    assert_eq!(out.crs_epsg(), Some(4326));
    let mut holes = 0;
    for (i, feature) in out.features.iter().enumerate() {
        assert_eq!(feature.fid, i as u64 + 1);
        assert_eq!(feature.attributes, vec![V::Text("scene.tif".into()), V::Text("a=b".into())]);
        let Some(G::Polygon { interiors, .. }) = &feature.geometry else { panic!() };
        holes += interiors.len();
    }
    assert_eq!(holes, 1);
    let empty = footprint(&raster(1, 1, 1, &[0.], Some(0.)), &options).unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.geom_type, out.geom_type);
    assert_eq!(empty.schema.len(), 2);
    assert_eq!(empty.schema.fields()[0].name, "location");
    assert_eq!(empty.schema.fields()[1].name, "label");
    for names in [vec![""], vec![" "], vec!["name "], vec![" name"], vec!["name", "NAME"]] {
        let options = FootprintOptions { attributes: names.into_iter().map(|n| (n.into(), "v".into())).collect(), ..Default::default() };
        assert!(footprint(&r, &options).is_err());
    }
}

#[test]
fn sampling_output_crs_and_rounding_preserve_values_attributes_and_nulls() {
    let mut r = raster(3, 1, 1, &[1.125, -1.125, f64::MAX / 100.], None);
    r.raster.crs.epsg = Some(4326);
    let mut points = layer(vec![
        Some(G::Point(Coord { x: 0.5, y: 0.5, z: Some(7.), m: Some(8.) })),
        Some(G::point(1.5, 0.5)),
        Some(G::point(2.5, 0.5)),
        Some(G::point(10., 0.5)),
        None,
    ]);
    points.schema.add_field(FieldDef::new("source_float", FieldType::Float));
    for feature in &mut points.features {
        feature.attributes.push(V::Float(9.87654321));
    }
    let inputs = [SampleInput { field_name: "value".into(), raster: &r, band: 1 }];
    let options = SampleOptions { dst_srs: Some(terrio::crs::SpatialRef::from_epsg(3857).unwrap()), decimal_places: Some(2) };
    let out = sample_with_options(&points, &inputs, &options).unwrap();
    assert_eq!(out.crs_epsg(), Some(3857));
    for (i, expected) in [V::Float(1.13), V::Float(-1.13), V::Float(f64::MAX / 100.), V::Null, V::Null].iter().enumerate() {
        assert_eq!(&out.features[i].attributes[2], expected);
        assert_eq!(out.features[i].attributes[..2], points.features[i].attributes);
        assert_eq!(out.features[i].fid, points.features[i].fid);
    }
    let Some(G::Point(c)) = &out.features[0].geometry else { panic!() };
    assert!((c.x - 55_659.74539663678).abs() < 1e-6);
    assert!((c.y - 55_660.45186542152).abs() < 1e-6);
    assert_eq!((c.z, c.m), (Some(7.), Some(8.)));
    assert_eq!(sample(&points, &inputs).unwrap().features[0].attributes[2], V::Float(1.125));
    assert!(sample_with_options(&points, &inputs, &SampleOptions { decimal_places: Some(16), ..Default::default() }).is_err());
    points.set_crs_epsg(None);
    r.raster.crs = Default::default();
    let inputs = [SampleInput { field_name: "value".into(), raster: &r, band: 1 }];
    assert!(sample_with_options(&points, &inputs, &options).is_err());
}

#[test]
fn sampling_rounding_respects_precision_limits_and_zero_decimal_places() {
    let points = layer(vec![Some(G::point(0.5, 0.5))]);
    for (value, places, expected) in [(736_754.262_358_918, 15, 736_754.262_358_918), (1.5, 0, 2.), (-1.5, 0, -2.), (f64::MAX, 2, f64::MAX)]
    {
        let mut r = raster(1, 1, 1, &[value], None);
        r.raster.crs.epsg = Some(4326);
        let inputs = [SampleInput { field_name: "value".into(), raster: &r, band: 1 }];
        let options = SampleOptions { decimal_places: Some(places), ..Default::default() };
        let out = sample_with_options(&points, &inputs, &options).unwrap();
        assert_eq!(out.features[0].attributes[1], V::Float(expected));
    }
}
