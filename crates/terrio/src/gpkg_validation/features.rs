//! Feature-table declarations and geometry-column registration (OGC GeoPackage 1.4).
//! Rule numbering follows GDAL 3.13.3's validate_gpkg.py; see NOTICE.
use super::*;

pub(super) const BASE: &[&str] =
    &["GEOMETRY", "POINT", "LINESTRING", "POLYGON", "MULTIPOINT", "MULTILINESTRING", "MULTIPOLYGON", "GEOMETRYCOLLECTION"];
pub(super) const EXTENDED: &[&str] = &["CIRCULARSTRING", "COMPOUNDCURVE", "CURVEPOLYGON", "MULTICURVE", "MULTISURFACE", "CURVE", "SURFACE"];

impl Checker {
    /// Checks declarations only. Binary geometry and R-tree checks are separate
    /// rule groups, required before the complete validator becomes public.
    pub(super) fn features(&mut self) {
        let Some(contents) = self.db.tables.get("gpkg_contents") else { return };
        let names: Vec<_> = contents
            .rows
            .iter()
            .filter(|row| contents.get(row, "data_type").and_then(Value::as_str) == Some("features"))
            .filter_map(|row| contents.get(row, "table_name").and_then(Value::as_str).map(str::to_owned))
            .collect();
        if names.is_empty() {
            return;
        }
        self.structure(
            "gpkg_geometry_columns",
            21,
            &[
                column("table_name", "TEXT", true, None, 1),
                column("column_name", "TEXT", true, None, 2),
                column("geometry_type_name", "TEXT", true, None, 0),
                column("srs_id", "INTEGER", true, None, 0),
                column("z", "TINYINT", true, None, 0),
                column("m", "TINYINT", true, None, 0),
            ],
            false,
        );
        let Some(registry) = self.db.tables.get("gpkg_geometry_columns") else { return };
        for name in &names {
            let rows: Vec<_> =
                registry.rows.iter().filter(|row| registry.get(row, "table_name").and_then(Value::as_str) == Some(name.as_str())).collect();
            self.report.require(rows.len() == 1, 22, format!("{name}: expected one geometry-column registration"));
            for row in rows {
                let field = registry.get(row, "column_name").and_then(Value::as_str);
                let kind = registry.get(row, "geometry_type_name").and_then(Value::as_str);
                for axis in ["z", "m"] {
                    self.report.require(
                        matches!(registry.get(row, axis), Some(Value::Int(0..=2))),
                        27,
                        format!("{name}: invalid {axis} policy"),
                    );
                }
                if let Some((_, table)) = self.db.tables.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)) {
                    let geometry =
                        field.and_then(|field| table.schema.columns.iter().find(|column| column.name.eq_ignore_ascii_case(field)));
                    self.report.require(geometry.is_some(), 24, format!("{name}: missing registered geometry column"));
                    if let Some(geometry) = geometry {
                        self.report.require(
                            BASE.contains(&geometry.data_type.as_str()) || EXTENDED.contains(&geometry.data_type.as_str()),
                            25,
                            format!("{name}: invalid geometry column type"),
                        );
                        self.report.require(
                            kind == Some(geometry.data_type.as_str()),
                            31,
                            format!("{name}: geometry declaration disagrees with registration"),
                        );
                    }
                    self.report.require(
                        table.schema.rowid_alias.is_some_and(|i| Some(&table.schema.columns[i]) != geometry),
                        29,
                        format!("{name}: requires an INTEGER primary-key rowid alias separate from geometry"),
                    );
                    for column in &table.schema.columns {
                        if Some(column) != geometry {
                            self.report.require(
                                content::valid_data_type(&column.data_type),
                                5,
                                format!("{name}.{}: invalid attribute type", column.name),
                            );
                        }
                    }
                }
                if kind.is_some_and(|kind| EXTENDED.contains(&kind)) {
                    let extension = format!("gpkg_geom_{}", kind.unwrap());
                    let declared = self.db.tables.get("gpkg_extensions").is_some_and(|extensions| {
                        extensions.rows.iter().any(|row| {
                            extensions.get(row, "table_name").and_then(Value::as_str) == Some(name.as_str())
                                && extensions.get(row, "column_name").and_then(Value::as_str) == field
                                && extensions.get(row, "extension_name").and_then(Value::as_str) == Some(extension.as_str())
                                && extensions.get(row, "scope").and_then(Value::as_str) == Some("read-write")
                        })
                    });
                    self.report.require(declared, 68, format!("{name}: missing geometry-type extension declaration"));
                }
            }
        }
        let contents = &self.db.tables["gpkg_contents"];
        for row in &registry.rows {
            let name = registry.get(row, "table_name").and_then(Value::as_str);
            let matches: Vec<_> = contents
                .rows
                .iter()
                .filter(|row| {
                    contents.get(row, "table_name").and_then(Value::as_str) == name
                        && contents.get(row, "data_type").and_then(Value::as_str) == Some("features")
                })
                .collect();
            self.report.require(matches.len() == 1, 23, "geometry registration must reference one features entry");
            let srs = registry.get(row, "srs_id");
            self.report.require(
                matches!(srs, Some(Value::Int(id)) if self.db.srs(*id)),
                14,
                "geometry registration references an undefined SRS",
            );
            for entry in matches {
                if !matches!(contents.get(entry, "srs_id"), Some(Value::Null)) {
                    self.report.require(srs == contents.get(entry, "srs_id"), 146, "geometry and contents SRS identifiers disagree");
                }
            }
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    fn feature_primary_key_must_be_a_separate_rowid_alias() {
        for (declaration, valid) in [
            ("fid INTEGER PRIMARY KEY, geom POINT", true),
            ("fid INT PRIMARY KEY, geom POINT", false),
            ("fid INTEGER PRIMARY KEY DESC, geom POINT", false),
            ("fid INTEGER, geom POINT, PRIMARY KEY(fid,geom)", false),
            ("geom INTEGER PRIMARY KEY", false),
        ] {
            let table = |sql: &str, rows| Table { schema: Schema::parse(sql).unwrap(), rows, evaluator: None };
            let text = |value: &str| Value::Text(value.into());
            let db = Snapshot {
                objects: vec![],
                tables: BTreeMap::from([
                    (
                        "gpkg_contents".into(),
                        table(
                            "CREATE TABLE gpkg_contents(table_name TEXT,data_type TEXT,srs_id INTEGER)",
                            vec![vec![text("t"), text("features"), Value::Int(0)]],
                        ),
                    ),
                    ("gpkg_spatial_ref_sys".into(), table("CREATE TABLE gpkg_spatial_ref_sys(srs_id INTEGER)", vec![vec![Value::Int(0)]])),
                    (
                        "gpkg_geometry_columns".into(),
                        table(
                            "CREATE TABLE gpkg_geometry_columns(table_name TEXT NOT NULL,column_name TEXT NOT NULL,geometry_type_name TEXT NOT NULL,srs_id INTEGER NOT NULL,z TINYINT NOT NULL,m TINYINT NOT NULL,PRIMARY KEY(table_name,column_name))",
                            vec![vec![text("t"), text("geom"), text("POINT"), Value::Int(0), Value::Int(0), Value::Int(0)]],
                        ),
                    ),
                    ("t".into(), table(&format!("CREATE TABLE t({declaration})"), vec![])),
                ]),
            };
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.features();
            assert_eq!(!checker.report.errors.iter().any(|error| error.requirement == 29), valid, "{declaration}");
        }
    }

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn feature_declarations_match_gdal() {
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let directory = tempfile::tempdir().unwrap();
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]); data=pathlib.Path(sys.argv[2]); base=root/'base.gpkg'
gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),format='GPKG',layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
cases=[]
def add(name,sql=''):
 path=root/(str(len(cases))+'.gpkg'); shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('PRAGMA ignore_check_constraints=ON')
  if sql: db.executescript(sql)
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(1,4,0); checker.extended_pragma_info=True
 with sqlite3.connect(path) as db: checker._check_features(db.cursor())
 # Rule 33 checks binary geometry SRIDs, a separate, unfinished rule group.
 cases.append(dict(name=name,path=str(path),requirements=sorted(e[0] for e in checker.errors if e[0] != 33)))
add('Natural Earth polygons')
add('empty feature table','DELETE FROM countries')
for axis in ['z','m']:
 for value in [-1,3]: add(axis+str(value),'UPDATE gpkg_geometry_columns SET '+axis+'='+str(value))
add('unknown SRS','UPDATE gpkg_geometry_columns SET srs_id=999999')
add('different existing SRS','UPDATE gpkg_geometry_columns SET srs_id=-1')
add('wrong geometry declaration',"UPDATE gpkg_geometry_columns SET geometry_type_name='GEOMETRY'")
add('invalid geometry declaration',"UPDATE gpkg_geometry_columns SET geometry_type_name='INVALID'")
add('geometry registered as attributes',"UPDATE gpkg_contents SET data_type='attributes'; INSERT INTO gpkg_contents(table_name,data_type,identifier) VALUES('other','features','other'); CREATE TABLE other(fid INTEGER PRIMARY KEY,geom POINT); INSERT INTO gpkg_geometry_columns VALUES('other','geom','POINT',0,0,0)")
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(cases.len(), 11);
        for case in cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let mut checker = Checker { db: Snapshot::read(&bytes).unwrap(), report: CoreReport::default() };
            checker.features();
            let mut actual: Vec<_> =
                checker.report.errors.iter().map(|error| error.requirement.number().expect("core requirement")).collect();
            actual.sort_unstable();
            let expected: Vec<u32> = serde_json::from_value(case["requirements"].clone()).unwrap();
            assert_eq!(actual, expected, "{}: {:?}", case["name"], checker.report.errors);
        }
    }
}
