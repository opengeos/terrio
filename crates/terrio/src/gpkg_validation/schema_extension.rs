//! Schema-extension declarations and constraints, GeoPackage 1.4 requirements 103–114/141.
//! GDAL 3.13.3 validate_gpkg.py is the reference (see NOTICE); spec corrections
//! cover dangling constraint names, NULL range inclusivity and NULL glob values.
use super::*;
use std::cmp::Ordering;

fn int_real_cmp(integer: i64, real: f64) -> Option<Ordering> {
    if real.is_nan() {
        return None;
    }
    if real < i64::MIN as f64 {
        return Some(Ordering::Greater);
    }
    if real >= 9_223_372_036_854_775_808.0 {
        return Some(Ordering::Less);
    }
    match integer.cmp(&(real as i64)) {
        Ordering::Equal => (integer as f64).partial_cmp(&real),
        other => Some(other),
    }
}
/// SQLite's stored-value order for two NUMERIC-affinity columns. Preserve
/// integer precision when comparing an INTEGER to a REAL near 2^53 or 2^63.
pub(super) fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Real(a), Value::Real(b)) => a.partial_cmp(b),
        (Value::Int(a), Value::Real(b)) => int_real_cmp(*a, *b),
        (Value::Real(a), Value::Int(b)) => int_real_cmp(*b, *a).map(Ordering::reverse),
        (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
        (Value::Blob(a), Value::Blob(b)) => Some(a.cmp(b)),
        (Value::Blob(_), _) => Some(Ordering::Greater),
        (_, Value::Blob(_)) => Some(Ordering::Less),
        (Value::Text(_), _) => Some(Ordering::Greater),
        (_, Value::Text(_)) => Some(Ordering::Less),
    }
}
fn null(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

impl Checker {
    pub(super) fn schema_extension(&mut self) {
        let declarations: Vec<_> = self
            .db
            .tables
            .get("gpkg_extensions")
            .map(|table| {
                table
                    .rows
                    .iter()
                    .filter(|row| table.get(row, "extension_name").and_then(Value::as_str) == Some("gpkg_schema"))
                    .map(|row| (table.get(row, "scope"), table.get(row, "column_name")))
                    .collect()
            })
            .unwrap_or_default();
        if !declarations.is_empty() {
            self.report.require(
                self.report.version.unwrap_or(10400) < 10201 || declarations.len() == 2,
                141,
                "gpkg_schema must have exactly two declarations",
            );
            self.report.require(
                declarations.iter().all(|(scope, _)| scope.and_then(Value::as_str) == Some("read-write")),
                141,
                "gpkg_schema extension must have read-write scope",
            );
            self.report.require(
                declarations.iter().all(|(_, field)| null(*field)),
                141,
                "gpkg_schema declaration column_name must be null",
            );
        }
        if !self.db.object("gpkg_data_columns") {
            self.report.require(declarations.is_empty(), 141, "declared gpkg_data_columns table is missing");
            return;
        }
        self.structure(
            "gpkg_data_columns",
            103,
            &[
                column("table_name", "TEXT", true, None, 1),
                column("column_name", "TEXT", true, None, 2),
                column("name", "TEXT", false, None, 0),
                column("title", "TEXT", false, None, 0),
                column("description", "TEXT", false, None, 0),
                column("mime_type", "TEXT", false, None, 0),
                column("constraint_name", "TEXT", false, None, 0),
            ],
            false,
        );
        let legacy = self.report.version.is_some_and(|v| v < 10100);
        let min_flag = if legacy { "minIsInclusive" } else { "min_is_inclusive" };
        let max_flag = if legacy { "maxIsInclusive" } else { "max_is_inclusive" };
        self.report.require(self.db.object("gpkg_data_column_constraints"), 141, "gpkg_data_column_constraints table is missing");
        self.structure(
            "gpkg_data_column_constraints",
            107,
            &[
                column("constraint_name", "TEXT", true, None, 0),
                column("constraint_type", "TEXT", true, None, 0),
                column("value", "TEXT", false, None, 0),
                column("min", "NUMERIC", false, None, 0),
                column(min_flag, "BOOLEAN", false, None, 0),
                column("max", "NUMERIC", false, None, 0),
                column(max_flag, "BOOLEAN", false, None, 0),
                column("description", "TEXT", false, None, 0),
            ],
            false,
        );
        let constraints = self.db.tables.get("gpkg_data_column_constraints");
        if let Some(columns) = self.db.tables.get("gpkg_data_columns") {
            for row in &columns.rows {
                let name = columns.get(row, "table_name").and_then(Value::as_str);
                let field = columns.get(row, "column_name").and_then(Value::as_str);
                let registered = ["gpkg_contents", "gpkg_extensions"].iter().any(|table| {
                    self.db
                        .tables
                        .get(*table)
                        .is_some_and(|table| table.rows.iter().any(|row| table.get(row, "table_name").and_then(Value::as_str) == name))
                });
                self.report.require(name.is_some() && registered, 104, "schema column table must be registered in contents or extensions");
                if let Some((_, table)) = name.and_then(|name| self.db.tables.iter().find(|(key, _)| key.eq_ignore_ascii_case(name))) {
                    self.report.require(
                        field.is_some_and(|field| table.schema.columns.iter().any(|column| column.name.eq_ignore_ascii_case(field))),
                        105,
                        "schema references a missing column",
                    );
                } else if name.is_some_and(|name| self.db.object(name)) {
                    self.report.unsupported_checks.push(format!("schema reference to view {name:?} still needs view-column validation"));
                } else {
                    self.report.require(false, 105, "schema references a missing column");
                }
                if !null(columns.get(row, "constraint_name")) {
                    let name = columns.get(row, "constraint_name").and_then(Value::as_str);
                    self.report.require(
                        name.is_some_and(|name| {
                            name.to_lowercase() == name
                                && constraints.is_some_and(|table| {
                                    table.rows.iter().any(|row| table.get(row, "constraint_name").and_then(Value::as_str) == Some(name))
                                })
                        }),
                        106,
                        "schema constraint_name must reference a lowercase constraint name",
                    );
                }
            }
        }
        let Some(table) = constraints else { return };
        let rows = &table.rows;
        let is = |row: &[Value], kind| table.get(row, "constraint_type").and_then(Value::as_str) == Some(kind);
        self.report.require(
            rows.iter().all(|row| matches!(table.get(row, "constraint_type").and_then(Value::as_str), Some("range" | "enum" | "glob"))),
            108,
            "invalid constraint_type",
        );
        let mut counts = BTreeMap::new();
        for row in rows.iter().filter(|row| is(row, "range") || is(row, "glob")) {
            *counts.entry(table.get(row, "constraint_name").and_then(Value::as_str)).or_insert(0usize) += 1;
        }
        self.report.require(counts.values().all(|count| *count == 1), 109, "range/glob constraint names must be unique");
        self.report.require(
            !rows.iter().any(|row| is(row, "range") && !null(table.get(row, "value"))),
            110,
            "range constraint value must be null",
        );
        for bound in ["min", "max"] {
            self.report.require(
                !rows.iter().any(|row| is(row, "range") && null(table.get(row, bound))),
                111,
                format!("range constraint {bound} must be non-null"),
            );
        }
        self.report.require(
            !rows.iter().any(|row| {
                is(row, "range")
                    && table
                        .get(row, "min")
                        .zip(table.get(row, "max"))
                        .and_then(|(min, max)| compare(min, max))
                        .is_some_and(|order| order != Ordering::Less)
            }),
            111,
            "range constraint min must be less than max",
        );
        for flag in [min_flag, max_flag] {
            self.report.require(
                rows.iter().filter(|row| is(row, "range")).all(|row| matches!(table.get(row, flag), Some(Value::Int(0 | 1)))),
                112,
                format!("range constraint {flag} must be 0 or 1"),
            );
        }
        for field in ["min", min_flag, "max", max_flag] {
            self.report.require(
                !rows.iter().any(|row| (is(row, "enum") || is(row, "glob")) && !null(table.get(row, field))),
                113,
                format!("enum/glob constraint {field} must be null"),
            );
        }
        self.report.require(
            !rows.iter().any(|row| (is(row, "enum") || is(row, "glob")) && null(table.get(row, "value"))),
            114,
            "enum/glob constraint value must be non-null",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_numeric_comparisons_preserve_integer_precision() {
        for (integer, real, expected) in [
            (9_007_199_254_740_993, 9_007_199_254_740_992.0, Ordering::Greater),
            (i64::MAX, 9_223_372_036_854_775_808.0, Ordering::Less),
            (i64::MIN, i64::MIN as f64, Ordering::Equal),
            (1, 1.5, Ordering::Less),
            (-1, -1.5, Ordering::Greater),
        ] {
            assert_eq!(compare(&Value::Int(integer), &Value::Real(real)), Some(expected));
            assert_eq!(compare(&Value::Real(real), &Value::Int(integer)), Some(expected.reverse()));
        }
        assert_eq!(compare(&Value::Null, &Value::Int(1)), None);
        assert_eq!(compare(&Value::Int(1), &Value::Text("0".into())), Some(Ordering::Less));
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn schema_rules_match_gdal_with_documented_spec_corrections() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);base=root/'base.gpkg';cases=[]
gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
with sqlite3.connect(base) as db:
 field=next(row[1] for row in db.execute('PRAGMA table_info(countries)') if row[1] not in ('fid','geom'))
 db.executescript('''
 CREATE TABLE IF NOT EXISTS gpkg_extensions(table_name TEXT,column_name TEXT,extension_name TEXT NOT NULL,definition TEXT NOT NULL,scope TEXT NOT NULL,UNIQUE(table_name,column_name,extension_name));
 CREATE TABLE gpkg_data_columns(table_name TEXT NOT NULL,column_name TEXT NOT NULL,name TEXT,title TEXT,description TEXT,mime_type TEXT,constraint_name TEXT,PRIMARY KEY(table_name,column_name));
 CREATE TABLE gpkg_data_column_constraints(constraint_name TEXT NOT NULL,constraint_type TEXT NOT NULL,value TEXT,min NUMERIC,min_is_inclusive BOOLEAN,max NUMERIC,max_is_inclusive BOOLEAN,description TEXT);
 INSERT INTO gpkg_extensions VALUES('gpkg_data_columns',NULL,'gpkg_schema','http://www.geopackage.org/spec140/#extension_schema','read-write');
 INSERT INTO gpkg_extensions VALUES('gpkg_data_column_constraints',NULL,'gpkg_schema','http://www.geopackage.org/spec140/#extension_schema','read-write');
 INSERT INTO gpkg_data_column_constraints VALUES('test_range','range',NULL,0,1,100,1,NULL);
 INSERT INTO gpkg_data_column_constraints VALUES('test_enum','enum','one',NULL,NULL,NULL,NULL,NULL),('test_enum','enum','two',NULL,NULL,NULL,NULL,NULL);
 INSERT INTO gpkg_data_column_constraints VALUES('test_glob','glob','*',NULL,NULL,NULL,NULL,NULL);
 ''')
 db.execute("INSERT INTO gpkg_data_columns(table_name,column_name,constraint_name) VALUES('countries',?,'test_range')",(field,))
def add(name,sql='',extra=(),version=(1,4,0),reference_exception=False):
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('PRAGMA ignore_check_constraints=ON')
  if sql:db.executescript(sql)
  assert db.execute('PRAGMA integrity_check').fetchone()[0]=='ok',name
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=version;checker.extended_pragma_info=True
 try:
  with sqlite3.connect(path) as db:checker._check_schema(db.cursor())
 except sqlite3.OperationalError:assert reference_exception,name
 else:assert not reference_exception,name
 cases.append(dict(name=name,path=str(path),version=version[0]*10000+version[1]*100+version[2],requirements=sorted([e[0] for e in checker.errors]+list(extra)),reference_exception=reference_exception))
add('schema constraints on Natural Earth field')
add('empty schema tables','DELETE FROM gpkg_data_columns;DELETE FROM gpkg_data_column_constraints')
add('no schema extension','DROP TABLE gpkg_data_columns;DROP TABLE gpkg_data_column_constraints;DELETE FROM gpkg_extensions')
add('missing declared columns','DROP TABLE gpkg_data_columns')
add('missing constraints table','DROP TABLE gpkg_data_column_constraints',reference_exception=True)
add('one declaration',"DELETE FROM gpkg_extensions WHERE table_name='gpkg_data_column_constraints'")
add('wrong extension scope',"UPDATE gpkg_extensions SET scope='write-only'")
add('declaration column name',"UPDATE gpkg_extensions SET column_name='column_name'")
add('unregistered column table',"UPDATE gpkg_data_columns SET table_name='missing'")
add('missing column',"UPDATE gpkg_data_columns SET column_name='missing'")
add('case insensitive column',"UPDATE gpkg_data_columns SET column_name=upper(column_name)")
add('extension table column',"UPDATE gpkg_data_columns SET table_name='gpkg_data_columns',column_name='name'")
add('unknown constraint reference',"UPDATE gpkg_data_columns SET constraint_name='missing'",extra=[106])
add('uppercase constraint reference',"UPDATE gpkg_data_columns SET constraint_name='TEST_RANGE';UPDATE gpkg_data_column_constraints SET constraint_name='TEST_RANGE' WHERE constraint_name='test_range'",extra=[106])
add('invalid constraint type',"UPDATE gpkg_data_column_constraints SET constraint_type='invalid'")
add('duplicate range name',"INSERT INTO gpkg_data_column_constraints SELECT * FROM gpkg_data_column_constraints WHERE constraint_type='range'")
add('mixed glob and range name',"UPDATE gpkg_data_column_constraints SET constraint_name='test_range' WHERE constraint_type='glob'")
add('range nonnull value',"UPDATE gpkg_data_column_constraints SET value='x' WHERE constraint_type='range'")
for bound in ['min','max']:
 add('range null '+bound,"UPDATE gpkg_data_column_constraints SET "+bound+"=NULL WHERE constraint_type='range'")
for minval,maxval in [('100','0'),('100','100'),('9007199254740993','9007199254740992.0'),('9223372036854775807','9223372036854775808.0'),("'a'","'z'"),("'z'","'a'"),("'abc'",'100'),('100',"'abc'")]:
 add('range bounds '+minval+' '+maxval,"UPDATE gpkg_data_column_constraints SET min="+minval+",max="+maxval+" WHERE constraint_type='range'")
for flag in ['min_is_inclusive','max_is_inclusive']:
 for value in [-1,2,0.5,None]:
  add('range flag '+flag+str(value),"UPDATE gpkg_data_column_constraints SET "+flag+"="+('NULL' if value is None else str(value))+" WHERE constraint_type='range'",extra=[112] if value is None else [])
for kind in ['enum','glob']:
 for col in ['min','max','min_is_inclusive','max_is_inclusive']:
  add(kind+' nonnull '+col,"UPDATE gpkg_data_column_constraints SET "+col+"=1 WHERE constraint_type='"+kind+"'")
 add(kind+' null value',"UPDATE gpkg_data_column_constraints SET value=NULL WHERE constraint_type='"+kind+"'",extra=[114] if kind=='glob' else [])
add('legacy inclusivity names','ALTER TABLE gpkg_data_column_constraints RENAME COLUMN min_is_inclusive TO minIsInclusive;ALTER TABLE gpkg_data_column_constraints RENAME COLUMN max_is_inclusive TO maxIsInclusive',version=(1,0,0))
add('wrong constraints schema type',"PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'min NUMERIC','min REAL') WHERE name='gpkg_data_column_constraints'")
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 40);
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let mut checker = Checker {
                db: Snapshot::read(&bytes).unwrap(),
                report: CoreReport { version: Some(case["version"].as_u64().unwrap() as u32), ..Default::default() },
            };
            checker.schema_extension();
            if case["reference_exception"].as_bool() == Some(true) {
                assert!(!checker.report.is_valid(), "{case}");
                continue;
            }
            let mut actual: Vec<_> = checker.report.errors.iter().map(|e| e.requirement.number().expect("core requirement")).collect();
            actual.sort_unstable();
            let expected: Vec<u32> = serde_json::from_value(case["requirements"].clone()).unwrap();
            assert_eq!(actual, expected, "{}: {:?}", case["name"], checker.report.errors);
        }
        eprintln!("{} schema comparison cases passed", cases.len());
    }
}
