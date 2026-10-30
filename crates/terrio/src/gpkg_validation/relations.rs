// SPDX-FileCopyrightText: Copyright (c) 2017, Even Rouault <even.rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     swig/python/gdal-utils/osgeo_utils/samples/validate_gpkg.py

//! Related Tables extension declarations and table/column references.
//! Rules follow GDAL 3.13.3 validate_gpkg.py (see NOTICE). Its unnumbered
//! diagnostics are grouped under the registered gpkg_related_tables extension.
use super::*;

const RULE: Requirement = Requirement::Extension("gpkg_related_tables");
fn related(name: Option<&str>) -> bool {
    matches!(name, Some("related_tables" | "gpkg_related_tables"))
}

impl Checker {
    pub(super) fn relations(&mut self) {
        let extensions = self.db.tables.get("gpkg_extensions");
        let declarations: Vec<_> = extensions
            .map(|table| {
                table
                    .rows
                    .iter()
                    .filter(|row| related(table.get(row, "extension_name").and_then(Value::as_str)))
                    .map(|row| {
                        (
                            table.get(row, "table_name").and_then(Value::as_str),
                            table.get(row, "scope").and_then(Value::as_str),
                            table.get(row, "column_name"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !declarations.is_empty() {
            for (name, scope, field) in &declarations {
                self.report.require_rule(matches!(field, Some(Value::Null)), RULE, "related-tables declaration column_name must be null");
                self.report.require_rule(
                    *scope == Some("read-write"),
                    RULE,
                    format!("related-tables declaration for {name:?} must have read-write scope"),
                );
            }
            self.report.require_rule(
                declarations.iter().any(|(name, _, _)| name.is_some_and(|name| name.eq_ignore_ascii_case("gpkgext_relations"))),
                RULE,
                "missing gpkgext_relations extension declaration",
            );
            self.report.require_rule(
                declarations
                    .iter()
                    .filter_map(|(name, _, _)| name.map(str::to_ascii_lowercase))
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    > 1,
                RULE,
                "related-tables extension must declare at least one mapping table",
            );
        }
        if !self.db.object("gpkgext_relations") {
            self.report.require_rule(declarations.is_empty(), RULE, "declared gpkgext_relations table is missing");
            return;
        }
        self.structure_rule(
            "gpkgext_relations",
            RULE,
            &[
                column("id", "INTEGER", true, None, 1),
                column("base_table_name", "TEXT", true, None, 0),
                column("base_primary_column", "TEXT", true, Some("'id'"), 0),
                column("related_table_name", "TEXT", true, None, 0),
                column("related_primary_column", "TEXT", true, Some("'id'"), 0),
                column("relation_name", "TEXT", true, None, 0),
                column("mapping_table_name", "TEXT", true, None, 0),
            ],
            false,
        );
        let Some(table) = self.db.tables.get("gpkgext_relations") else { return };
        // Collect the small relation registry so declaration checks can use the
        // same view-aware reference helper as mapping-table column checks.
        let references: Vec<_> = table
            .rows
            .iter()
            .map(|row| {
                [
                    "base_table_name",
                    "base_primary_column",
                    "related_table_name",
                    "related_primary_column",
                    "mapping_table_name",
                    "relation_name",
                ]
                .map(|field| table.get(row, field).and_then(Value::as_str).map(str::to_owned))
            })
            .collect();
        for [base, base_id, other, other_id, mapping, relation] in references {
            let valid_relation = relation.as_deref().is_some_and(|name| {
                ["media", "simple_attributes", "features", "attributes", "tiles"].contains(&name)
                    || name
                        .strip_prefix("x-")
                        .and_then(|name| name.split_once('_'))
                        .is_some_and(|(author, relation)| !author.is_empty() && !relation.is_empty())
            });
            if relation.is_some() {
                self.report.require_rule(valid_relation, RULE, format!("invalid related-tables relation name {relation:?}"));
            }
            for (name, field) in [(&base, &base_id), (&other, &other_id)] {
                let exists = name.as_deref().is_some_and(|name| self.db.object(name));
                self.report.require_rule(exists, RULE, format!("related-tables target {name:?} does not exist"));
                if exists {
                    self.relation_column(name.as_deref(), field.as_deref());
                }
            }
            self.report.require_rule(
                mapping.as_deref().is_some_and(|name| self.db.object(name)),
                RULE,
                format!("related-tables mapping table {mapping:?} does not exist"),
            );
            let declared = mapping.as_deref().is_some_and(|name| {
                self.db.tables.get("gpkg_extensions").is_some_and(|ext| {
                    ext.rows.iter().any(|row| {
                        related(ext.get(row, "extension_name").and_then(Value::as_str))
                            && ext.get(row, "table_name").and_then(Value::as_str).is_some_and(|value| value.eq_ignore_ascii_case(name))
                    })
                })
            });
            self.report.require_rule(declared, RULE, format!("mapping table {mapping:?} has no related-tables extension declaration"));
            for column in ["base_id", "related_id"] {
                self.relation_column(mapping.as_deref(), Some(column));
            }
        }
    }

    fn relation_column(&mut self, name: Option<&str>, field: Option<&str>) {
        if let Some((_, table)) = name.and_then(|name| self.db.tables.iter().find(|(key, _)| key.eq_ignore_ascii_case(name))) {
            self.report.require_rule(
                field.is_some_and(|field| table.schema.columns.iter().any(|column| column.name.eq_ignore_ascii_case(field))),
                RULE,
                format!("related-tables column {name:?}.{field:?} does not exist"),
            );
        } else if name.is_some_and(|name| self.db.object(name)) {
            self.report
                .unsupported_checks
                .push(format!("related-tables reference to view {name:?}.{field:?} still needs view-schema validation"));
        } else {
            self.report.require_rule(false, RULE, format!("related-tables column {name:?}.{field:?} does not exist"));
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn related_table_rules_match_gdal() {
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
 db.executescript('''
 CREATE TABLE IF NOT EXISTS gpkg_extensions(table_name TEXT,column_name TEXT,extension_name TEXT NOT NULL,definition TEXT NOT NULL,scope TEXT NOT NULL,UNIQUE(table_name,column_name,extension_name));
 CREATE TABLE gpkgext_relations(id INTEGER PRIMARY KEY,base_table_name TEXT NOT NULL,base_primary_column TEXT NOT NULL DEFAULT 'id',related_table_name TEXT NOT NULL,related_primary_column TEXT NOT NULL DEFAULT 'id',relation_name TEXT NOT NULL,mapping_table_name TEXT NOT NULL);
 CREATE TABLE documents(id INTEGER PRIMARY KEY,title TEXT);
 CREATE TABLE country_documents(base_id INTEGER NOT NULL,related_id INTEGER NOT NULL);
 INSERT INTO documents VALUES(1,'Natural Earth source');
 INSERT INTO country_documents SELECT fid,1 FROM countries;
 INSERT INTO gpkg_extensions VALUES('gpkgext_relations',NULL,'gpkg_related_tables','http://www.geopackage.org/spec/#extension_related_tables','read-write');
 INSERT INTO gpkg_extensions VALUES('country_documents',NULL,'gpkg_related_tables','http://www.geopackage.org/spec/#extension_related_tables','read-write');
 INSERT INTO gpkgext_relations(base_table_name,base_primary_column,related_table_name,relation_name,mapping_table_name) VALUES('countries','fid','documents','attributes','country_documents');
 ''')
def add(name,sql='',reference_exception=None):
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  if sql:db.executescript(sql)
  assert db.execute('PRAGMA integrity_check').fetchone()[0]=='ok',name
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(1,4,0);checker.extended_pragma_info=True
 try:
  with sqlite3.connect(path) as db:checker._check_relations(db.cursor())
 except (sqlite3.OperationalError,AttributeError) as error:
  assert type(error).__name__==reference_exception,(name,type(error).__name__)
 else:assert reference_exception is None,name
 assert all(e[0] is None for e in checker.errors),name
 cases.append(dict(name=name,path=str(path),errors=len(checker.errors),reference_exception=reference_exception))
add('real Natural Earth relationships')
add('legacy extension name',"UPDATE gpkg_extensions SET extension_name='related_tables'")
add('mixed extension names',"UPDATE gpkg_extensions SET extension_name='related_tables' WHERE table_name='country_documents'")
add('empty relation registry','DELETE FROM gpkgext_relations')
add('no extension','DROP TABLE gpkgext_relations;DELETE FROM gpkg_extensions')
add('missing declared registry','DROP TABLE gpkgext_relations')
add('no registry declaration',"DELETE FROM gpkg_extensions WHERE table_name='gpkgext_relations'")
add('no mapping declaration',"DELETE FROM gpkg_extensions WHERE table_name='country_documents'")
add('wrong scope',"UPDATE gpkg_extensions SET scope='write-only'")
for field in ['base_table_name','related_table_name','mapping_table_name','base_primary_column','related_primary_column']:
 add('missing '+field,"UPDATE gpkgext_relations SET "+field+"='missing'")
for field in ['base_primary_column','related_primary_column']:
 add('column case '+field,'UPDATE gpkgext_relations SET '+field+'=upper('+field+')')
for col in ['base_id','related_id']:
 add('missing mapping '+col,'ALTER TABLE country_documents RENAME COLUMN '+col+' TO missing')
add('mapping missing both columns','DROP TABLE country_documents;CREATE TABLE country_documents(wrong TEXT)')
add('quoted mapping name',"ALTER TABLE country_documents RENAME TO \"country's documents\";UPDATE gpkgext_relations SET mapping_table_name='country''s documents';UPDATE gpkg_extensions SET table_name='country''s documents' WHERE table_name='country_documents'")
add('wrong registry schema type',"PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'relation_name TEXT','relation_name BLOB') WHERE name='gpkgext_relations'")
add('wrong registry schema default',"PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,\"DEFAULT 'id'\",\"DEFAULT 'other'\") WHERE name='gpkgext_relations'")
add('missing registry column','ALTER TABLE gpkgext_relations DROP COLUMN relation_name')
add('extra registry column','ALTER TABLE gpkgext_relations ADD COLUMN note TEXT')
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 20);
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let db = Snapshot::read(&bytes).unwrap();
            for name in ["countries", "documents", "country_documents"] {
                if let Some(table) = db.tables.get(name) {
                    assert!(table.rows.is_empty());
                }
            }
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.relations();
            assert!(checker.report.errors.iter().all(|e| e.requirement == RULE));
            if !case["reference_exception"].is_null() {
                assert!(!checker.report.is_valid(), "{case}");
                continue;
            }
            assert_eq!(
                checker.report.errors.len() as u64,
                case["errors"].as_u64().unwrap(),
                "{}: {:?}",
                case["name"],
                checker.report.errors
            );
        }
        eprintln!("{} related-table comparison cases passed", cases.len());
    }
}
