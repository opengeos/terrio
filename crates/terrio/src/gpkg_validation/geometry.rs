//! Bounded, allocation-free inspection of GeoPackage binary geometries.
//! GeoPackage 1.4 requirements 19, 32, 33, 68 and 152; GDAL rule attribution in NOTICE.
use super::*;
use std::collections::BTreeSet;

const TYPES: &[&str] = &[
    "GEOMETRY",
    "POINT",
    "LINESTRING",
    "POLYGON",
    "MULTIPOINT",
    "MULTILINESTRING",
    "MULTIPOLYGON",
    "GEOMETRYCOLLECTION",
    "CIRCULARSTRING",
    "COMPOUNDCURVE",
    "CURVEPOLYGON",
    "MULTICURVE",
    "MULTISURFACE",
];

struct Wkb<'a> {
    bytes: &'a [u8],
    position: usize,
}
#[derive(Debug)]
struct Shape {
    kind: u32,
    dimension: u32,
    empty: bool,
}
impl Wkb<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.position.checked_add(N)?;
        let value = self.bytes.get(self.position..end)?.try_into().ok()?;
        self.position = end;
        Some(value)
    }
    fn integer(&mut self, little: bool) -> Option<u32> {
        let bytes = self.take()?;
        Some(if little { u32::from_le_bytes(bytes) } else { u32::from_be_bytes(bytes) })
    }
    fn number(&mut self, little: bool) -> Option<f64> {
        let bytes = self.take()?;
        Some(if little { f64::from_le_bytes(bytes) } else { f64::from_be_bytes(bytes) })
    }
    fn coordinates(&mut self, count: usize, dimensions: usize) -> Option<()> {
        let end = self.position.checked_add(count.checked_mul(dimensions)?.checked_mul(8)?)?;
        self.bytes.get(self.position..end)?;
        self.position = end;
        Some(())
    }
    fn shape(&mut self, depth: usize) -> Option<Shape> {
        if depth > 64 {
            return None;
        }
        let little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let code = self.integer(little)?;
        let kind = code % 1000;
        let dimension = code / 1000;
        if dimension > 3 || !(1..=12).contains(&kind) {
            return None;
        }
        let dimensions = 2 + usize::from(dimension == 1 || dimension == 3) + usize::from(dimension >= 2);
        let empty = if kind == 1 {
            let x = self.number(little)?;
            let y = self.number(little)?;
            self.coordinates(1, dimensions - 2)?;
            x.is_nan() && y.is_nan()
        } else {
            let count = self.integer(little)? as usize;
            if kind == 2 || kind == 8 {
                if kind == 8 && count != 0 && (count < 3 || count.is_multiple_of(2)) {
                    return None;
                }
                self.coordinates(count, dimensions)?;
                count == 0
            } else {
                // Each ring or member needs at least a count/header. Refuse huge
                // counts before iteration, without allocating from untrusted sizes.
                if count > (self.bytes.len() - self.position) / 4 {
                    return None;
                }
                let mut empty = true;
                for _ in 0..count {
                    if kind == 3 {
                        let points = self.integer(little)? as usize;
                        self.coordinates(points, dimensions)?;
                        empty &= points == 0;
                    } else {
                        let member = self.shape(depth + 1)?;
                        let compatible = match kind {
                            4 => member.kind == 1,
                            5 => member.kind == 2,
                            6 => member.kind == 3,
                            7 => true,
                            9 => matches!(member.kind, 2 | 8),
                            10 | 11 => matches!(member.kind, 2 | 8 | 9),
                            12 => matches!(member.kind, 3 | 10),
                            _ => false,
                        };
                        if !compatible {
                            return None;
                        }
                        empty &= member.empty;
                    }
                }
                empty
            }
        };
        Some(Shape { kind, dimension, empty })
    }
}

fn blob_shape(blob: &[u8]) -> Option<(Shape, i32, bool, u8)> {
    if blob.get(..3)? != b"GP\0" {
        return None;
    }
    let flags = *blob.get(3)?;
    if flags & 0xe0 != 0 {
        return None;
    }
    let envelope = (flags >> 1) & 7;
    let envelope_bytes = match envelope {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        _ => return None,
    };
    let srs = blob.get(4..8)?.try_into().ok()?;
    let srs = if flags & 1 != 0 { i32::from_le_bytes(srs) } else { i32::from_be_bytes(srs) };
    let bytes = blob.get(8 + envelope_bytes..)?;
    let mut wkb = Wkb { bytes, position: 0 };
    let shape = wkb.shape(0)?;
    if wkb.position != bytes.len() {
        return None;
    }
    Some((shape, srs, flags & 0x10 != 0, envelope))
}

struct Registration {
    name: String,
    field: String,
    kind: String,
    srs: i64,
    z: i64,
    m: i64,
}
impl Checker {
    pub(super) fn geometries(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(registry) = self.db.tables.get("gpkg_geometry_columns") else { return Ok(()) };
        let registrations: Vec<_> = registry
            .rows
            .iter()
            .filter_map(|row| {
                Some(Registration {
                    name: registry.get(row, "table_name")?.as_str()?.into(),
                    field: registry.get(row, "column_name")?.as_str()?.into(),
                    kind: registry.get(row, "geometry_type_name")?.as_str()?.into(),
                    srs: registry.get(row, "srs_id")?.as_i64()?,
                    z: registry.get(row, "z")?.as_i64()?,
                    m: registry.get(row, "m")?.as_i64()?,
                })
            })
            .collect();
        let encoding = u32::from_be_bytes(
            bytes.get(56..60).and_then(|v| v.try_into().ok()).ok_or_else(|| Error::InvalidData("truncated SQLite header".into()))?,
        );
        let mut srids = BTreeSet::new();
        let mut types = BTreeSet::new();
        sqlite_repack::inspect(bytes, |entry, rowid, record| {
            if entry.0 != "table" {
                return Ok(());
            }
            let Some(table) = self.db.tables.get(&entry.1) else { return Ok(()) };
            if !registrations.iter().any(|r| r.name.eq_ignore_ascii_case(&entry.1)) {
                return Ok(());
            }
            let row = table.decode(record, rowid, encoding)?;
            for r in registrations.iter().filter(|r| r.name.eq_ignore_ascii_case(&entry.1)) {
                let Some(value) = table.schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(&r.field)).and_then(|i| row.get(i))
                else {
                    continue;
                };
                if matches!(value, Value::Null) {
                    continue;
                }
                let shape = if let Value::Blob(blob) = value { blob_shape(blob) } else { None };
                let Some((shape, srs, empty, envelope)) = shape else {
                    self.report.require(false, 19, format!("{}.{} row {rowid:?}: invalid GeoPackage geometry", r.name, r.field));
                    continue;
                };
                if i64::from(srs) != r.srs && srids.insert((r.name.clone(), r.field.clone(), srs)) {
                    self.report.require(false, 33, format!("{}: geometry SRS {srs} differs from {}", r.name, r.srs));
                }
                self.report.require(
                    empty == shape.empty && (!empty || envelope == 0),
                    152,
                    format!("{}: inconsistent empty geometry flag/envelope", r.name),
                );
                let z = shape.dimension == 1 || shape.dimension == 3;
                let m = shape.dimension >= 2;
                for (valid, message) in [
                    (r.z != 1 || z, "mandatory Z absent"),
                    (r.m != 1 || m, "mandatory M absent"),
                    (!z || r.z > 0, "forbidden Z present"),
                    (!m || r.m > 0, "forbidden M present"),
                ] {
                    self.report.require(valid, 19, format!("{}: {message}", r.name));
                }
                types.insert((r.name.clone(), r.field.clone(), shape.kind));
            }
            Ok(())
        })?;
        for r in &registrations {
            let mut valid = true;
            for (_, _, kind) in types.iter().filter(|(name, field, _)| name == &r.name && field == &r.field) {
                let extended = *kind >= 8;
                let kind = TYPES[*kind as usize];
                valid &= match r.kind.as_str() {
                    "GEOMETRY" => true,
                    "GEOMETRYCOLLECTION" => matches!(
                        kind,
                        "GEOMETRYCOLLECTION" | "MULTIPOINT" | "MULTILINESTRING" | "MULTIPOLYGON" | "MULTICURVE" | "MULTISURFACE"
                    ),
                    "CURVEPOLYGON" | "SURFACE" => matches!(kind, "POLYGON" | "CURVEPOLYGON"),
                    "MULTICURVE" => matches!(kind, "MULTILINESTRING" | "MULTICURVE"),
                    "MULTISURFACE" => matches!(kind, "MULTIPOLYGON" | "MULTISURFACE"),
                    "CURVE" => matches!(kind, "LINESTRING" | "CIRCULARSTRING" | "COMPOUNDCURVE"),
                    expected => kind == expected,
                };
                if extended {
                    let extension = format!("gpkg_geom_{kind}");
                    let declared = self.db.tables.get("gpkg_extensions").is_some_and(|ext| {
                        ext.rows.iter().any(|row| {
                            ext.get(row, "table_name").and_then(Value::as_str) == Some(r.name.as_str())
                                && ext.get(row, "column_name").and_then(Value::as_str) == Some(r.field.as_str())
                                && ext.get(row, "extension_name").and_then(Value::as_str) == Some(extension.as_str())
                                && ext.get(row, "scope").and_then(Value::as_str) == Some("read-write")
                        })
                    });
                    self.report.require(declared, 68, format!("{}: missing extension for geometry content {kind}", r.name));
                }
            }
            self.report.require(valid, 32, format!("{}: geometry content disagrees with declared type {}", r.name, r.kind));
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    fn rejects_truncation_counts_reserved_flags_and_trailing_bytes() {
        let mut point = b"GP\0\x01".to_vec();
        point.extend(4326i32.to_le_bytes());
        point.push(1);
        point.extend(1u32.to_le_bytes());
        point.extend(1f64.to_le_bytes());
        point.extend(2f64.to_le_bytes());
        assert!(blob_shape(&point).is_some());
        for length in 0..point.len() {
            assert!(blob_shape(&point[..length]).is_none());
        }
        let mut extra = point.clone();
        extra.push(0);
        assert!(blob_shape(&extra).is_none());
        for flags in [0x21, 0x41, 0x81, 0x0b, 0x0d, 0x0f] {
            let mut invalid = point.clone();
            invalid[3] = flags;
            assert!(blob_shape(&invalid).is_none());
        }
        let mut invalid = point.clone();
        invalid[8] = 2;
        assert!(blob_shape(&invalid).is_none());
        let mut invalid = point.clone();
        invalid[9..13].copy_from_slice(&7u32.to_le_bytes());
        invalid[13..17].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(blob_shape(&invalid).is_none());
    }

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn geometry_rules_match_gdal_with_empty_flag_correction() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys,struct
from osgeo import gdal,ogr
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]); data=pathlib.Path(sys.argv[2]); base=root/'base.gpkg'
gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),format='GPKG',layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
cases=[]
def add(name,sql='',blob=None,kind='GEOMETRY',z=0,m=0,correct_empty=False):
 path=root/(str(len(cases))+'.gpkg'); shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('PRAGMA ignore_check_constraints=ON')
  db.execute('UPDATE gpkg_geometry_columns SET geometry_type_name=?,z=?,m=?',(kind,z,m))
  if blob is not None:
   db.execute('DELETE FROM countries')
   db.execute('INSERT INTO countries(geom) VALUES(?)',(blob,))
  if sql: db.executescript(sql)
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(1,4,0); checker.extended_pragma_info=True
 with sqlite3.connect(path) as db: checker._check_features(db.cursor())
 requirements=[e[0] for e in checker.errors if e[0] in (19,32,33,68,152)]
 # GDAL 3.13.3 reads bit 3 instead of GeoPackage's bit 4 for empty_flag.
 # Empty geometries generated by OGR have valid bit 4 and no envelope.
 if correct_empty:
  assert requirements.count(152)==1,(name,requirements)
  requirements.remove(152)
 cases.append(dict(name=name,path=str(path),requirements=sorted(requirements)))
add('Natural Earth multipolygons')
add('real geometry wrong type',kind='POINT')
add('geometry SRS mismatch','UPDATE gpkg_geometry_columns SET srs_id=-1')
add('mandatory Z absent',z=1)
add('mandatory M absent',m=1)
for wkt in ['POINT (1 2)','POINT Z (1 2 3)','POINT M (1 2 4)','POINT ZM (1 2 3 4)','LINESTRING (1 2,3 4)','POLYGON ((0 0,1 0,1 1,0 0))','MULTIPOINT ((1 2),(3 4))','GEOMETRYCOLLECTION (POINT (1 2),LINESTRING (1 2,3 4))','CIRCULARSTRING (0 0,1 1,2 0)','COMPOUNDCURVE ((0 0,1 1),CIRCULARSTRING (1 1,2 2,3 1))','CURVEPOLYGON (CIRCULARSTRING (0 0,1 1,2 0,1 -1,0 0))','MULTICURVE ((0 0,1 1))','MULTISURFACE (((0 0,1 0,1 1,0 0)))','POINT EMPTY','LINESTRING EMPTY','POLYGON EMPTY','GEOMETRYCOLLECTION EMPTY']:
 geom=ogr.CreateGeometryFromWkt(wkt)
 assert geom is not None,wkt
 for endian in [0,1]:
  # Independent OGR ISO WKB, no envelope, same byte order in GPKG and WKB.
  blob=b'GP'+bytes([0,endian|(16 if geom.IsEmpty() else 0)])+struct.pack('<i' if endian else '>i',4326)+bytes(geom.ExportToIsoWkb(endian))
  add(wkt+str(endian),blob=blob,z=2,m=2,correct_empty=geom.IsEmpty())
  if not geom.IsEmpty():
   # Forbidden Z/M are checked independently from decoding the payload.
   if geom.Is3D() or geom.IsMeasured(): add('forbidden dimensions '+wkt+str(endian),blob=blob)
with sqlite3.connect(base) as db: original=db.execute('SELECT geom FROM countries WHERE geom IS NOT NULL LIMIT 1').fetchone()[0]
for name,blob in [
 ('truncated header',original[:7]),
 ('header only',original[:8]),
 ('truncated payload',original[:-8]),
 ('bad magic',b'XX'+original[2:]),
 ('bad version',original[:2]+bytes([1])+original[3:]),
 ('extended binary flag',original[:3]+bytes([original[3]|32])+original[4:]),
 ('invalid envelope code',original[:3]+bytes([(original[3]&~14)|14])+original[4:]),
]:
 path=root/(str(len(cases))+'.gpkg'); shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('DELETE FROM countries'); db.execute('INSERT INTO countries(geom) VALUES(?)',(blob,))
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(1,4,0); checker.extended_pragma_info=True
 crashed=False
 try:
  with sqlite3.connect(path) as db: checker._check_features(db.cursor())
 except (IndexError,struct.error,RuntimeError): crashed=True
 assert crashed or any(e[0]==19 for e in checker.errors),(name,checker.errors)
 cases.append(dict(name=name,path=str(path),invalid=True))
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() >= 40);
        for case in cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let db = Snapshot::read(&bytes).unwrap();
            assert!(db.tables["countries"].rows.is_empty());
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.geometries(&bytes).unwrap();
            if case["invalid"].as_bool() == Some(true) {
                assert!(checker.report.errors.iter().any(|e| e.requirement == 19), "{case}: {:?}", checker.report);
                continue;
            }
            let mut actual: Vec<_> = checker.report.errors.iter().map(|e| e.requirement).collect();
            actual.sort_unstable();
            let expected: Vec<u32> = serde_json::from_value(case["requirements"].clone()).unwrap();
            assert_eq!(actual, expected, "{}: {:?}", case["name"], checker.report.errors);
        }
    }
}
