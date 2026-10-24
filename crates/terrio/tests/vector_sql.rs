use terrio::ops::sql::query;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue as V, Geometry, Layer};

fn data() -> Vec<Layer> {
    let mut a = Layer::new("cities");
    a.crs = Some(wbvector::Crs::new().with_epsg(4326));
    for (n, t) in [("name", FieldType::Text), ("key", FieldType::Integer), ("pop", FieldType::Integer)] {
        a.schema.add_field(FieldDef::new(n, t));
    }
    for (i, (name, key, pop)) in
        [("A", V::Integer(1), V::Integer(5)), ("a", V::Integer(1), V::Integer(9)), ("B", V::Integer(2), V::Null), ("C", V::Null, V::Null)]
            .into_iter()
            .enumerate()
    {
        a.features.push(Feature {
            fid: i as u64 + 10,
            geometry: Some(Geometry::Point(Coord::xy(i as f64, 0.))),
            attributes: vec![V::Text(name.into()), key, pop],
        });
    }
    let mut b = Layer::new("nations");
    b.schema.add_field(FieldDef::new("id", FieldType::Integer));
    b.schema.add_field(FieldDef::new("name", FieldType::Text));
    for (id, name) in [(V::Integer(1), "first"), (V::Integer(1), "duplicate"), (V::Null, "null key")] {
        b.features.push(Feature { fid: 0, geometry: None, attributes: vec![id, V::Text(name.into())] });
    }
    vec![a, b]
}
#[test]
fn expressions_keep_geometry_types_and_nulls() {
    let layers = data();
    let out = query(
        &layers,
        "SELECT c.name, c.pop*2 AS doubled, pop/2 AS half, pop>6 AS large, 1.0+pop AS real FROM cities c ORDER BY pop DESC LIMIT 3",
    )
    .unwrap();
    assert_eq!(out.features.len(), 3);
    assert_eq!(out.features[0].fid, 11);
    assert_eq!(out.features[0].attributes, vec![V::Text("a".into()), V::Integer(18), V::Integer(4), V::Integer(1), V::Float(10.)]);
    assert_eq!(out.features[2].attributes[1], V::Null);
    assert!(out.features[0].geometry.is_some());
    assert_eq!(out.crs_epsg(), layers[0].crs_epsg());
    assert_eq!(out.schema.fields()[4].field_type, FieldType::Float);
    let empty = query(&layers, "SELECT pop*2 AS doubled FROM cities WHERE pop<0").unwrap();
    assert_eq!(empty.schema.fields()[0].field_type, FieldType::Integer);
    assert!(query(&layers, "SELECT name*2 FROM cities").is_err());
}
#[test]
fn distinct_and_summaries_discard_geometry() {
    let layers = data();
    let out = query(&layers, "SELECT DISTINCT key FROM cities ORDER BY key DESC LIMIT 2").unwrap();
    assert_eq!(out.features.iter().map(|f| f.attributes[0].clone()).collect::<Vec<_>>(), vec![V::Integer(2), V::Integer(1)]);
    assert!(out.features.iter().all(|f| f.geometry.is_none()));
    assert!(out.crs.is_none());
    let out = query(&layers, "SELECT COUNT(*) AS n, COUNT(pop), COUNT(DISTINCT key), SUM(pop), AVG(pop), MIN(name), MAX(pop) FROM cities")
        .unwrap();
    assert_eq!(
        out.features[0].attributes,
        vec![V::Integer(4), V::Integer(2), V::Integer(2), V::Integer(14), V::Float(7.), V::Text("A".into()), V::Integer(9)]
    );
    let empty = query(&layers, "SELECT COUNT(*),SUM(pop),AVG(pop),MIN(pop) FROM cities WHERE pop<0").unwrap();
    assert_eq!(empty.features[0].attributes, vec![V::Integer(0), V::Null, V::Null, V::Null]);
    for q in [
        "SELECT COUNT(*),name FROM cities",
        "SELECT SUM(name) FROM cities",
        "SELECT SUM(pop+1) FROM cities",
        "SELECT DISTINCT name,key FROM cities",
        "SELECT COUNT(*) FROM cities GROUP BY key",
    ] {
        assert!(query(&layers, q).is_err(), "{q}");
    }
}
#[test]
fn ogr_join_uses_first_match_and_null_fills_missing_and_null_keys() {
    let layers = data();
    let out = query(
        &layers,
        "SELECT c.*, n.name AS nation, n.id+pop AS adjusted FROM cities AS c LEFT JOIN nations AS n ON c.key=n.id ORDER BY pop DESC",
    )
    .unwrap();
    assert_eq!(out.features.len(), 4);
    assert_eq!(out.features[0].attributes[3], V::Text("first".into()));
    assert_eq!(out.features[0].attributes[4], V::Integer(10));
    assert!(out.features[2..].iter().all(|f| f.attributes[3] == V::Null));
    assert!(out.features.iter().all(|f| f.geometry.is_some()));
    assert!(query(&layers, "SELECT n.name FROM cities c LEFT JOIN nations n ON c.key=n.id WHERE n.id=1").is_err());
    assert!(query(&layers, "SELECT n.name FROM cities c LEFT JOIN nations n ON c.key=n.id ORDER BY n.id").is_err());
}
#[test]
fn safe_integer_edges() {
    let out = query(
        &data(),
        "SELECT (-9223372036854775807-1)/-1 AS a, (-9223372036854775807-1)%-1 AS b, -(-9223372036854775807-1) AS c FROM cities LIMIT 1",
    )
    .unwrap();
    assert_eq!(out.features[0].attributes, vec![V::Null, V::Null, V::Null]);
}

#[test]
fn qualified_fid_and_independent_joins() {
    let out = query(&data(), "SELECT c.FID+1 AS id, a.name AS first, b.name AS second FROM cities c LEFT JOIN nations a ON c.key=a.id LEFT JOIN nations b ON c.key=b.id LIMIT 1").unwrap();
    assert_eq!(out.features[0].attributes, vec![V::Integer(11), V::Text("first".into()), V::Text("first".into())]);
    assert!(query(&data(), "SELECT b.name FROM cities c LEFT JOIN nations a ON c.key=a.id LEFT JOIN nations b ON a.id=b.id").is_err());
}

#[test]
fn binary_values_are_not_confused_with_null() {
    let mut l = Layer::new("binary");
    l.schema.add_field(FieldDef::new("payload", FieldType::Blob));
    for value in [V::Blob(vec![1]), V::Blob(vec![2]), V::Blob(vec![1]), V::Null] {
        l.features.push(Feature { fid: 0, geometry: None, attributes: vec![value] });
    }
    let out = query(std::slice::from_ref(&l), "SELECT DISTINCT payload FROM binary").unwrap();
    assert_eq!(out.features.len(), 3);
    let out = query(&[l], "SELECT COUNT(payload) AS n,COUNT(DISTINCT payload) AS kinds FROM binary").unwrap();
    assert_eq!(out.features[0].attributes, vec![V::Integer(3), V::Integer(2)]);
}

#[test]
fn null_literal_has_ogr_string_type() {
    let out = query(&data(), "SELECT NULL AS missing FROM cities LIMIT 1").unwrap();
    assert_eq!(out.schema.fields()[0].field_type, FieldType::Text);
    assert_eq!(out.features[0].attributes, vec![V::Null]);
    assert!(query(&data(), "SELECT 1+NULL FROM cities").is_err());
}
