//! SQLite required values, unique keys and index records (requirement 6).
//! CHECK and virtual generated columns use the SQLite scalar evaluator.
//! Expression keys and partial predicates use the same scalar evaluator.
use super::*;
use integrity::Atom;
use std::cmp::Ordering;

#[derive(Clone)]
struct Slot {
    column: Option<usize>,
    expression: Option<sqlite_expr::Expression>,
    metadata: schema::Column,
    descending: bool,
}
struct Index {
    name: String,
    table: String,
    slots: Vec<Slot>,
    key_len: usize,
    unique: bool,
    predicate: Option<sqlite_expr::Expression>,
    expected: BTreeMap<Vec<Atom>, usize>,
    previous: Option<Vec<Atom>>,
    bad: bool,
}
struct PrimaryOrder {
    slots: Vec<Slot>,
    previous: Option<Vec<Atom>>,
    bad: bool,
}
fn slots(table: &Table, key: &schema::UniqueKey) -> Result<Vec<Slot>> {
    key.columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let column = table
                .schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| Error::InvalidData("index column does not exist".into()))?;
            let mut metadata = table.schema.columns[column].clone();
            metadata.data_type = "BLOB".into(); // Records have already undergone storage affinity.
            if let Some(collation) = &key.collations[i] {
                metadata.collation = collation.clone();
            }
            if !matches!(metadata.collation.as_str(), "BINARY" | "NOCASE" | "RTRIM") {
                return Err(Error::Unsupported(format!("index collation {}", metadata.collation)));
            }
            Ok(Slot { column: Some(column), expression: None, metadata, descending: key.descending[i] })
        })
        .collect()
}
fn same(a: &[Slot], b: &[Slot]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.column == b.column && a.metadata.collation == b.metadata.collation)
}
fn index_atom(value: &Value, slot: &Slot, encoding: u32) -> Atom {
    if slot.metadata.collation == "BINARY"
        && matches!(encoding, 2 | 3)
        && let Value::Text(text) = value
    {
        let bytes = text.encode_utf16().flat_map(|unit| if encoding == 2 { unit.to_le_bytes() } else { unit.to_be_bytes() }).collect();
        return Atom::Text(bytes, 0);
    }
    integrity::atom(value, &slot.metadata, false).unwrap_or(Atom::Null)
}
fn row_key(context: &sqlite_expr::Context<'_>, table: &Table, slots: &[Slot]) -> Result<Vec<Atom>> {
    let row = context.row;
    let rowid = context.rowid;
    let encoding = context.encoding;
    slots
        .iter()
        .map(|slot| {
            Ok(if let Some(expression) = &slot.expression {
                index_atom(&expression.evaluate(context)?, slot, encoding)
            } else if let Some(i) = slot.column {
                let source = &table.schema.columns[i];
                let real = sqlite_expr::has_real_affinity(&source.data_type);
                if real && let Value::Int(value) = row[i] {
                    index_atom(&Value::Real(value as f64), slot, encoding)
                } else {
                    index_atom(&row[i], slot, encoding)
                }
            } else {
                index_atom(&rowid.map_or(Value::Null, Value::Int), slot, encoding)
            })
        })
        .collect()
}
pub(super) fn compare_atom(a: &Atom, b: &Atom) -> Ordering {
    match (a, b) {
        (Atom::Null, Atom::Null) => Ordering::Equal,
        (Atom::Null, _) => Ordering::Less,
        (_, Atom::Null) => Ordering::Greater,
        (Atom::Integer(a), Atom::Integer(b)) => a.cmp(b),
        (Atom::Real(a), Atom::Real(b)) => f64::from_bits(*a).partial_cmp(&f64::from_bits(*b)).expect("canonical non-NaN real"),
        (Atom::Integer(a), Atom::Real(b)) => schema_extension::compare(&Value::Int(*a), &Value::Real(f64::from_bits(*b))).unwrap(),
        (Atom::Real(a), Atom::Integer(b)) => schema_extension::compare(&Value::Real(f64::from_bits(*a)), &Value::Int(*b)).unwrap(),
        (Atom::Text(a, n), Atom::Text(b, m)) => a.cmp(b).then(n.cmp(m)),
        (Atom::Blob(a), Atom::Blob(b)) => a.cmp(b),
        (Atom::Blob(_), _) => Ordering::Greater,
        (_, Atom::Blob(_)) => Ordering::Less,
        (Atom::Text(..), _) => Ordering::Greater,
        (_, Atom::Text(..)) => Ordering::Less,
    }
}
fn compare(a: &[Atom], b: &[Atom], slots: &[Slot]) -> Ordering {
    for ((a, b), slot) in a.iter().zip(b).zip(slots) {
        let order = compare_atom(a, b);
        if order != Ordering::Equal {
            return if slot.descending { order.reverse() } else { order };
        }
    }
    a.len().cmp(&b.len())
}
fn primary(table: &Table) -> Result<Vec<Slot>> {
    let Some(key) = table.schema.unique.iter().find(|key| key.primary) else { return Ok(Vec::new()) };
    let primary = slots(table, key)?;
    // SQLite reuses an earlier UNIQUE with the same columns/collations,
    // including its direction, when selecting WITHOUT ROWID table storage.
    for key in &table.schema.unique {
        let candidate = slots(table, key)?;
        if same(&candidate, &primary) {
            return Ok(candidate);
        }
    }
    Ok(primary)
}
fn make_index(name: String, table_name: &str, table: &Table, mut key: Vec<Slot>, unique: bool, automatic: bool) -> Result<Index> {
    let key_len = key.len();
    if table.schema.without_rowid {
        for mut slot in primary(table)? {
            if key.iter().any(|old| old.column == slot.column && old.metadata.collation == slot.metadata.collation) {
                continue;
            }
            // CREATE TABLE's auxiliary PK columns are ascending. Explicit
            // CREATE INDEX preserves the declared PK direction instead.
            if automatic {
                slot.descending = false;
            }
            key.push(slot);
        }
    } else {
        let metadata = schema::Column {
            name: "rowid".into(),
            data_type: "BLOB".into(),
            not_null: true,
            primary_key: 0,
            default: None,
            collation: "BINARY".into(),
            not_null_declared: false,
            generated: None,
        };
        key.push(Slot { column: None, expression: None, metadata, descending: false });
    }
    Ok(Index {
        name,
        table: table_name.into(),
        slots: key,
        key_len,
        unique,
        predicate: None,
        expected: BTreeMap::new(),
        previous: None,
        bad: false,
    })
}
impl Checker {
    pub(super) fn constraints(&mut self, bytes: &[u8]) -> Result<()> {
        let mut indexes = Vec::new();
        let mut primary_order = BTreeMap::new();
        for (name, table) in &self.db.tables {
            if table.schema.virtual_module.is_some() {
                continue;
            }
            if table.schema.without_rowid {
                primary_order.insert(name.clone(), PrimaryOrder { slots: primary(table)?, previous: None, bad: false });
            }
            let mut automatic: Vec<(Vec<Slot>, bool)> = Vec::new();
            for key in &table.schema.unique {
                let fields = slots(table, key)?;
                if key.primary && fields.len() == 1 && fields[0].column == table.schema.rowid_alias {
                    continue;
                }
                if let Some(old) = automatic.iter_mut().find(|(old, _)| same(old, &fields)) {
                    if key.primary {
                        old.1 = true;
                    }
                } else {
                    automatic.push((fields, key.primary));
                }
            }
            for (i, (fields, is_primary)) in automatic.into_iter().enumerate() {
                if is_primary && table.schema.without_rowid {
                    continue;
                }
                let index_name = format!("sqlite_autoindex_{name}_{}", i + 1);
                let mut index = make_index(index_name.clone(), name, table, fields, true, true)?;
                index.bad = !self.db.objects.iter().any(|object| object.0 == "index" && object.1 == index_name);
                indexes.push(index);
            }
        }
        for object in self.db.objects.iter().filter(|entry| entry.0 == "index" && !entry.4.is_empty()) {
            let Some((name, table)) = self.db.tables.iter().find(|(name, _)| name.eq_ignore_ascii_case(&object.2)) else {
                self.report.require(false, 6, format!("{}: index has no owning table", object.1));
                continue;
            };
            let declaration = Schema::expression_index(&object.4)?;
            let mut fields = Vec::new();
            for sql in declaration.terms {
                let (expression, descending) = sqlite_expr::Expression::index_term(&sql)?;
                expression.dependencies(name, &table.schema, false)?;
                let (column, collation) = expression.index_column(&table.schema.columns);
                if !matches!(collation.as_str(), "BINARY" | "NOCASE" | "RTRIM") {
                    return Err(Error::Unsupported(format!("index collation {collation}")));
                }
                let metadata = schema::Column {
                    name: sql,
                    data_type: "BLOB".into(),
                    not_null: false,
                    primary_key: 0,
                    default: None,
                    collation,
                    not_null_declared: false,
                    generated: None,
                };
                fields.push(Slot { column, expression: column.is_none().then_some(expression), metadata, descending });
            }
            let mut index = make_index(object.1.clone(), name, table, fields, declaration.unique, false)?;
            if let Some(sql) = declaration.predicate {
                let predicate = sqlite_expr::Expression::parse(&sql)?;
                predicate.dependencies(name, &table.schema, false)?;
                index.predicate = Some(predicate);
            }
            indexes.push(index);
        }
        for object in self.db.objects.iter().filter(|entry| entry.0 == "index" && entry.4.is_empty()) {
            self.report.require(indexes.iter().any(|index| index.name == object.1), 6, format!("{}: orphan automatic index", object.1));
        }
        let encoding = u32::from_be_bytes(
            bytes.get(56..60).and_then(|b| b.try_into().ok()).ok_or_else(|| Error::InvalidData("truncated SQLite header".into()))?,
        );
        let mut invalid_values = std::collections::BTreeSet::new();
        sqlite_repack::inspect(bytes, |entry, rowid, record| {
            if entry.0 != "table" {
                return Ok(());
            }
            let table = self.db.table(&entry.1);
            let row = table.decode(&entry.1, record, rowid, encoding)?;
            if let Some(evaluator) = &table.evaluator {
                for (i, check) in evaluator.checks.iter().enumerate() {
                    if !check.check(&sqlite_expr::Context {
                        columns: &table.schema.columns,
                        row: &row,
                        rowid,
                        table: Some(&entry.1),
                        strict: table.schema.strict,
                        logical: false,
                        unicode_case: self.db.unicode_case,
                        encoding,
                    })? {
                        invalid_values.insert(format!("{}: CHECK constraint {} failed", entry.1, i + 1));
                    }
                }
            }
            for (i, column) in table.schema.columns.iter().enumerate() {
                let null = matches!(row[i], Value::Null) || matches!(row[i],Value::Real(v) if v.is_nan());
                let required = column.not_null_declared || (table.schema.without_rowid || table.schema.strict) && column.primary_key > 0;
                if null && required {
                    invalid_values.insert(format!("{}.{}: NULL violates NOT NULL", entry.1, column.name));
                }
                if !null && table.schema.strict {
                    let valid = match column.data_type.as_str() {
                        "INT" | "INTEGER" => matches!(row[i], Value::Int(_)),
                        "REAL" => matches!(row[i], Value::Int(_) | Value::Real(_)),
                        "TEXT" => matches!(row[i], Value::Text(_)),
                        "BLOB" => matches!(row[i], Value::Blob(_)),
                        "ANY" => true,
                        _ => false,
                    };
                    if !valid {
                        invalid_values.insert(format!("{}.{}: invalid storage class in STRICT table", entry.1, column.name));
                    }
                }
            }
            let context = sqlite_expr::Context {
                columns: &table.schema.columns,
                row: &row,
                rowid,
                table: Some(&entry.1),
                strict: table.schema.strict,
                logical: false,
                unicode_case: self.db.unicode_case,
                encoding,
            };
            for index in indexes.iter_mut().filter(|index| index.table == entry.1) {
                if let Some(predicate) = &index.predicate
                    && sqlite_expr::truth(&predicate.evaluate(&context)?) != Some(true)
                {
                    continue;
                }
                let key = row_key(&context, table, &index.slots)?;
                *index.expected.entry(key).or_default() += 1;
            }
            if let Some(PrimaryOrder { slots, previous, bad }) = primary_order.get_mut(&entry.1) {
                let key = row_key(&context, table, slots)?;
                if previous.as_ref().is_some_and(|old| compare(old, &key, slots) != Ordering::Less) {
                    *bad = true;
                }
                *previous = Some(key);
            }
            Ok(())
        })?;
        for index in &mut indexes {
            if !index.unique {
                continue;
            }
            let mut previous = None;
            for (key, count) in &index.expected {
                let prefix = &key[..index.key_len];
                if !prefix.contains(&Atom::Null) && (previous == Some(prefix) || *count > 1) {
                    index.bad = true;
                }
                previous = Some(prefix);
            }
        }
        sqlite_repack::inspect(bytes, |entry, _, record| {
            if entry.0 != "index" {
                return Ok(());
            }
            let Some(index) = indexes.iter_mut().find(|index| index.name == entry.1) else {
                return Ok(());
            };
            let values = sqlite_repack::decode_record(record, encoding)?;
            if values.len() != index.slots.len() {
                index.bad = true;
                return Ok(());
            }
            let key: Vec<_> = values.iter().zip(&index.slots).map(|(value, slot)| index_atom(value, slot, encoding)).collect();
            if index.previous.as_ref().is_some_and(|old| compare(old, &key, &index.slots) != Ordering::Less) {
                index.bad = true;
            }
            index.previous = Some(key.clone());
            if let Some(count) = index.expected.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    index.expected.remove(&key);
                }
            } else {
                index.bad = true;
            }
            Ok(())
        })?;
        for message in invalid_values {
            self.report.require(false, 6, message);
        }
        for (name, PrimaryOrder { bad, .. }) in primary_order {
            self.report.require(!bad, 6, format!("{name}: unordered or duplicate WITHOUT ROWID primary keys"));
        }
        for index in indexes {
            self.report.require(
                !index.bad && index.expected.is_empty(),
                6,
                format!("{}: index records, ordering or uniqueness disagree with table rows", index.name),
            );
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    fn nullability_keeps_nullable_legacy_primary_keys_separate() {
        let legacy = Schema::parse("CREATE TABLE t(id INTEGER PRIMARY KEY DESC,v TEXT)").unwrap();
        assert_eq!(legacy.rowid_alias, None);
        assert!(!legacy.columns[0].not_null_declared);
        let strict = Schema::parse("CREATE TABLE t(id TEXT PRIMARY KEY,v TEXT) STRICT").unwrap();
        assert!(strict.strict && strict.columns[0].primary_key == 1);
        let required = Schema::parse("CREATE TABLE t(id INTEGER PRIMARY KEY,v TEXT NOT NULL)").unwrap();
        assert!(required.columns[1].not_null_declared);
    }

    #[test]
    fn generated_columns_preserve_physical_record_layout() {
        use crate::drivers::vector::sqlite::{self as writer, Table as OutputTable, Value as OutputValue};
        let sql = "CREATE TABLE t(a INTEGER,b INTEGER AS (a+1),c TEXT,d INTEGER GENERATED ALWAYS AS (a*2) STORED)";
        let schema = Schema::parse(sql).unwrap();
        assert!(!schema.columns[1].generated.as_ref().unwrap().stored);
        assert!(schema.columns[3].generated.as_ref().unwrap().stored);
        let tables = [OutputTable {
            name: "t".into(),
            sql: sql.into(),
            rows: vec![(1, vec![OutputValue::Int(3), OutputValue::Text("three".into()), OutputValue::Int(6)])],
            autoindexes: vec![],
        }];
        let bytes = writer::write_database(&tables, &[], 10400, 0x4750_4b47).unwrap();
        let snapshot = Snapshot::read(&bytes).unwrap();
        sqlite_repack::inspect(&bytes, |entry, rowid, record| {
            if entry.1 == "t" {
                let row = snapshot.tables["t"].decode_physical(record, rowid, 1)?;
                assert_eq!(row, [Value::Int(3), Value::Null, Value::Text("three".into()), Value::Int(6)]);
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn constraints_and_index_records_match_sqlite_integrity_check() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);base=root/'base.gpkg';cases=[]
source=gdal.OpenEx(str(data/'countries.gpkg'),gdal.OF_VECTOR);layer=source.GetLayer(0).GetName().replace('"','""')
gdal.VectorTranslate(str(base),source,layerName='countries',SQLStatement='SELECT * FROM "'+layer+'" LIMIT 3',SQLDialect='SQLITE',layerCreationOptions=['SPATIAL_INDEX=NO']).Close();source=None
with sqlite3.connect(base) as db:dump='\n'.join(db.iterdump())
for encoding in ['UTF-8','UTF-16le','UTF-16be']:
 template=root/(encoding+'.gpkg')
 with sqlite3.connect(template) as db:
  db.execute("PRAGMA encoding='"+encoding+"'");db.executescript(dump);db.execute('PRAGMA application_id=0x47504b47');db.execute('PRAGMA user_version=10400')
 def add(name,sql='',parameters=(),damage=None):
  path=root/(str(len(cases))+'.gpkg');shutil.copyfile(template,path)
  with sqlite3.connect(path) as db:
   if sql:db.executescript(sql)
   for statement,args in parameters:db.execute(statement,args)
   db.commit()
   if damage:
    rootpage=db.execute("SELECT rootpage FROM sqlite_master WHERE name='idx'").fetchone()[0]
  if damage:
   raw=bytearray(path.read_bytes());size=int.from_bytes(raw[16:18],'big') or 65536;at=(rootpage-1)*size
   assert raw[at]==10
   first=at+int.from_bytes(raw[at+8:at+10],'big');second=at+int.from_bytes(raw[at+10:at+12],'big')
   assert raw[first]<128 and raw[second]<128
   if damage=='key':
    header=raw[first+1];raw[first+1+header]=ord('z')
   else:
    assert raw[first]==raw[second];size=raw[first]+1;raw[first:first+size]=raw[second:second+size]
   path.write_bytes(raw)
  with sqlite3.connect(path) as db:
   try:valid=db.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
   except sqlite3.Error:valid=False
  cases.append(dict(name=encoding+' '+name,path=str(path),valid=valid))
 add('real country metadata indexes')
 add('plain index','CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT);INSERT INTO sample VALUES(10,\'a\'),(20,\'b\'),(30,\'c\');CREATE INDEX idx ON sample(v)')
 add('descending mixed index','CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT,w REAL);INSERT INTO sample VALUES(10,\'a\',1.5),(20,\'a\',-1),(30,\'c\',2);CREATE INDEX idx ON sample(v DESC,w ASC)')
 add('nullable UNIQUE','CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT,w INT,UNIQUE(v,w));INSERT INTO sample VALUES(1,NULL,1),(2,NULL,1)')
 add('nullable text PRIMARY KEY','CREATE TABLE sample(id TEXT PRIMARY KEY,v TEXT);INSERT INTO sample VALUES(NULL,\'a\'),(NULL,\'b\')')
 add('nullable descending INTEGER PRIMARY KEY','CREATE TABLE sample(id INTEGER PRIMARY KEY DESC,v TEXT);INSERT INTO sample VALUES(NULL,\'a\'),(NULL,\'b\')')
 add('redundant rowid UNIQUE','CREATE TABLE sample(id INTEGER PRIMARY KEY UNIQUE,v TEXT UNIQUE);INSERT INTO sample VALUES(1,\'a\'),(2,\'b\')')
 add('duplicate constraints share autoindex','CREATE TABLE sample(v TEXT,UNIQUE(v ASC),UNIQUE(v DESC));INSERT INTO sample VALUES(\'a\'),(\'b\')')
 add('WITHOUT ROWID secondary UNIQUE','CREATE TABLE sample(a TEXT,b INT,c TEXT,PRIMARY KEY(a DESC,b),UNIQUE(c)) WITHOUT ROWID;INSERT INTO sample VALUES(\'a\',1,\'x\'),(\'b\',2,\'y\')')
 add('WITHOUT ROWID explicit secondary','CREATE TABLE sample(a TEXT,b INT,c TEXT,PRIMARY KEY(a DESC,b)) WITHOUT ROWID;INSERT INTO sample VALUES(\'a\',1,\'x\'),(\'b\',2,\'y\');CREATE INDEX idx ON sample(c)')
 add('WITHOUT ROWID reused primary UNIQUE','CREATE TABLE sample(a TEXT UNIQUE,PRIMARY KEY(a)) WITHOUT ROWID;INSERT INTO sample VALUES(\'a\'),(\'b\')')
 add('WITHOUT ROWID reused primary direction','CREATE TABLE sample(a TEXT UNIQUE,PRIMARY KEY(a DESC)) WITHOUT ROWID;INSERT INTO sample VALUES(\'a\'),(\'b\')')
 add('WITHOUT ROWID different PK collation suffix','CREATE TABLE sample(a TEXT,b INT,c TEXT,PRIMARY KEY(a DESC,b)) WITHOUT ROWID;INSERT INTO sample VALUES(\'a\',1,\'x\'),(\'b\',2,\'y\');CREATE INDEX idx ON sample(a COLLATE NOCASE,c)')
 add('STRICT valid storage classes','CREATE TABLE sample(a INTEGER PRIMARY KEY,b REAL,c TEXT,d BLOB,e ANY) STRICT;INSERT INTO sample VALUES(1,2,\'text\',X\'00ff\',\'uncoerced\')')
 add('generated fields preserve physical indexes','CREATE TABLE sample(a INTEGER,b INTEGER AS (a+1),c TEXT NOT NULL UNIQUE,d REAL AS (a*2) STORED);INSERT INTO sample(a,c) VALUES(1,\'one\'),(2,\'two\');CREATE INDEX idx ON sample(d DESC,c)')
 for expression in ['lower(v)', '+v', 'CAST(v AS TEXT)', 'lower(v COLLATE RTRIM)', '(v COLLATE NOCASE) COLLATE RTRIM', "coalesce(w,0)+length(v)", "CASE WHEN w>0 THEN v ELSE NULL END"]:
  add('expression '+expression,"CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT COLLATE NOCASE,w REAL);INSERT INTO sample VALUES(1,'a',2),(2,'A',-1),(3,'b ',NULL),(4,NULL,0);CREATE INDEX idx ON sample("+expression+" DESC,w ASC)")
 for expression in ['desc', 'asc', 'a+desc', 'a+asc', '+desc', 'desc DESC', 'asc ASC']:
  add('direction keyword column '+expression,'CREATE TABLE sample(a INT,desc INT UNIQUE,asc INT);INSERT INTO sample VALUES(1,3,2),(2,2,3);CREATE INDEX idx ON sample('+expression+')')
 for predicate in ['w>0', 'v IS NOT NULL', 'w IN (0,2,NULL)', 'w AND v IS NOT NULL', 'NULL', '0', '1', "v COLLATE NOCASE='a'"]:
  add('partial '+predicate,"CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT,w INT);INSERT INTO sample VALUES(1,'a',2),(2,'A',-1),(3,'b',NULL),(4,NULL,0);CREATE INDEX idx ON sample(lower(v) DESC) WHERE "+predicate)
 add('partial UNIQUE nullable keys',"CREATE TABLE sample(v TEXT,w INT);INSERT INTO sample VALUES(NULL,1),(NULL,2),('A',1),('a',0);CREATE UNIQUE INDEX idx ON sample(lower(v)) WHERE w>0")
 add('generated expression and predicate',"CREATE TABLE sample(a INT,b INT AS(a+1),c INT AS(b*2));INSERT INTO sample(a) VALUES(1),(2),(3),(NULL);CREATE INDEX idx ON sample(c+1 DESC,b) WHERE b>2")
 for expression in ['a', '+a', 'a COLLATE RTRIM', 'lower(a)', 'b+0']:
  add('WITHOUT ROWID expression '+expression,"CREATE TABLE sample(a TEXT COLLATE NOCASE,b INT,c TEXT,PRIMARY KEY(a DESC,b)) WITHOUT ROWID;INSERT INTO sample VALUES('a',1,'x'),('A',2,'y'),('b',3,'z');CREATE INDEX idx ON sample("+expression+" DESC) WHERE b>=2")
 add('stale expression keys',"CREATE TABLE sample(v INT);INSERT INTO sample VALUES(1),(2),(3);CREATE INDEX idx ON sample(v+1);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'v+1','v+2') WHERE name='idx'")
 add('stale partial membership',"CREATE TABLE sample(v INT);INSERT INTO sample VALUES(1),(2),(3);CREATE INDEX idx ON sample(v) WHERE v>1;PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'v>1','v>2') WHERE name='idx'")
 add('stale partial missing keys',"CREATE TABLE sample(v INT);INSERT INTO sample VALUES(1),(2),(3);CREATE INDEX idx ON sample(v) WHERE v>2;PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'v>2','v>1') WHERE name='idx'")
 for collation in ['BINARY','NOCASE','RTRIM']:
  add('Unicode '+collation,'CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT);CREATE INDEX idx ON sample(v COLLATE '+collation+')', [('INSERT INTO sample(v) VALUES(?)',(value,)) for value in ['Ā','é','a','水','😁','A','a ','a\x00x','a\x00y']])
 add('required value violated',"CREATE TABLE sample(v TEXT);INSERT INTO sample VALUES(NULL);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'v TEXT','v TEXT NOT NULL') WHERE name='sample'")
 add('STRICT type violated',"CREATE TABLE sample(id INTEGER PRIMARY KEY,v INTEGER);INSERT INTO sample VALUES(1,'not integer');PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=sql||' STRICT' WHERE name='sample'")
 add('STRICT primary null violated',"CREATE TABLE sample(id TEXT PRIMARY KEY,v TEXT);INSERT INTO sample VALUES(NULL,'x');PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=sql||' STRICT' WHERE name='sample'")
 add('changed UNIQUE collation duplicates',"CREATE TABLE sample(v TEXT COLLATE BINARY UNIQUE);INSERT INTO sample VALUES('A'),('a');PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'COLLATE BINARY','COLLATE NOCASE') WHERE name='sample'")
 add('orphan automatic index',"CREATE TABLE sample(v TEXT UNIQUE);INSERT INTO sample VALUES('a');PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'TEXT UNIQUE','TEXT') WHERE name='sample'")
 add('changed index direction',"CREATE TABLE sample(v TEXT);INSERT INTO sample VALUES('a'),('b'),('c');CREATE INDEX idx ON sample(v ASC);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'v ASC','v DESC') WHERE name='idx'")
 add('changed index field',"CREATE TABLE sample(v TEXT,w INTEGER);INSERT INTO sample VALUES('a',1),('b',2);CREATE INDEX idx ON sample(v);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'sample(v)','sample(w)') WHERE name='idx'")
 add('changed WITHOUT ROWID order',"CREATE TABLE sample(a TEXT PRIMARY KEY,b INTEGER) WITHOUT ROWID;INSERT INTO sample VALUES('a',1),('b',2);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'a TEXT PRIMARY KEY','a TEXT PRIMARY KEY DESC') WHERE name='sample'")
 if encoding=='UTF-8':
  for damage in ['key','duplicate']:
   add('damaged index '+damage,'CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT);INSERT INTO sample VALUES(10,\'a\'),(20,\'b\'),(30,\'c\');CREATE INDEX idx ON sample(v)',damage=damage)
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 60);
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let snapshot = Snapshot::read(&bytes);
            let (valid, details) = match snapshot {
                Ok(db) => {
                    assert!(db.tables["countries"].rows.is_empty());
                    let mut checker = Checker { db, report: CoreReport::default() };
                    let result = checker.constraints(&bytes);
                    assert!(checker.report.warnings.is_empty(), "{:?}", checker.report.warnings);
                    (result.is_ok() && checker.report.is_valid(), format!("{result:?}: {:?}", checker.report.errors))
                }
                Err(error) => (false, error.to_string()),
            };
            // SQLite 3.54.1 checks missing partial-index entries but overlooks
            // surplus entries excluded by a changed predicate. Require exact
            // membership here, including that independently reproduced case.
            let stale_partial = case["name"].as_str().unwrap().ends_with(" stale partial membership");
            let reference = case["valid"].as_bool().unwrap();
            if stale_partial {
                assert!(reference, "reference behavior changed: {case}");
            }
            assert_eq!(valid, reference && !stale_partial, "{}: {details}", case["name"]);
        }
        eprintln!("{} SQLite constraint/index comparison cases passed", cases.len());
    }
}
