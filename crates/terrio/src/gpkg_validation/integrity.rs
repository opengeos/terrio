//! SQLite foreign-key integrity (GeoPackage requirement 7).
//! Parent affinities and collations follow SQLite's foreign-key comparison rules.
//! Only referenced keys are retained; unrelated columns and complete rows are not cached.
use super::*;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Atom {
    Null,
    Integer(i64),
    Real(u64),
    Text(Vec<u8>, usize),
    Blob(Vec<u8>),
}
#[derive(Clone, Copy, PartialEq)]
enum Affinity {
    Numeric,
    Real,
    Text,
    Blob,
}
fn affinity(kind: &str) -> Affinity {
    if kind.contains("INT") {
        Affinity::Numeric
    } else if ["CHAR", "CLOB", "TEXT"].iter().any(|s| kind.contains(s)) {
        Affinity::Text
    } else if kind.is_empty() || kind.contains("BLOB") {
        Affinity::Blob
    } else if ["REAL", "FLOA", "DOUB"].iter().any(|s| kind.contains(s)) {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}
pub(super) fn numeric(text: &str) -> Option<Value> {
    // SQLite's numeric conversion reads up to the first NUL and recognizes
    // all six ASCII whitespace characters (including vertical tab).
    let text = text.split('\0').next()?.trim_matches(|c: char| matches!(c, '\t'..='\r' | ' '));
    let bytes = text.as_bytes();
    let mut at = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
        digits += 1;
    }
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return None;
        }
    }
    if at != bytes.len() {
        return None;
    }
    if let Ok(integer) = text.parse::<i64>() {
        return Some(Value::Int(integer));
    }
    let real = text.parse::<f64>().ok()?;
    Some(metadata::integer(&Value::Real(real)).map_or(Value::Real(real), Value::Int))
}
// SQLite's scaled powers of ten and 64-bit decimal conversion. The extra
// 32 scale bits preserve SQLite's rounding at decimal boundaries (NOTICE).
fn power_of_ten(power: i32) -> u64 {
    const SCALE: [u64; 26] = [
        0x8049a4ac0c5811ae,
        0xcf42894a5dce35ea,
        0xa76c582338ed2621,
        0x873e4f75e2224e68,
        0xda7f5bf590966848,
        0xb080392cc4349dec,
        0x8e938662882af53e,
        0xe65829b3046b0afa,
        0xba121a4650e4ddeb,
        0x964e858c91ba2655,
        0xf2d56790ab41c2a2,
        0xc428d05aa4751e4c,
        0x9e74d1b791e07e48,
        0xcccccccccccccccc,
        0xcecb8f27f4200f3a,
        0xa70c3c40a64e6c51,
        0x86f0ac99b4e8dafd,
        0xda01ee641a708de9,
        0xb01ae745b101e9e4,
        0x8e41ade9fbebc27d,
        0xe5d3ef282a242e81,
        0xb9a74a0637ce2ee1,
        0x95f83d0a1fb69cd9,
        0xf24a01a73cf2dccf,
        0xc3b8358109e84f07,
        0x9e19db92b4e31ba9,
    ];
    const LOW: [u32; 26] = [
        0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b, 0x0cb4a5a3, 0x92f34d62, 0x3a6a07f9, 0xfae27299,
        0xaa97e14c, 0x775ea265, 0xcccccccc, 0x00000000, 0x999090b6, 0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9, 0x6d953e2c,
        0x4abdaf10, 0xbc633b39, 0x0a862f81, 0x6c07a2c2,
    ];
    let base = |n: u32| {
        let value = 10u128.pow(n);
        let bits = 128 - value.leading_zeros();
        if bits <= 64 { (value << (64 - bits)) as u64 } else { (value >> (bits - 64)) as u64 }
    };
    if (0..27).contains(&power) {
        return base(power as u32);
    }
    if power == -1 {
        return 0xcccccccccccccccc;
    }
    let group = power.div_euclid(27);
    let n = power.rem_euclid(27);
    let index = (group + 13) as usize;
    let scale = SCALE[index];
    if n == 0 {
        return scale;
    }
    let b = base(n as u32) as u128;
    let product = (scale as u128) * b + (((LOW[index] as u128) * b) >> 32);
    let mut high = (product >> 64) as u64;
    if high >> 63 == 0 {
        high = (high << 1) | ((product >> 63) as u64 & 1);
    }
    high
}
fn decimal_digits(value: f64) -> (Vec<u8>, i32) {
    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let mut mantissa = bits & 0x000f_ffff_ffff_ffff;
    let binary_exponent = if exponent == 0 {
        let zeros = mantissa.leading_zeros();
        mantissa <<= zeros;
        -1074 - zeros as i32
    } else {
        mantissa = (mantissa << 11) | (1u64 << 63);
        exponent - 1086
    };
    let power = 17 - (((binary_exponent + 63) * 78913) >> 18);
    let high = (((mantissa as u128) * (power_of_ten(power) as u128)) >> 64) as u64;
    let shift = -(binary_exponent + ((power * 108853) >> 15) + 2);
    let high = high >> shift;
    let decimal = high.wrapping_add((high << 1) & 2) >> 1;
    let digits = decimal.to_string().into_bytes();
    let point = digits.len() as i32 - power;
    (digits, point)
}

/// SQLite 3.54's %!.17g rendering, including its round-trip shortening.
/// The digit rounding/shortening follows sqlite3FpDecode (see NOTICE).
pub(super) fn real_text(real: f64) -> String {
    if real == 0.0 {
        return "0.0".into();
    }
    if real == f64::INFINITY {
        return "Inf".into();
    }
    if real == f64::NEG_INFINITY {
        return "-Inf".into();
    }
    if real.is_nan() {
        return "NaN".into();
    }
    let absolute = real.abs();
    let (mut digits, mut decimal_point) = decimal_digits(absolute);
    let mut precision = 17;
    if digits.len() > 17 {
        if digits[15] == b'9' && digits[14] == b'9' {
            let mut at = 14;
            while at > 0 && digits[at - 1] == b'9' {
                at -= 1;
            }
            let candidate = if at == 0 { 1 } else { std::str::from_utf8(&digits[..at]).unwrap().parse::<u64>().unwrap() + 1 };
            if format!("{candidate}e{}", decimal_point - at as i32).parse::<f64>().ok() == Some(absolute) {
                precision = at + 1;
            }
        } else if decimal_point >= digits.len() as i32 || digits[13..16] == *b"000" {
            let mut at = 13;
            while digits[at - 1] == b'0' {
                at -= 1;
            }
            let candidate = std::str::from_utf8(&digits[..at]).unwrap().parse::<u64>().unwrap();
            if format!("{candidate}e{}", decimal_point - at as i32).parse::<f64>().ok() == Some(absolute) {
                precision = at + 1;
            }
        }
        let round_up = digits[precision] >= b'5';
        digits.truncate(precision);
        if round_up {
            let mut at = digits.len();
            while at > 0 && digits[at - 1] == b'9' {
                digits[at - 1] = b'0';
                at -= 1;
            }
            if at == 0 {
                digits.insert(0, b'1');
                decimal_point += 1;
            } else {
                digits[at - 1] += 1;
            }
        }
    }
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }
    let mut out = if real.is_sign_negative() { "-".to_string() } else { String::new() };
    let exponent = decimal_point - 1;
    if !(-4..17).contains(&exponent) {
        out.push(digits[0] as char);
        out.push('.');
        if digits.len() == 1 {
            out.push('0');
        } else {
            out.push_str(std::str::from_utf8(&digits[1..]).unwrap());
        }
        out.push_str(&format!("e{exponent:+03}"));
    } else if decimal_point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-decimal_point) as usize));
        out.push_str(std::str::from_utf8(&digits).unwrap());
    } else {
        let point = decimal_point as usize;
        if point >= digits.len() {
            out.push_str(std::str::from_utf8(&digits).unwrap());
            out.extend(std::iter::repeat_n('0', point - digits.len()));
            out.push_str(".0");
        } else {
            out.push_str(std::str::from_utf8(&digits[..point]).unwrap());
            out.push('.');
            out.push_str(std::str::from_utf8(&digits[point..]).unwrap());
        }
    }
    out
}

pub(super) fn atom(value: &Value, column: &schema::Column, stored: bool) -> Option<Atom> {
    let a = affinity(&column.data_type);
    let value = if stored {
        if a == Affinity::Real
            && let Value::Int(value) = value
        {
            Value::Real(*value as f64)
        } else {
            value.clone()
        }
    } else {
        match (a, value) {
            (Affinity::Text, Value::Int(value)) => Value::Text(value.to_string()),
            (Affinity::Text, Value::Real(value)) if !value.is_nan() => Value::Text(real_text(*value)),
            (Affinity::Numeric | Affinity::Real, Value::Text(text)) => numeric(text).unwrap_or_else(|| value.clone()),
            _ => value.clone(),
        }
    };
    match value {
        Value::Null => None,
        Value::Int(value) => Some(Atom::Integer(value)),
        Value::Real(value) if value.is_nan() => None,
        Value::Real(value) => Some(metadata::integer(&Value::Real(value)).map_or_else(|| Atom::Real(value.to_bits()), Atom::Integer)),
        Value::Blob(bytes) => Some(Atom::Blob(bytes)),
        Value::Text(text) => {
            let mut bytes = text.into_bytes();
            let mut length = 0;
            match column.collation.as_str() {
                "NOCASE" => {
                    length = bytes.len();
                    bytes.truncate(bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len()));
                    bytes.make_ascii_lowercase();
                }
                "RTRIM" => {
                    while bytes.last() == Some(&b' ') {
                        bytes.pop();
                    }
                }
                _ => {}
            }
            Some(Atom::Text(bytes, length))
        }
    }
}
struct ForeignPlan {
    child: String,
    child_columns: Vec<usize>,
    parent: Option<String>,
    parent_columns: Vec<usize>,
    columns: Vec<schema::Column>,
    missing: BTreeSet<Vec<Atom>>,
    invalid: bool,
}
fn bad(message: &str) -> Error {
    Error::InvalidData(format!("foreign key: {message}"))
}
fn positions(schema: &Schema, names: &[String]) -> Result<Vec<usize>> {
    names
        .iter()
        .map(|name| {
            schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)).ok_or_else(|| bad("referenced column does not exist"))
        })
        .collect()
}
fn plan(db: &Snapshot, name: &str, foreign: &schema::ForeignKey) -> Result<ForeignPlan> {
    let child = &db.tables[name];
    let child_columns = positions(&child.schema, &foreign.columns)?;
    let mut result = ForeignPlan {
        child: name.into(),
        child_columns,
        parent: None,
        parent_columns: vec![],
        columns: vec![],
        missing: BTreeSet::new(),
        invalid: false,
    };
    let Some((parent_name, parent)) = db.tables.iter().find(|(key, _)| key.eq_ignore_ascii_case(&foreign.table)) else {
        if db.object(&foreign.table) {
            return Err(bad("parent is not a physical table"));
        }
        return Ok(result);
    };
    let parent_names = if foreign.parent_columns.is_empty() {
        let mut columns: Vec<_> = parent.schema.columns.iter().filter(|c| c.primary_key > 0).collect();
        columns.sort_by_key(|c| c.primary_key);
        columns.into_iter().map(|c| c.name.clone()).collect()
    } else {
        foreign.parent_columns.clone()
    };
    if parent_names.is_empty() || parent_names.len() != result.child_columns.len() {
        return Err(bad("parent/child key arity mismatch"));
    }
    let parent_columns = positions(&parent.schema, &parent_names)?;
    let mut unique = parent.schema.unique.clone();
    for entry in db.objects.iter().filter(|entry| entry.0 == "index" && entry.2.eq_ignore_ascii_case(parent_name) && !entry.4.is_empty()) {
        if let Some(key) = Schema::unique_index(&entry.4)? {
            unique.push(key);
        }
    }
    let eligible = parent_columns.len() == 1 && parent.schema.rowid_alias == Some(parent_columns[0])
        || unique.iter().any(|key| {
            let Ok(mut indices) = positions(&parent.schema, &key.columns) else { return false };
            if indices
                .iter()
                .zip(&key.collations)
                .any(|(i, collation)| collation.as_ref().is_some_and(|c| !c.eq_ignore_ascii_case(&parent.schema.columns[*i].collation)))
            {
                return false;
            }
            let mut expected = parent_columns.clone();
            indices.sort_unstable();
            expected.sort_unstable();
            indices == expected
        });
    if !eligible {
        return Err(bad("parent key is not PRIMARY KEY or a matching non-partial UNIQUE key"));
    }
    result.parent = Some(parent_name.clone());
    result.columns = parent_columns.iter().map(|i| parent.schema.columns[*i].clone()).collect();
    if result.columns.iter().any(|c| !matches!(c.collation.as_str(), "BINARY" | "NOCASE" | "RTRIM")) {
        return Err(Error::Unsupported("foreign-key collation is unavailable".into()));
    }
    result.parent_columns = parent_columns;
    Ok(result)
}
impl Checker {
    pub(super) fn foreign_keys(&mut self, bytes: &[u8]) -> Result<()> {
        let mut plans = Vec::new();
        let mut problems = Vec::new();
        for (name, table) in &self.db.tables {
            for foreign in &table.schema.foreign_keys {
                match plan(&self.db, name, foreign) {
                    Ok(plan) => plans.push(plan),
                    Err(error) => problems.push(format!("{name}: {error}")),
                }
            }
        }
        if !plans.is_empty() {
            let encoding =
                u32::from_be_bytes(bytes.get(56..60).and_then(|v| v.try_into().ok()).ok_or_else(|| bad("truncated SQLite header"))?);
            // First collect only non-NULL referenced keys. Any NULL component
            // satisfies a SQLite foreign key, including composite keys.
            sqlite_repack::inspect(bytes, |entry, rowid, record| {
                if entry.0 != "table" || !plans.iter().any(|p| p.child == entry.1) {
                    return Ok(());
                }
                let row = self.db.tables[&entry.1].decode(&entry.1, record, rowid, encoding)?;
                for p in plans.iter_mut().filter(|p| p.child == entry.1) {
                    if p.child_columns.iter().any(|i| matches!(row[*i], Value::Null) || matches!(row[*i],Value::Real(v) if v.is_nan())) {
                        continue;
                    }
                    if p.parent.is_none() {
                        p.invalid = true;
                        continue;
                    }
                    let key = p
                        .child_columns
                        .iter()
                        .zip(&p.columns)
                        .map(|(i, column)| {
                            // REAL-affinity source columns can use integer serial
                            // types on disk; SQLite exposes them as REAL on read.
                            let source = &self.db.tables[&entry.1].schema.columns[*i];
                            if affinity(&source.data_type) == Affinity::Real
                                && let Value::Int(value) = row[*i]
                            {
                                atom(&Value::Real(value as f64), column, false)
                            } else {
                                atom(&row[*i], column, false)
                            }
                        })
                        .collect::<Option<Vec<_>>>();
                    if let Some(key) = key {
                        p.missing.insert(key);
                    }
                }
                Ok(())
            })?;
            // Remove matches while streaming parent rows. Table order is
            // irrelevant, and parents without rowids use their decoded PK order.
            sqlite_repack::inspect(bytes, |entry, rowid, record| {
                if entry.0 != "table" || !plans.iter().any(|p| p.parent.as_deref() == Some(entry.1.as_str()) && !p.missing.is_empty()) {
                    return Ok(());
                }
                let row = self.db.tables[&entry.1].decode(&entry.1, record, rowid, encoding)?;
                for p in plans.iter_mut().filter(|p| p.parent.as_deref() == Some(entry.1.as_str()) && !p.missing.is_empty()) {
                    let key =
                        p.parent_columns.iter().zip(&p.columns).map(|(i, column)| atom(&row[*i], column, true)).collect::<Option<Vec<_>>>();
                    if let Some(key) = key {
                        p.missing.remove(&key);
                    }
                }
                Ok(())
            })?;
        }
        for p in plans {
            if p.invalid || !p.missing.is_empty() {
                problems.push(format!("{}: unresolved reference to {:?} ({} distinct keys)", p.child, p.parent, p.missing.len()));
            }
        }
        self.report.require(problems.is_empty(), 7, format!("foreign_key_check failed: {}", problems.join("; ")));
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires SQLite 3.54 Python bindings from the GDAL reference environment"]
    fn floating_affinity_text_matches_sqlite_across_exponents() {
        let script = r#"
import json,sqlite3,struct,random,math
randomizer=random.Random(289);values=[]
for exponent in range(-323,309):
 x=float('1e'+str(exponent))
 for candidate in [x,math.nextafter(x,0.0),math.nextafter(x,math.inf)]:
  if math.isfinite(candidate):values.extend([candidate,-candidate])
for _ in range(5000):
 x=struct.unpack('<d',struct.pack('<Q',randomizer.getrandbits(64)))[0]
 if math.isfinite(x):values.append(x)
with sqlite3.connect(':memory:') as db:
 print(json.dumps([[struct.unpack('<Q',struct.pack('<d',x))[0],db.execute('SELECT CAST(? AS TEXT)',(x,)).fetchone()[0]] for x in values]))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<(u64, String)> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 8000);
        for (bits, expected) in &cases {
            let value = f64::from_bits(*bits);
            assert_eq!(real_text(value), *expected, "bits {bits:016x}: {value:?}");
        }
        eprintln!("{} SQLite floating-affinity comparisons passed", cases.len());
    }

    #[test]
    fn foreign_key_rows_are_checked_without_gdal() {
        use crate::drivers::vector::sqlite::{self as writer, Table as OutputTable, Value as OutputValue};
        for (value, valid) in [
            (OutputValue::Null, true),
            (OutputValue::Text("+1".into()), true),
            (OutputValue::Int(2), false),
            (OutputValue::Blob(vec![1]), false),
        ] {
            let tables = [
                OutputTable {
                    name: "parent".into(),
                    sql: "CREATE TABLE parent(id INTEGER PRIMARY KEY)".into(),
                    rows: vec![(1, vec![OutputValue::Null])],
                    autoindexes: vec![],
                },
                OutputTable {
                    name: "child".into(),
                    sql: "CREATE TABLE child(value BLOB REFERENCES parent)".into(),
                    rows: vec![(1, vec![value])],
                    autoindexes: vec![],
                },
            ];
            let bytes = writer::write_database(&tables, &[], 10400, 0x4750_4b47).unwrap();
            let mut checker = Checker { db: Snapshot::read(&bytes).unwrap(), report: CoreReport::default() };
            checker.foreign_keys(&bytes).unwrap();
            assert_eq!(checker.report.is_valid(), valid, "{:?}", checker.report.errors);
        }
    }

    #[test]
    fn sqlite_real_text_uses_seventeen_digits_and_round_trip_shortening() {
        for (value, expected) in [
            (1e16, "10000000000000000.0"),
            (1e17, "1.0e+17"),
            (1e-5, "1.0e-05"),
            (1e-310, "9.9999999999999695e-311"),
            (1.2345678901234567, "1.2345678901234567"),
            (-0.0, "0.0"),
        ] {
            assert_eq!(real_text(value), expected);
        }
    }

    #[test]
    fn numeric_affinity_matches_sqlite_decimal_terminators() {
        for text in ["1e", "0x1", "NaN", "Inf", "1x", ""] {
            assert!(numeric(text).is_none(), "{text:?}");
        }
        for text in ["1", "+1", " 1 ", "1.0", "1e0", ".1e1", "1\0junk", "\x0b1\x0b"] {
            assert_eq!(numeric(text), Some(Value::Int(1)), "{text}");
        }
        assert_eq!(numeric("9007199254740993"), Some(Value::Int(9_007_199_254_740_993)));
        assert_eq!(numeric("1.5"), Some(Value::Real(1.5)));
    }

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn foreign_keys_match_sqlite_on_real_data_and_affinity_edges() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);base=root/'base.gpkg';cases=[]
gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
def add(name,sql,parameters=()):
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.executescript(sql)
  for statement,args in parameters:db.execute(statement,args)
  db.commit()
  try:valid=not db.execute('PRAGMA foreign_key_check').fetchall()
  except sqlite3.OperationalError:valid=False
 cases.append(dict(name=name,path=str(path),valid=valid))
add('real country references','CREATE TABLE child(id INTEGER PRIMARY KEY,country_id INTEGER REFERENCES countries(fid));INSERT INTO child(country_id) SELECT fid FROM countries')
add('missing country','CREATE TABLE child(id INTEGER PRIMARY KEY,country_id INTEGER REFERENCES countries(fid));INSERT INTO child(country_id) VALUES(-999)')
for rows in ['',"INSERT INTO child VALUES(NULL)","INSERT INTO child VALUES(1)"]:
 add('absent parent '+rows,'CREATE TABLE child(a REFERENCES missing(a));'+rows)
 add('nonunique parent '+rows,'CREATE TABLE p(a);CREATE TABLE child(a REFERENCES p(a));'+rows)
for storage in ['VIRTUAL','STORED']:
 for value in ['1','2','NULL']:
  add('generated child '+storage+' '+value,'CREATE TABLE p(a INTEGER PRIMARY KEY);CREATE TABLE child(seed INT,a INT AS(seed+1) '+storage+' REFERENCES p);INSERT INTO p VALUES(2);INSERT INTO child(seed) VALUES('+value+')')
  add('generated parent '+storage+' '+value,'CREATE TABLE p(seed INT,a INT AS(seed+1) '+storage+' UNIQUE);CREATE TABLE child(a REFERENCES p(a));INSERT INTO p(seed) VALUES(1);INSERT INTO child VALUES('+value+')')
add('generated parent text affinity and collation',"CREATE TABLE p(seed INT,a TEXT COLLATE NOCASE AS(seed||'x') UNIQUE);CREATE TABLE child(a REFERENCES p(a));INSERT INTO p(seed) VALUES(1);INSERT INTO child VALUES('1X')")
add('implicit primary key','CREATE TABLE p(a INTEGER PRIMARY KEY);CREATE TABLE child(x REFERENCES p);INSERT INTO p VALUES(-1);INSERT INTO child VALUES(-1)')
add('no implicit primary key','CREATE TABLE p(a UNIQUE);CREATE TABLE child(x REFERENCES p)')
add('composite key','CREATE TABLE p(a TEXT,b INTEGER,PRIMARY KEY(a,b));CREATE TABLE child(x,y,FOREIGN KEY(x,y) REFERENCES p);INSERT INTO p VALUES(\'one\',1);INSERT INTO child VALUES(\'one\',1)')
add('reordered composite key','CREATE TABLE p(a TEXT,b INTEGER,UNIQUE(a,b));CREATE TABLE child(x,y,FOREIGN KEY(x,y) REFERENCES p(b,a));INSERT INTO p VALUES(\'one\',1);INSERT INTO child VALUES(1,\'one\')')
add('WITHOUT ROWID composite','CREATE TABLE p(b INTEGER,a TEXT,PRIMARY KEY(a,b)) WITHOUT ROWID;CREATE TABLE child(z,x,y,PRIMARY KEY(z,x),FOREIGN KEY(x,y) REFERENCES p(a,b)) WITHOUT ROWID;INSERT INTO p VALUES(1,\'one\');INSERT INTO child VALUES(0,\'one\',1)')
for values in ['NULL,1',"'missing',NULL","'missing',1"]:
 add('composite nulls '+values,'CREATE TABLE p(a TEXT,b INTEGER,PRIMARY KEY(a,b));CREATE TABLE child(x,y,FOREIGN KEY(x,y) REFERENCES p);INSERT INTO child VALUES('+values+')')
for definition in ['CREATE UNIQUE INDEX idx ON p(a)','CREATE UNIQUE INDEX idx ON p((a))','CREATE UNIQUE INDEX idx ON p(a) WHERE a IS NOT NULL','CREATE UNIQUE INDEX idx ON p(a+0)','CREATE INDEX idx ON p(a)','CREATE UNIQUE INDEX idx ON p(a COLLATE NOCASE)']:
 add('parent index '+definition,'CREATE TABLE p(a TEXT);'+definition+';CREATE TABLE child(a REFERENCES p(a));INSERT INTO p VALUES(\'one\');INSERT INTO child VALUES(\'one\')')
add('collation mismatch in table UNIQUE','CREATE TABLE p(a TEXT COLLATE NOCASE,UNIQUE(a COLLATE BINARY));CREATE TABLE child(a REFERENCES p(a))')
add('case insensitive names','CREATE TABLE P(A INTEGER PRIMARY KEY);CREATE TABLE child(a REFERENCES p(a));INSERT INTO P VALUES(1);INSERT INTO child VALUES(1)')
add('rowid primary-key collation override','CREATE TABLE p(a INTEGER COLLATE NOCASE,PRIMARY KEY(a COLLATE BINARY));CREATE TABLE child(a REFERENCES p(a));INSERT INTO p VALUES(1);INSERT INTO child VALUES(1)')
for ptype in ['INTEGER','NUMERIC','REAL','TEXT','BLOB']:
 for value in [1,1.5,9007199254740993]:
  if ptype=='INTEGER' and value==1.5:continue
  add(ptype+' numeric '+str(value),'CREATE TABLE p(a '+ptype+' PRIMARY KEY);CREATE TABLE child(a BLOB REFERENCES p(a))',[('INSERT INTO p VALUES(?)',(value,)),('INSERT INTO child VALUES(?)',(value,))])
for value in ['1','01','1.0','1.5','+1',' 1 ','1e0','0x1','1\x00','1\x00junk','\x0b1\x0b','9007199254740993','9007199254740993.0']:
 add('integer affinity '+repr(value),'CREATE TABLE p(a INTEGER PRIMARY KEY);CREATE TABLE child(a BLOB REFERENCES p(a));INSERT INTO p VALUES(1),(9007199254740993)',[('INSERT INTO child VALUES(?)',(value,))])
for value in [1.,1.5,1e-5,1e-4,1e14,1e15,1e16,1e20,1.2345678901234567,-0.0,1e-310,float('inf')]:
 add('real-to-text affinity '+repr(value),'CREATE TABLE p(a TEXT PRIMARY KEY);CREATE TABLE child(a BLOB REFERENCES p(a))',[('INSERT INTO p VALUES(?)',(value,)),('INSERT INTO child VALUES(?)',(value,))])
for collation in ['BINARY','NOCASE','RTRIM']:
 for a,b in [('Ab','ab'),('Ab ','Ab'),('ä','Ä'),('Ab\x00x','ab\x00y'),('Ab\x00x','ab\x00yy')]:
  add(collation+' '+repr(a)+' '+repr(b),'CREATE TABLE p(a TEXT COLLATE '+collation+' PRIMARY KEY);CREATE TABLE child(a BLOB REFERENCES p(a))',[('INSERT INTO p VALUES(?)',(a,)),('INSERT INTO child VALUES(?)',(b,))])
add('blob key','CREATE TABLE p(a BLOB PRIMARY KEY);CREATE TABLE child(a REFERENCES p(a));INSERT INTO p VALUES(X\'0100\');INSERT INTO child VALUES(X\'0100\')')
add('blob is not integer','CREATE TABLE p(a BLOB PRIMARY KEY);CREATE TABLE child(a REFERENCES p(a));INSERT INTO p VALUES(X\'01\');INSERT INTO child VALUES(1)')
add('self reference','CREATE TABLE p(a INTEGER PRIMARY KEY,b REFERENCES p);INSERT INTO p VALUES(1,2),(2,1)')
add('empty child with malformed parent key','CREATE TABLE p(a,b,UNIQUE(a,b));CREATE TABLE child(a REFERENCES p(a))')
for text in ['1','1.0']:
 add('REAL source exposed as real to TEXT parent '+text,'CREATE TABLE p(a TEXT PRIMARY KEY);CREATE TABLE child(a REAL REFERENCES p(a));INSERT INTO child VALUES(1)',[('INSERT INTO p VALUES(?)',(text,))])
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 70);
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let db = Snapshot::read(&bytes).unwrap();
            assert!(db.tables["countries"].rows.is_empty());
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.foreign_keys(&bytes).unwrap();
            assert!(checker.report.errors.iter().all(|e| e.requirement == 7));
            assert_eq!(checker.report.is_valid(), case["valid"].as_bool().unwrap(), "{}: {:?}", case["name"], checker.report.errors);
        }
        eprintln!("{} SQLite foreign-key comparison cases passed", cases.len());
    }
}
