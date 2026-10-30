//! GeoPackage extension declarations and their metadata references.
use super::*;

fn valid_name(name: &str, version: u32) -> bool {
    if name.starts_with("gpkg_") {
        return [
            "gpkg_rtree_index",
            "gpkg_zoom_other",
            "gpkg_webp",
            "gpkg_metadata",
            "gpkg_schema",
            "gpkg_crs_wkt",
            "gpkg_crs_wkt_1_1",
            "gpkg_elevation_tiles",
            "gpkg_2d_gridded_coverage",
            "gpkg_related_tables",
        ]
        .contains(&name)
            || name.strip_prefix("gpkg_geom_").is_some_and(|kind| {
                ["CIRCULARSTRING", "COMPOUNDCURVE", "CURVEPOLYGON", "MULTICURVE", "MULTISURFACE", "CURVE", "SURFACE"].contains(&kind)
            })
            || version < 10200 && ["gpkg_geometry_type_trigger", "gpkg_srs_id_trigger"].contains(&name);
    }
    name.split_once('_').is_some_and(|(author, extension)| {
        !author.is_empty()
            && !extension.is_empty()
            && author.bytes().all(|value| value.is_ascii_alphanumeric())
            && extension.bytes().all(|value| value.is_ascii_alphanumeric() || value == b'_')
    })
}

impl Checker {
    pub(super) fn extensions(&mut self) {
        if !self.db.object("gpkg_extensions") {
            return;
        }
        self.structure(
            "gpkg_extensions",
            58,
            &[
                column("table_name", "TEXT", false, None, 0),
                column("column_name", "TEXT", false, None, 0),
                column("extension_name", "TEXT", true, None, 0),
                column("definition", "TEXT", true, None, 0),
                column("scope", "TEXT", true, None, 0),
            ],
            false,
        );
        let Some(table) = self.db.tables.get("gpkg_extensions") else { return };
        for row in &table.rows {
            let target = table.get(row, "table_name").and_then(Value::as_str);
            let field = table.get(row, "column_name").and_then(Value::as_str);
            self.report.require(
                target.is_some() || matches!(table.get(row, "table_name"), Some(Value::Null)),
                60,
                "extension table_name must be text or null",
            );
            self.report.require(
                field.is_some() || matches!(table.get(row, "column_name"), Some(Value::Null)),
                61,
                "extension column_name must be text or null",
            );
            if let Some(field) = field {
                if let Some(target) = target {
                    if let Some((_, referenced)) = self.db.tables.iter().find(|(name, _)| name.eq_ignore_ascii_case(target)) {
                        self.report.require(
                            referenced.schema.columns.iter().any(|column| column.name.eq_ignore_ascii_case(field)),
                            61,
                            format!("extension references missing column {target}.{field}"),
                        );
                    } else if self.db.object(target) {
                        // This extension rule has not evaluated the target view schema.
                        self.report
                            .unsupported_checks
                            .push(format!("extension view column {target}.{field} still needs view-schema validation"));
                    } else {
                        self.report.require(false, 61, format!("extension column references missing table {target}"));
                    }
                } else {
                    self.report.require(false, 61, "extension column_name requires table_name");
                }
            } else if let Some(target) = target {
                self.report.require(self.db.object(target), 60, format!("extension references missing table {target}"));
            }
            let name = table.get(row, "extension_name").and_then(Value::as_str);
            self.report.require(
                name.is_some_and(|name| valid_name(name, self.report.version.unwrap_or(10400))),
                62,
                "extension_name must use an allowed author/name or registered gpkg extension",
            );
            self.report.require(
                table.get(row, "definition").and_then(Value::as_str).is_some_and(|definition| !definition.trim().is_empty()),
                63,
                "extension definition must reference its defining document",
            );
            let scope = table.get(row, "scope").and_then(Value::as_str);
            self.report.require(matches!(scope, Some("read-write" | "write-only")), 64, "invalid extension scope");
            if name == Some("gpkg_rtree_index") {
                let features = target.is_some_and(|target| {
                    self.db.tables.get("gpkg_contents").is_some_and(|contents| {
                        contents.rows.iter().any(|row| {
                            contents.get(row, "table_name").and_then(Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case(target))
                                && contents.get(row, "data_type").and_then(Value::as_str) == Some("features")
                        })
                    })
                });
                self.report.require(features, 75, "rtree extension must reference a registered features table");
                self.report.require(scope == Some("write-only"), 75, "rtree extension scope must be write-only");
            }
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    #[test]
    fn unchecked_view_extension_columns_make_the_report_incomplete() {
        let mut extension = Table::from_sql("gpkg_extensions", "CREATE TABLE gpkg_extensions(table_name TEXT,column_name TEXT,extension_name TEXT NOT NULL,definition TEXT NOT NULL,scope TEXT NOT NULL,UNIQUE(table_name,column_name,extension_name))").unwrap();
        extension
            .rows
            .push(["v", "x", "ACME_test", "Extension Title: ACME test", "read-write"].into_iter().map(|s| Value::Text(s.into())).collect());
        let mut checker = Checker {
            db: Snapshot {
                unicode_case: false,
                objects: vec![("view".into(), "v".into(), "v".into(), 0, "CREATE VIEW v AS SELECT 1 AS x".into())],
                tables: BTreeMap::from([("gpkg_extensions".into(), extension)]),
            },
            report: CoreReport::default(),
        };
        checker.db.objects.push(("table".into(), "gpkg_extensions".into(), "gpkg_extensions".into(), 0, String::new()));
        checker.extensions();
        assert!(checker.report.errors.is_empty(), "{:?}", checker.report);
        assert!(checker.report.warnings.is_empty());
        assert_eq!(checker.report.unsupported_checks.len(), 1);
        assert!(!checker.report.is_valid());
    }
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn extension_declarations_match_gdal_with_documented_spec_corrections() {
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let directory = tempfile::tempdir().unwrap();
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]); data=pathlib.Path(sys.argv[2]); base=root/'base.gpkg'
source=gdal.OpenEx(str(data/'countries.gpkg'),gdal.OF_VECTOR)
name=source.GetLayer(0).GetName().replace('"','""')
gdal.VectorTranslate(str(base),source,format='GPKG',layerName='countries',SQLStatement='SELECT * FROM "'+name+'" LIMIT 1',SQLDialect='SQLITE',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
source=None
ddl='CREATE TABLE gpkg_extensions(table_name TEXT,column_name TEXT,extension_name TEXT NOT NULL,definition TEXT NOT NULL,scope TEXT NOT NULL,UNIQUE(table_name,column_name,extension_name))'
cases=[]
def add(label,rows=(),*,version=10400,schema=ddl,delta=0):
 path=root/(str(len(cases))+'.gpkg'); shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('DROP TABLE IF EXISTS gpkg_extensions')
  if schema:
   db.execute(schema)
   db.executemany('INSERT INTO gpkg_extensions(table_name,column_name,extension_name,definition,scope) VALUES(?,?,?,?,?)',rows)
  db.execute('PRAGMA user_version='+str(version))
  db.execute('PRAGMA application_id='+str(0x47503131 if version<10200 else 0x47504b47))
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(version//10000,(version//100)%100,version%100); checker.extended_pragma_info=True
 with sqlite3.connect(path) as db: checker._check_gpkg_extensions(db.cursor())
 cases.append(dict(name=label,path=str(path),errors=len(checker.errors),delta=delta))
def row(name='ACME_test',table=None,column=None,definition='Extension Title: ACME test',scope='read-write'):
 return (table,column,name,definition,scope)
add('no extension table',schema=None)
for name in ['ACME_test','A0_abc_12','gpkg_zoom_other','gpkg_webp','gpkg_metadata','gpkg_schema','gpkg_crs_wkt','gpkg_crs_wkt_1_1','gpkg_elevation_tiles','gpkg_2d_gridded_coverage','gpkg_related_tables','related_tables']:
 add('valid '+name,[row(name)])
for name in GPKGChecker.EXT_GEOM_TYPES: add('curve '+name,[row('gpkg_geom_'+name)])
for name in ['gpkg_geometry_type_trigger','gpkg_srs_id_trigger']:
 for version in [10100,10200]: add(name+' '+str(version),[row(name)],version=version)
for name in ['noseparator','gpkg_unregistered','bad-author_test','author_bad-name','acmé_test']:
 add('invalid name '+name,[row(name)])
for name in ['_missing_author','author_']:
 add('empty name component '+name,[row(name)],delta=1)
add('case insensitive table and column',[row(table='COUNTRIES',column='GEOM')])
add('missing column',[row(table='countries',column='missing')])
add('missing table with column',[row(table='missing',column='geom')])
add('missing table without column',[row(table='missing')],delta=1)
add('column without table',[row(column='geom')],delta=1)
for scope in ['READ-WRITE','readonly','']:
 add('invalid scope '+repr(scope),[row(scope=scope)])
add('valid rtree registration',[row('gpkg_rtree_index','countries','geom',scope='write-only')])
add('rtree global registration',[row('gpkg_rtree_index',scope='write-only')])
add('rtree wrong scope',[row('gpkg_rtree_index','countries','geom')])
for definition in ['', '   ']: add('empty definition '+repr(definition),[row(definition=definition)],delta=1)
add('wrong column type',schema=ddl.replace('column_name TEXT','column_name INTEGER'))
add('extra column',schema=ddl.replace(',UNIQUE',',extra TEXT,UNIQUE'))
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() >= 40);
        for case in cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let report = super::super::check_core(&bytes);
            let errors =
                report.errors.iter().filter(|error| matches!(error.requirement.number(), Some(58 | 60 | 61 | 62 | 63 | 64 | 75))).count();
            assert_eq!(errors as u64, case["errors"].as_u64().unwrap() + case["delta"].as_u64().unwrap(), "{case}: {report:?}");
        }
    }
}
