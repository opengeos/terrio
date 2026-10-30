//! Registered attribute/feature views and their identifier requirements.
use super::*;
use std::collections::BTreeSet;

impl Checker {
    pub(super) fn views(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(contents) = self.db.tables.get("gpkg_contents") else { return Ok(()) };
        for row in &contents.rows {
            let Some(name) = contents.get(row, "table_name").and_then(Value::as_str) else { continue };
            let Some(kind @ ("features" | "attributes")) = contents.get(row, "data_type").and_then(Value::as_str) else { continue };
            if !self.db.objects.iter().any(|o| o.0 == "view" && o.1.eq_ignore_ascii_case(name)) {
                continue;
            }
            let view = sqlite_expr::View::compile(&self.db, name)?;
            let requirement = if kind == "features" { 150 } else { 151 };
            self.report.require(
                view.schema.columns.first().is_some_and(|c| c.data_type == "INTEGER"),
                requirement,
                format!("{name}: view requires an INTEGER first column"),
            );
            let geometry =
                self.db
                    .tables
                    .get("gpkg_geometry_columns")
                    .and_then(|registry| {
                        registry.rows.iter().find(|r| registry.get(r, "table_name").and_then(Value::as_str) == Some(name)).and_then(|r| {
                            Some((registry.get(r, "column_name")?.as_str()?, registry.get(r, "geometry_type_name")?.as_str()?))
                        })
                    })
                    .filter(|_| kind == "features");
            if let Some((field, kind)) = geometry {
                let column = view.schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(field));
                self.report.require(column.is_some(), 24, format!("{name}: missing registered geometry column"));
                if let Some(column) = column {
                    self.report.require(
                        features::BASE.contains(&column.data_type.as_str()) || features::EXTENDED.contains(&column.data_type.as_str()),
                        25,
                        format!("{name}: invalid view geometry type"),
                    );
                    self.report.require(
                        column.data_type == kind,
                        31,
                        format!("{name}: view geometry declaration disagrees with registration"),
                    );
                }
            }
            for column in &view.schema.columns {
                if geometry.is_none_or(|(field, _)| !column.name.eq_ignore_ascii_case(field)) {
                    self.report.require(
                        content::valid_data_type(&column.data_type),
                        5,
                        format!("{name}.{}: invalid view attribute type {}", column.name, column.data_type),
                    );
                }
            }
            if view.schema.columns.first().is_some_and(|c| !matches!(c.collation.as_str(), "BINARY" | "NOCASE" | "RTRIM")) {
                return Err(Error::Unsupported("SQLite view identifier collation".into()));
            }
            let mut identifiers = BTreeSet::new();
            let mut unique = true;
            view.visit(&self.db, bytes, &mut |row| {
                if let Some((value, column)) = row.first().zip(view.schema.columns.first()) {
                    let mut column = column.clone();
                    column.data_type = "BLOB".into();
                    unique &= integrity::atom(value, &column, false).is_some_and(|key| identifiers.insert(key));
                } else {
                    unique = false;
                }
                Ok(())
            })?;
            self.report.require(unique, requirement, format!("{name}: view identifiers must be unique and non-NULL"));
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn registered_views_match_gdal_and_geometry_validation() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions();root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);base=root/'base.gpkg';cases=[]
gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
with sqlite3.connect(base) as db:
 geom,kind,srs=db.execute("SELECT column_name,geometry_type_name,srs_id FROM gpkg_geometry_columns WHERE table_name='countries'").fetchone()
def add(name,query,*,feature=False,definition='id INTEGER,v TEXT',rows=[(1,'one'),(2,'two')],extra='',geometry_kind=None):
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  if feature:
   db.execute('CREATE TABLE src(id INTEGER,v TEXT,geom '+(geometry_kind or kind)+')')
   db.execute('INSERT INTO src SELECT fid,CAST(fid AS TEXT),"'+geom+'" FROM countries LIMIT 3')
  else:
   db.execute('CREATE TABLE src('+definition+')');db.executemany('INSERT INTO src VALUES(?,?)',rows)
  if extra:db.executescript(extra)
  db.execute('CREATE VIEW v AS '+query)
  db.execute("INSERT INTO gpkg_contents(table_name,data_type,identifier,srs_id) VALUES('v',?,'v',?)",('features' if feature else 'attributes',srs if feature else None))
  if feature:db.execute('INSERT INTO gpkg_geometry_columns VALUES(?,?,?,?,0,0)',('v','geom',kind,srs))
  db.commit()
  checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False);checker.version=(1,4,0);checker.extended_pragma_info=True
  try:
   if feature:checker._check_vector_user_table(db.cursor(),'v')
   else:checker._check_attribute_user_table(db.cursor(),'v')
   valid=not checker.errors
  except (sqlite3.Error,IndexError,TypeError,ValueError):valid=False
 cases.append(dict(name=name,path=str(path),valid=valid))
add('attribute projection','SELECT id,v FROM src')
add('attribute star','SELECT * FROM src')
add('filtered attribute','SELECT s.id,s.v FROM src s WHERE id>1')
add('empty attributes','SELECT * FROM src WHERE 0')
add('nested attributes','SELECT * FROM middle',extra='CREATE VIEW middle AS SELECT id,v FROM src')
add('duplicate identifiers','SELECT * FROM src',rows=[(1,'one'),(1,'two')])
add('NULL identifier','SELECT * FROM src',rows=[(None,'one')])
add('DISTINCT removes duplicate ids','SELECT DISTINCT * FROM src',rows=[(1,'one'),(1,'one')])
add('DISTINCT leaves duplicate ids','SELECT DISTINCT * FROM src',rows=[(1,'one'),(1,'two')])
add('text stored in INTEGER id','SELECT * FROM src',rows=[('one','one'),('two','two')])
add('INT is not INTEGER view key','SELECT * FROM src',definition='id INT,v TEXT')
add('text key','SELECT * FROM src',definition='id TEXT,v TEXT')
add('computed key has no declared type','SELECT id+0 AS id,v FROM src')
add('cast key is INT','SELECT CAST(id AS INTEGER) AS id,v FROM src')
add('computed attribute has no declared type','SELECT id,upper(v) AS v FROM src')
add('cast attribute type','SELECT id,CAST(v AS TEXT) AS v FROM src')
add('duplicate IDs use collation','SELECT * FROM src',definition='id TEXT COLLATE NOCASE,v TEXT',rows=[('a','one'),('A','two')])
for query in ['SELECT * FROM src','SELECT s.id,s.v,s.geom FROM src s WHERE id>0','SELECT * FROM src WHERE 0','SELECT id,v,geom FROM middle']:
 add('feature '+query,query,feature=True,extra='CREATE VIEW middle AS SELECT * FROM src')
add('feature geometry type mismatch','SELECT * FROM src',feature=True,geometry_kind='BLOB')
add('feature missing geometry','SELECT id,v FROM src',feature=True)
add('feature computed geometry loses declaration','SELECT id,v,CASE WHEN id>0 THEN geom END AS geom FROM src',feature=True)
add('feature invalid geometry payload','SELECT * FROM src',feature=True,extra="UPDATE src SET geom=X'000102030405060708090a0b0c0d0e0f'")
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let mut checker = Checker { db: Snapshot::read(&bytes).unwrap(), report: CoreReport::default() };
            checker.views(&bytes).unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
            checker.geometries(&bytes).unwrap();
            assert_eq!(checker.report.errors.is_empty(), case["valid"].as_bool().unwrap(), "{}: {:?}", case["name"], checker.report);
            assert!(checker.report.warnings.is_empty());
            let full = check_core(&bytes);
            assert_eq!(full.errors.is_empty(), case["valid"].as_bool().unwrap(), "full validator {}: {full:?}", case["name"]);
        }
        eprintln!("{} registered view/GDAL comparisons passed", cases.len());
    }

    #[test]
    fn unsupported_or_recursive_views_cannot_pass_silently() {
        let deep = format!("CREATE VIEW v AS SELECT {} FROM sample", vec!["id"; 140].join("+"));
        let deep_predicate = format!("CREATE VIEW v AS SELECT id FROM sample WHERE {}", vec!["id"; 140].join("+"));
        for sql in [
            "CREATE VIEW v AS SELECT * FROM v",
            "CREATE VIEW v AS SELECT * FROM missing",
            "CREATE VIEW v AS SELECT count(*) FROM sample",
            "CREATE VIEW v AS SELECT * FROM sample ORDER BY id",
            "CREATE VIEW v AS SELECT * FROM sample JOIN sample AS other ON sample.id=other.id",
            "CREATE VIEW v AS SELECT * FROM sample UNION ALL SELECT * FROM sample",
            "CREATE VIEW v(a,b) AS SELECT id FROM sample",
            &deep,
            &deep_predicate,
        ] {
            let db = Snapshot {
                objects: vec![("view".into(), "v".into(), "v".into(), 0, sql.into())],
                tables: BTreeMap::from([(
                    "sample".into(),
                    Table::from_sql("sample", "CREATE TABLE sample(id INTEGER PRIMARY KEY)").unwrap(),
                )]),
            };
            assert!(sqlite_expr::View::compile(&db, "v").is_err(), "{sql}");
        }
    }
}
