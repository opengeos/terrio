//! GeoPackage metadata and metadata references, requirements 93–102 and 140.
//! Rules follow OGC GeoPackage 1.4 and GDAL 3.13.3 validate_gpkg.py; see NOTICE.
use super::*;
use std::collections::BTreeSet;

const SCOPES: &[&str] = &[
    "undefined",
    "fieldSession",
    "collectionSession",
    "series",
    "dataset",
    "featureType",
    "feature",
    "attributeType",
    "attribute",
    "tile",
    "model",
    "catalog",
    "schema",
    "taxonomy",
    "software",
    "service",
    "collectionHardware",
    "nonGeographicDataset",
    "dimensionGroup",
];

/// SQLite INTEGER equality must not truncate a fractional REAL identifier.
pub(super) fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Int(value) => Some(*value),
        Value::Real(value) if value.fract() == 0.0 && *value >= i64::MIN as f64 && *value < 9_223_372_036_854_775_808.0 => {
            Some(*value as i64)
        }
        _ => None,
    }
}

fn row_identifier(column: &schema::Column, value: &Value) -> Option<i64> {
    let kind = &column.data_type;
    if !kind.contains("INT") && ["CHAR", "CLOB", "TEXT"].iter().any(|text| kind.contains(text)) {
        let Value::Text(text) = value else { return None };
        let id = text.parse::<i64>().ok()?;
        // A numeric parameter compared to a TEXT-affinity column is converted
        // to its canonical text form: '01' does not equal integer 1.
        return (text == &id.to_string()).then_some(id);
    }
    integer(value).or_else(|| {
        if !kind.is_empty()
            && !kind.contains("BLOB")
            && let Value::Text(text) = value
        {
            text.trim().parse::<i64>().ok().or_else(|| text.trim().parse::<f64>().ok().and_then(|number| integer(&Value::Real(number))))
        } else {
            None
        }
    })
}

impl Checker {
    pub(super) fn metadata(&mut self, bytes: &[u8]) -> Result<()> {
        let declared = self.db.tables.get("gpkg_extensions").is_some_and(|table| {
            table.rows.iter().any(|row| table.get(row, "extension_name").and_then(Value::as_str) == Some("gpkg_metadata"))
        });
        if declared {
            let extensions = &self.db.tables["gpkg_extensions"];
            for row in
                extensions.rows.iter().filter(|row| extensions.get(row, "extension_name").and_then(Value::as_str) == Some("gpkg_metadata"))
            {
                self.report.require(
                    extensions.get(row, "scope").and_then(Value::as_str) == Some("read-write"),
                    140,
                    "gpkg_metadata extension must have read-write scope",
                );
            }
        }
        if !self.db.object("gpkg_metadata") {
            self.report.require(!declared, 140, "declared gpkg_metadata table is missing");
            return Ok(());
        }
        self.structure(
            "gpkg_metadata",
            93,
            &[
                column("id", "INTEGER", true, None, 1),
                column("md_scope", "TEXT", true, Some("'dataset'"), 0),
                column("md_standard_uri", "TEXT", true, None, 0),
                column("mime_type", "TEXT", true, Some("'text/xml'"), 0),
                column("metadata", "TEXT", true, Some("''"), 0),
            ],
            false,
        );
        self.structure(
            "gpkg_metadata_reference",
            95,
            &[
                column("reference_scope", "TEXT", true, None, 0),
                column("table_name", "TEXT", false, None, 0),
                column("column_name", "TEXT", false, None, 0),
                column("row_id_value", "INTEGER", false, None, 0),
                column("timestamp", "DATETIME", true, Some("strftime('%Y-%m-%dT%H:%M:%fZ','now')"), 0),
                column("md_file_id", "INTEGER", true, None, 0),
                column("md_parent_id", "INTEGER", false, None, 0),
            ],
            false,
        );
        let Some(metadata) = self.db.tables.get("gpkg_metadata") else { return Ok(()) };
        let mut invalid_scopes = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for row in &metadata.rows {
            if let Some(id) = metadata.get(row, "id").and_then(integer) {
                ids.insert(id);
            }
            let scope = metadata.get(row, "md_scope").and_then(Value::as_str);
            if !scope.is_some_and(|scope| SCOPES.contains(&scope)) && invalid_scopes.insert(scope) {
                self.report.require(false, 94, format!("invalid metadata md_scope {scope:?}"));
            }
        }
        let Some(references) = self.db.tables.get("gpkg_metadata_reference") else { return Ok(()) };
        let mut pending = Vec::new();
        let mut wanted: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        let mut bad_reference_scopes = BTreeSet::new();
        for (index, row) in references.rows.iter().enumerate() {
            let scope = references.get(row, "reference_scope").and_then(Value::as_str);
            if !matches!(scope, Some("geopackage" | "table" | "column" | "row" | "row/col")) && bad_reference_scopes.insert(scope) {
                self.report.require(false, 96, format!("invalid metadata reference_scope {scope:?}"));
            }
            let name = references.get(row, "table_name").and_then(Value::as_str);
            let field = references.get(row, "column_name").and_then(Value::as_str);
            let row_id = references.get(row, "row_id_value");
            if scope == Some("geopackage") {
                self.report.require(
                    matches!(references.get(row, "table_name"), Some(Value::Null)),
                    97,
                    "geopackage metadata reference must have null table_name",
                );
            } else {
                self.report.require(name.is_some(), 97, "metadata reference requires table_name");
                let registered = name.is_some_and(|name| {
                    self.db.tables.get("gpkg_contents").is_some_and(|table| {
                        table.rows.iter().any(|row| table.get(row, "table_name").and_then(Value::as_str) == Some(name))
                    })
                });
                self.report.require(registered, 97, "metadata reference table is not registered in gpkg_contents");
            }
            if matches!(scope, Some("geopackage" | "table" | "row")) {
                self.report.require(
                    matches!(references.get(row, "column_name"), Some(Value::Null)),
                    98,
                    "metadata reference column_name must be null for this scope",
                );
            } else {
                self.report.require(field.is_some(), 98, "metadata column reference requires column_name");
                if let Some((_, table)) = name.and_then(|name| self.db.tables.iter().find(|(key, _)| key.eq_ignore_ascii_case(name))) {
                    self.report.require(
                        field.is_some_and(|field| table.schema.columns.iter().any(|column| column.name.eq_ignore_ascii_case(field))),
                        98,
                        "metadata reference column does not exist",
                    );
                } else if name.is_some_and(|name| self.db.object(name)) {
                    self.report.warnings.push(format!("metadata reference to view {name:?} still needs view-column validation"));
                } else {
                    self.report.require(false, 98, "metadata reference column does not exist");
                }
            }
            if matches!(scope, Some("geopackage" | "table" | "column")) {
                self.report.require(matches!(row_id, Some(Value::Null)), 99, "metadata reference row_id_value must be null for this scope");
            } else {
                self.report.require(!matches!(row_id, None | Some(Value::Null)), 99, "metadata row reference requires row_id_value");
                let id = row_id.and_then(integer);
                if let Some((key, _)) = name.and_then(|name| self.db.tables.iter().find(|(key, _)| key.eq_ignore_ascii_case(name))) {
                    if let Some(id) = id {
                        wanted.entry(key.clone()).or_default().insert(id);
                        pending.push((index, key.clone(), id));
                    } else {
                        self.report.require(false, 99, "metadata reference row does not exist");
                    }
                } else if name.is_some_and(|name| self.db.object(name)) {
                    self.report.warnings.push(format!("metadata reference to view {name:?} still needs view-row validation"));
                } else {
                    self.report.require(false, 99, "metadata reference row does not exist");
                }
            }
            self.report.require(
                references.get(row, "timestamp").and_then(Value::as_str).is_some_and(valid_timestamp),
                100,
                "invalid metadata reference timestamp",
            );
            let file_id = references.get(row, "md_file_id").and_then(integer);
            self.report.require(file_id.is_some_and(|id| ids.contains(&id)), 101, "metadata md_file_id has no metadata record");
            if !matches!(references.get(row, "md_parent_id"), Some(Value::Null)) {
                let parent_id = references.get(row, "md_parent_id").and_then(integer);
                self.report.require(parent_id.is_some_and(|id| ids.contains(&id)), 102, "metadata md_parent_id has no metadata record");
                self.report.require(
                    references.get(row, "md_file_id") != references.get(row, "md_parent_id"),
                    102,
                    "metadata file cannot be its own parent",
                );
            }
        }
        if !pending.is_empty() {
            let encoding = u32::from_be_bytes(
                bytes.get(56..60).and_then(|v| v.try_into().ok()).ok_or_else(|| Error::InvalidData("truncated SQLite header".into()))?,
            );
            let mut found = BTreeSet::new();
            sqlite_repack::inspect(bytes, |entry, rowid, record| {
                let Some(ids) = wanted.get(&entry.1) else { return Ok(()) };
                let table = &self.db.tables[&entry.1];
                // A column named ROWID shadows SQLite's hidden rowid. Preserve
                // SQLite lookup semantics without retaining the user's rows.
                let id = if let Some(index) = table.schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case("rowid")) {
                    table
                        .decode(&entry.1, record, rowid, encoding)?
                        .get(index)
                        .and_then(|value| row_identifier(&table.schema.columns[index], value))
                } else {
                    rowid
                };
                if let Some(id) = id
                    && ids.contains(&id)
                {
                    found.insert((entry.1.clone(), id));
                }
                Ok(())
            })?;
            for (index, name, id) in pending {
                self.report.require(found.contains(&(name, id)), 99, format!("metadata reference {index}: row does not exist"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_never_truncate_fractional_or_out_of_range_values() {
        for value in [Value::Real(1.5), Value::Real(f64::NAN), Value::Real(f64::INFINITY), Value::Real(9_223_372_036_854_775_808.0)] {
            assert_eq!(integer(&value), None, "{value:?}");
        }
        assert_eq!(integer(&Value::Int(i64::MAX)), Some(i64::MAX));
        assert_eq!(integer(&Value::Real(i64::MIN as f64)), Some(i64::MIN));
        assert_eq!(integer(&Value::Real(-1.0)), Some(-1));
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn metadata_rules_match_gdal_with_documented_extension_scope_correction() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);base=root/'base.gpkg';cases=[]
ds=gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO'])
ds.SetMetadataItem('TERRIO_TEST','Natural Earth metadata validation');ds.Close()
with sqlite3.connect(base) as db:
 assert db.execute('SELECT count(*) FROM gpkg_metadata_reference').fetchone()[0]==2
 db.execute("CREATE TABLE shadow(ROWID TEXT, note TEXT)")
 db.executemany('INSERT INTO shadow VALUES(?,?)',[('21','canonical'),('022','noncanonical')])
 db.execute("INSERT INTO gpkg_contents(table_name,data_type,identifier) VALUES('shadow','attributes','shadow')")
 fid=db.execute('SELECT fid FROM countries LIMIT 1').fetchone()[0]
def add(name,sql='',extra=(),reference_exception=False):
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('PRAGMA ignore_check_constraints=ON')
  if sql:db.executescript(sql)
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(1,4,0);checker.extended_pragma_info=True
 try:
  with sqlite3.connect(path) as db:checker._check_metadata(db.cursor())
 except (sqlite3.OperationalError,AttributeError):
  assert reference_exception,name
 else:assert not reference_exception,name
 cases.append(dict(name=name,path=str(path),requirements=sorted([e[0] for e in checker.errors]+list(extra)),reference_exception=reference_exception))
add('GDAL-written Natural Earth metadata')
add('no metadata extension','DROP TABLE gpkg_metadata;DROP TABLE gpkg_metadata_reference;DELETE FROM gpkg_extensions')
add('empty metadata tables','DELETE FROM gpkg_metadata_reference;DELETE FROM gpkg_metadata')
for scope in ['undefined','fieldSession','collectionSession','series','dataset','featureType','feature','attributeType','attribute','tile','model','catalog','schema','taxonomy','software','service','collectionHardware','nonGeographicDataset','dimensionGroup','invalid']:
 add('metadata scope '+scope,"UPDATE gpkg_metadata SET md_scope='"+scope+"'")
add('missing declared metadata','DROP TABLE gpkg_metadata')
add('missing reference table','DROP TABLE gpkg_metadata_reference',reference_exception=True)
add('wrong first extension scope',"UPDATE gpkg_extensions SET scope='write-only' WHERE table_name='gpkg_metadata'")
# GDAL checks only the first declaration; both metadata tables require read-write.
add('wrong second extension scope',"UPDATE gpkg_extensions SET scope='write-only' WHERE table_name='gpkg_metadata_reference'",extra=[140])
add('invalid reference scope',"UPDATE gpkg_metadata_reference SET reference_scope='invalid'",reference_exception=True)
add('null table reference',"UPDATE gpkg_metadata_reference SET table_name=NULL WHERE reference_scope='table'")
add('unregistered table reference',"UPDATE gpkg_metadata_reference SET table_name='missing' WHERE reference_scope='table'")
add('geopackage with table',"UPDATE gpkg_metadata_reference SET table_name='countries' WHERE reference_scope='geopackage'")
add('table with column',"UPDATE gpkg_metadata_reference SET column_name='geom' WHERE reference_scope='table'")
add('table with row',"UPDATE gpkg_metadata_reference SET row_id_value=1 WHERE reference_scope='table'")
for field in ['geom','GEOM','missing',None]:
 add('column reference '+str(field),"UPDATE gpkg_metadata_reference SET reference_scope='column',column_name="+("NULL" if field is None else "'"+field+"'")+" WHERE reference_scope='table'",reference_exception=field is None)
for rowid in [fid,-1,999999,None,1.5]:
 for scope in ['row','row/col']:
  add(scope+' '+str(rowid),"UPDATE gpkg_metadata_reference SET reference_scope='"+scope+"',column_name="+("'geom'" if scope=='row/col' else 'NULL')+",row_id_value="+('NULL' if rowid is None else str(rowid))+" WHERE reference_scope='table'")
for rowid in [21,22]:
 add('shadowed text rowid '+str(rowid),"UPDATE gpkg_metadata_reference SET reference_scope='row',table_name='shadow',row_id_value="+str(rowid)+" WHERE reference_scope='table'")
for timestamp in ['2024-02-29T12:01:02.123456Z','2023-02-29T12:01:02.123Z','2024-01-01T24:00:00.000Z','2024-01-01T12:00:00Z','2024-01-01T12:00:00.1234567Z','yesterday']:
 add('timestamp '+timestamp,"UPDATE gpkg_metadata_reference SET timestamp='"+timestamp+"'")
add('missing file ID','UPDATE gpkg_metadata_reference SET md_file_id=999999')
add('fractional file ID','UPDATE gpkg_metadata_reference SET md_file_id=1.5')
add('missing parent ID','UPDATE gpkg_metadata_reference SET md_parent_id=999999')
add('fractional parent ID','UPDATE gpkg_metadata_reference SET md_parent_id=1.5')
add('self parent ID','UPDATE gpkg_metadata_reference SET md_parent_id=md_file_id')
add('valid parent ID','UPDATE gpkg_metadata_reference SET md_parent_id=2 WHERE md_file_id=1')
add('wrong metadata schema type',"PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'mime_type TEXT','mime_type BLOB') WHERE name='gpkg_metadata'")
add('wrong reference schema default',"PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'''now''','''then''') WHERE name='gpkg_metadata_reference'")
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 60);
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let db = Snapshot::read(&bytes).unwrap();
            assert!(db.tables["countries"].rows.is_empty());
            assert!(db.tables["shadow"].rows.is_empty());
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.metadata(&bytes).unwrap();
            if case["reference_exception"].as_bool() == Some(true) {
                assert!(!checker.report.errors.is_empty(), "{case}");
                continue;
            }
            let mut actual: Vec<_> = checker.report.errors.iter().map(|e| e.requirement.number().expect("core requirement")).collect();
            actual.sort_unstable();
            let expected: Vec<u32> = serde_json::from_value(case["requirements"].clone()).unwrap();
            assert_eq!(actual, expected, "{}: {:?}", case["name"], checker.report.errors);
        }
        eprintln!("{} metadata comparison cases passed", cases.len());
    }
}
