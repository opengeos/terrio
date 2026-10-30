// SPDX-FileCopyrightText: Copyright (c) 2017, Even Rouault <even.rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     swig/python/gdal-utils/osgeo_utils/samples/validate_gpkg.py

//! GeoPackage R-tree declarations and maintenance-trigger inventory.
//! OGC GeoPackage requirements 75, 77 and 78, following GDAL 3.13.3
//! validate_gpkg.py (attribution in NOTICE). Node content checks are separate.
use super::*;
use std::collections::BTreeSet;

impl Checker {
    pub(super) fn spatial_indexes(&mut self) {
        let Some(registry) = self.db.tables.get("gpkg_geometry_columns") else { return };
        let Some(contents) = self.db.tables.get("gpkg_contents") else { return };
        for row in &registry.rows {
            let Some(name) = registry.get(row, "table_name").and_then(Value::as_str) else { continue };
            let Some(field) = registry.get(row, "column_name").and_then(Value::as_str) else { continue };
            if !contents.rows.iter().any(|row| {
                contents.get(row, "table_name").and_then(Value::as_str) == Some(name)
                    && contents.get(row, "data_type").and_then(Value::as_str) == Some("features")
            }) {
                continue;
            }
            let index = format!("rtree_{name}_{field}");
            let declared = self.db.tables.get("gpkg_extensions").is_some_and(|ext| {
                ext.rows.iter().any(|row| {
                    ext.get(row, "table_name").and_then(Value::as_str) == Some(name)
                        && ext.get(row, "column_name").and_then(Value::as_str) == Some(field)
                        && ext.get(row, "extension_name").and_then(Value::as_str) == Some("gpkg_rtree_index")
                        && ext.get(row, "scope").and_then(Value::as_str) == Some("write-only")
                })
            });
            if !self.db.object(&index) {
                self.report.require(!declared, 75, format!("{index}: declared R-tree object is missing"));
                continue;
            }
            self.report.require(declared, 78, format!("{index}: missing R-tree extension declaration"));
            if let Some((_, table)) = self.db.tables.iter().find(|(key, _)| key.eq_ignore_ascii_case(&index)) {
                self.report.require(table.schema.columns.len() == 5, 77, format!("{index}: expected five columns"));
                for (i, expected) in ["id", "minx", "maxx", "miny", "maxy"].iter().enumerate() {
                    let Some(column) = table.schema.columns.get(i) else { continue };
                    // SQLite's R-tree modules expose fixed affinities through
                    // PRAGMA table_info, regardless of argument type annotations.
                    let kind = match table.schema.virtual_module.as_deref() {
                        Some("rtree") => {
                            if i == 0 {
                                "INT"
                            } else {
                                "REAL"
                            }
                        }
                        Some("rtree_i32") => "INT",
                        _ => column.data_type.as_str(),
                    };
                    self.report.require(column.name == *expected, 77, format!("{index}: column {i} must be {expected}"));
                    self.report.require(
                        if i == 0 { matches!(kind, "" | "INT") } else { matches!(kind, "" | "NUM" | "REAL") },
                        77,
                        format!("{index}.{expected}: invalid type {kind}"),
                    );
                    self.report.require(!column.not_null, 77, format!("{index}.{expected}: unexpected NOT NULL"));
                    self.report.require(column.default.is_none(), 77, format!("{index}.{expected}: unexpected default"));
                    self.report.require(column.primary_key == 0, 77, format!("{index}.{expected}: unexpected primary key"));
                }
            } else {
                self.report.unsupported_checks.push(format!("R-tree view {index} still needs view-schema validation"));
            }
            let expected: &[u32] = if self.report.version.unwrap_or(10400) < 10400 { &[1, 2, 3, 4] } else { &[2, 4, 5, 6, 7] };
            let prefix = format!("{index}_update");
            let mut found = BTreeSet::new();
            for object in self.db.objects.iter().filter(|entry| entry.0 == "trigger") {
                if let Some(suffix) = object.1.strip_prefix(&prefix) {
                    let number = suffix.parse::<u32>().ok();
                    self.report.require(
                        number.is_some_and(|number| expected.contains(&number)),
                        75,
                        format!("{}: unexpected update trigger", object.1),
                    );
                    if let Some(number) = number {
                        found.insert(number);
                    }
                }
            }
            for number in expected {
                self.report.require(found.contains(number), 75, format!("{index}_update{number}: missing trigger"));
            }
            for action in ["insert", "delete"] {
                let trigger = format!("{index}_{action}");
                self.report.require(
                    self.db.objects.iter().any(|entry| entry.0 == "trigger" && entry.1 == trigger),
                    75,
                    format!("{trigger}: missing trigger"),
                );
            }
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn index_declarations_match_gdal() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);cases=[]
for version in ['1.2','1.4']:
 base=root/(version+'.gpkg')
 gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),layerName='countries',options=['-dsco','VERSION='+version]).Close()
 def add(name,sql='',bad_suffix=False):
  path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
  with sqlite3.connect(path) as db:
   if sql: db.executescript(sql)
  checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
  checker.version=tuple(int(v) for v in version.split('.'));checker.extended_pragma_info=True
  try:
   with sqlite3.connect(path) as db:checker._check_features(db.cursor())
  except ValueError:
   assert bad_suffix,name
  else: assert not bad_suffix,name
  requirements=sorted(e[0] for e in checker.errors if e[0] in (75,77,78))
  cases.append(dict(name=version+' '+name,path=str(path),version=int(float(version)*1000)+9000,requirements=requirements,bad_suffix=bad_suffix))
 add('real Natural Earth R-tree')
 add('missing declaration',"DELETE FROM gpkg_extensions WHERE extension_name='gpkg_rtree_index'")
 add('wrong extension scope',"UPDATE gpkg_extensions SET scope='read-write' WHERE extension_name='gpkg_rtree_index'")
 for action in ['insert','delete']+['update'+str(i) for i in ([1,2,3,4] if version=='1.2' else [2,4,5,6,7])]:
  add('missing '+action,'DROP TRIGGER rtree_countries_geom_'+action)
 add('unexpected update number','CREATE TRIGGER rtree_countries_geom_update8 AFTER UPDATE ON countries BEGIN SELECT 1; END')
 add('nonnumeric update suffix','CREATE TRIGGER rtree_countries_geom_update_bad AFTER UPDATE ON countries BEGIN SELECT 1; END',bad_suffix=True)
 for columns in ['id,minx,maxx,miny,maxy','id INT,minx NUM,maxx REAL,miny,maxy','id INTEGER,minx REAL,maxx REAL,miny REAL,maxy REAL','id,minx TEXT,maxx,miny,maxy','id,minx,maxx,miny,maxy,extra','id PRIMARY KEY,minx,maxx,miny,maxy','id,minx REAL NOT NULL,maxx,miny,maxy','id,minx REAL DEFAULT 0,maxx,miny,maxy']:
  add('physical index '+columns,'DROP TABLE rtree_countries_geom;CREATE TABLE rtree_countries_geom('+columns+')')
 add('integer virtual index','DROP TABLE rtree_countries_geom;CREATE VIRTUAL TABLE rtree_countries_geom USING rtree_i32(id,minx,maxx,miny,maxy)')
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 35);
        for case in cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let mut checker = Checker {
                db: Snapshot::read(&bytes).unwrap(),
                report: CoreReport { version: Some(case["version"].as_u64().unwrap() as u32), ..Default::default() },
            };
            checker.spatial_indexes();
            if case["bad_suffix"].as_bool() == Some(true) {
                assert!(checker.report.errors.iter().any(|e| e.requirement == 75), "{case}");
                continue;
            }
            let mut actual: Vec<_> = checker.report.errors.iter().map(|e| e.requirement.number().expect("core requirement")).collect();
            actual.sort_unstable();
            let expected: Vec<u32> = serde_json::from_value(case["requirements"].clone()).unwrap();
            assert_eq!(actual, expected, "{}: {:?}", case["name"], checker.report.errors);
        }
    }
}
