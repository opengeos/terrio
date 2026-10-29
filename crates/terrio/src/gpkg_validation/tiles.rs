//! Tile pyramid metadata and streamed user-tile checks.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone)]
struct Matrix {
    zoom: i64,
    width: i64,
    height: i64,
    tile_width: i64,
    tile_height: i64,
    pixel_x: f64,
    pixel_y: f64,
}
impl Matrix {
    fn read(table: &Table, row: &[Value]) -> Option<Self> {
        Some(Self {
            zoom: table.get(row, "zoom_level")?.as_i64()?,
            width: table.get(row, "matrix_width")?.as_i64()?,
            height: table.get(row, "matrix_height")?.as_i64()?,
            tile_width: table.get(row, "tile_width")?.as_i64()?,
            tile_height: table.get(row, "tile_height")?.as_i64()?,
            pixel_x: table.get(row, "pixel_x_size")?.as_f64()?,
            pixel_y: table.get(row, "pixel_y_size")?.as_f64()?,
        })
    }
}
struct Pyramid {
    name: String,
    coverage: bool,
    matrices: Vec<Matrix>,
    coordinate_errors: BTreeSet<(u32, Option<i64>)>,
    webp: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Encoding {
    Png,
    Jpeg,
    Webp,
    Tiff,
}
/// Classify signatures only. Complete image decoding remains a separate check.
fn encoding(blob: &[u8]) -> Option<Encoding> {
    if blob.len() < 12 {
        None
    } else if blob.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Encoding::Png)
    } else if blob.starts_with(b"\xff\xd8\xff") {
        Some(Encoding::Jpeg)
    } else if blob.starts_with(b"RIFF") && &blob[8..12] == b"WEBP" {
        Some(Encoding::Webp)
    } else if blob.starts_with(b"II*\0") || blob.starts_with(b"MM\0*") {
        Some(Encoding::Tiff)
    } else {
        None
    }
}

impl Checker {
    fn tile_extension(&self, name: &str, extension: &str, scope: Option<&str>) -> bool {
        self.db.tables.get("gpkg_extensions").is_some_and(|table| {
            table.rows.iter().any(|row| {
                table.get(row, "table_name").and_then(Value::as_str) == Some(name)
                    && table.get(row, "column_name").and_then(Value::as_str) == Some("tile_data")
                    && table.get(row, "extension_name").and_then(Value::as_str) == Some(extension)
                    && scope.is_none_or(|scope| table.get(row, "scope").and_then(Value::as_str) == Some(scope))
            })
        })
    }

    pub(super) fn tiles(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(contents) = self.db.tables.get("gpkg_contents") else { return Ok(()) };
        let registered: Vec<_> = contents
            .rows
            .iter()
            .filter_map(|row| {
                let kind = contents.get(row, "data_type")?.as_str()?;
                matches!(kind, "tiles" | "2d-gridded-coverage")
                    .then(|| contents.get(row, "table_name")?.as_str().map(|name| (name.to_string(), kind == "2d-gridded-coverage")))?
            })
            .collect();
        if registered.is_empty() {
            return Ok(());
        }
        self.structure(
            "gpkg_tile_matrix_set",
            38,
            &[
                column("table_name", "TEXT", true, None, 1),
                column("srs_id", "INTEGER", true, None, 0),
                column("min_x", "DOUBLE", true, None, 0),
                column("min_y", "DOUBLE", true, None, 0),
                column("max_x", "DOUBLE", true, None, 0),
                column("max_y", "DOUBLE", true, None, 0),
            ],
            false,
        );
        self.structure(
            "gpkg_tile_matrix",
            42,
            &[
                column("table_name", "TEXT", true, None, 1),
                column("zoom_level", "INTEGER", true, None, 2),
                column("matrix_width", "INTEGER", true, None, 0),
                column("matrix_height", "INTEGER", true, None, 0),
                column("tile_width", "INTEGER", true, None, 0),
                column("tile_height", "INTEGER", true, None, 0),
                column("pixel_x_size", "DOUBLE", true, None, 0),
                column("pixel_y_size", "DOUBLE", true, None, 0),
            ],
            false,
        );
        if let Some(table) = self.db.tables.get("gpkg_tile_matrix_set") {
            for row in &table.rows {
                let name = table.get(row, "table_name").and_then(Value::as_str);
                self.report.require(
                    registered.iter().any(|(registered, _)| Some(registered.as_str()) == name),
                    39,
                    "unregistered tile matrix set",
                );
                self.report.require(
                    table.get(row, "srs_id").and_then(Value::as_i64).is_some_and(|id| self.db.srs(id)),
                    41,
                    "tile matrix set references missing SRS",
                );
            }
        }
        let mut matrices: BTreeMap<String, Vec<Matrix>> = BTreeMap::new();
        if let Some(table) = self.db.tables.get("gpkg_tile_matrix") {
            for row in &table.rows {
                let name = table.get(row, "table_name").and_then(Value::as_str);
                self.report.require(
                    registered.iter().any(|(registered, _)| Some(registered.as_str()) == name),
                    43,
                    "unregistered tile matrix",
                );
                self.report.require(
                    table.get(row, "zoom_level").and_then(Value::as_i64).is_some_and(|n| n >= 0),
                    46,
                    "negative or noninteger tile matrix zoom",
                );
                for (field, requirement) in [("matrix_width", 47), ("matrix_height", 48), ("tile_width", 49), ("tile_height", 50)] {
                    self.report.require(
                        table.get(row, field).and_then(Value::as_i64).is_some_and(|n| n > 0),
                        requirement,
                        format!("{field} must be a positive integer"),
                    );
                }
                for (field, requirement) in [("pixel_x_size", 51), ("pixel_y_size", 52)] {
                    self.report.require(
                        table.get(row, field).and_then(Value::as_f64).is_some_and(|n| n.is_finite() && n > 0.),
                        requirement,
                        format!("{field} must be positive and finite"),
                    );
                }
                if let (Some(name), Some(matrix)) = (name, Matrix::read(table, row)) {
                    matrices.entry(name.to_string()).or_default().push(matrix);
                }
            }
        }
        let mut pyramids = BTreeMap::new();
        for (name, coverage) in registered {
            let Some(physical) = self.db.tables.keys().find(|key| key.eq_ignore_ascii_case(&name)).cloned() else {
                if self.db.object(&name) {
                    self.report.warnings.push(format!("tile view {name} still needs view-schema and value validation"));
                } else {
                    self.report.require(false, 54, format!("missing tile table {name}"));
                }
                continue;
            };
            self.structure(
                &physical,
                54,
                &[
                    column("id", "INTEGER", true, None, 1),
                    column("zoom_level", "INTEGER", true, None, 0),
                    column("tile_column", "INTEGER", true, None, 0),
                    column("tile_row", "INTEGER", true, None, 0),
                    column("tile_data", "BLOB", true, None, 0),
                ],
                false,
            );
            self.report.require(
                self.db.tables[&physical].schema.rowid_alias.is_some(),
                54,
                format!("{name}: tile primary key must alias rowid"),
            );
            let mut levels = matrices.remove(&name).unwrap_or_default();
            levels.sort_by_key(|matrix| matrix.zoom);
            let mut other_zoom = false;
            if let Some(table) = self.db.tables.get("gpkg_extensions") {
                for row in &table.rows {
                    if table.get(row, "table_name").and_then(Value::as_str) == Some(&name)
                        && table.get(row, "extension_name").and_then(Value::as_str) == Some("gpkg_zoom_other")
                    {
                        other_zoom = true;
                        self.report.require(
                            table.get(row, "column_name").and_then(Value::as_str) == Some("tile_data"),
                            88,
                            "zoom extension must reference tile_data",
                        );
                    }
                }
            }
            for pair in levels.windows(2) {
                let (previous, next) = (&pair[0], &pair[1]);
                self.report.require(
                    next.pixel_x < previous.pixel_x && next.pixel_y < previous.pixel_y,
                    53,
                    format!("{name}: pixel sizes must decrease with zoom"),
                );
                if !other_zoom && previous.zoom.checked_add(1) == Some(next.zoom) {
                    for (previous, next) in [(previous.pixel_x, next.pixel_x), (previous.pixel_y, next.pixel_y)] {
                        self.report.require(
                            previous > 0. && ((next - previous / 2.) / previous).abs() < 1e-5,
                            35,
                            format!("{name}: consecutive pixel sizes must halve"),
                        );
                    }
                }
            }
            if !levels.is_empty()
                && let Some(table) = self.db.tables.get("gpkg_tile_matrix_set")
            {
                for row in &table.rows {
                    if table.get(row, "table_name").and_then(Value::as_str) != Some(&name) {
                        continue;
                    }
                    let bounds = (|| {
                        Some((
                            table.get(row, "max_x")?.as_f64()? - table.get(row, "min_x")?.as_f64()?,
                            table.get(row, "max_y")?.as_f64()? - table.get(row, "min_y")?.as_f64()?,
                        ))
                    })();
                    let consistent = bounds.is_some_and(|(dx, dy)| {
                        dx > 0.
                            && dy > 0.
                            && levels.iter().all(|m| {
                                let mx = m.width as f64 * m.tile_width as f64 * m.pixel_x;
                                let my = m.height as f64 * m.tile_height as f64 * m.pixel_y;
                                ((mx - dx) / dx).abs() < 1e-3 && ((my - dy) / dy).abs() < 1e-3
                            })
                    });
                    self.report.require(consistent, 45, format!("{name}: tile matrix and set extents disagree"));
                }
            }
            pyramids.insert(physical, Pyramid { name, coverage, matrices: levels, coordinate_errors: BTreeSet::new(), webp: false });
        }
        if pyramids.is_empty() {
            return Ok(());
        }
        let text_encoding = u32::from_be_bytes(bytes[56..60].try_into().expect("snapshot checked header"));
        sqlite_repack::inspect(bytes, |entry, rowid, record| {
            if entry.0 != "table" {
                return Ok(());
            }
            let Some(pyramid) = pyramids.get_mut(&entry.1) else {
                return Ok(());
            };
            let table = &self.db.tables[&entry.1];
            let values = table.decode(record, rowid, text_encoding)?;
            let zoom = table.get(&values, "zoom_level").and_then(Value::as_i64);
            let matrix = zoom.and_then(|zoom| pyramid.matrices.iter().find(|matrix| matrix.zoom == zoom));
            if let Some(matrix) = matrix {
                for (field, limit, requirement) in [("tile_column", matrix.width, 56), ("tile_row", matrix.height, 57)] {
                    if !table.get(&values, field).and_then(Value::as_i64).is_some_and(|n| n >= 0 && n < limit) {
                        pyramid.coordinate_errors.insert((requirement, zoom));
                    }
                }
            } else {
                pyramid.coordinate_errors.insert((44, zoom));
                pyramid.coordinate_errors.insert((55, zoom));
            }
            let blob = match table.get(&values, "tile_data") {
                Some(Value::Blob(blob)) => blob.as_slice(),
                _ => &[],
            };
            self.report.require(blob.len() >= 12, 19, format!("{}: missing/short tile blob", pyramid.name));
            if blob.len() >= 12 {
                let kind = encoding(blob);
                self.report.require(kind.is_some(), 36, format!("{}: unrecognized tile signature", pyramid.name));
                let allowed = if pyramid.coverage {
                    matches!(kind, Some(Encoding::Png | Encoding::Tiff))
                } else {
                    matches!(kind, Some(Encoding::Png | Encoding::Jpeg | Encoding::Webp))
                };
                self.report.require(allowed, 36, format!("{}: tile encoding does not match registered data type", pyramid.name));
                pyramid.webp |= kind == Some(Encoding::Webp);
            }
            Ok(())
        })?;
        for pyramid in pyramids.values() {
            for (requirement, zoom) in &pyramid.coordinate_errors {
                self.report.require(false, *requirement, format!("{}: invalid tile coordinates at zoom {zoom:?}", pyramid.name));
            }
            if pyramid.webp {
                self.report.require(
                    self.tile_extension(&pyramid.name, "gpkg_webp", Some("read-write")),
                    91,
                    format!("{}: WebP content requires its extension", pyramid.name),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_signatures_require_complete_magic_and_handle_short_blobs() {
        for size in 0..12 {
            assert_eq!(encoding(&vec![0x89; size]), None);
        }
        assert_eq!(encoding(b"\x89PNG\0\0\0\0\0\0\0\0"), None);
        assert_eq!(encoding(b"\x89PNG\r\n\x1a\n\0\0\0\0"), Some(Encoding::Png));
        assert_eq!(encoding(b"\xff\xd8\xff\0\0\0\0\0\0\0\0\0"), Some(Encoding::Jpeg));
        assert_eq!(encoding(b"RIFF\0\0\0\0WEBP"), Some(Encoding::Webp));
        assert_eq!(encoding(b"II*\0\0\0\0\0\0\0\0\0"), Some(Encoding::Tiff));
        assert_eq!(encoding(b"MM\0*\0\0\0\0\0\0\0\0"), Some(Encoding::Tiff));
    }

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn tile_rules_match_gdal_on_real_derivatives() {
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let directory = tempfile::tempdir().unwrap();
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]); data=pathlib.Path(sys.argv[2]); base=root/'base.gpkg'
gdal.Translate(str(base),str(data/'landsat.tif'),format='GPKG',srcWin=[0,0,256,256],outputType=gdal.GDT_Byte,creationOptions=['RASTER_TABLE=tiles','TILE_FORMAT=PNG','VERSION=1.4']).Close()
with sqlite3.connect(base) as db:
 for name, in db.execute("SELECT name FROM sqlite_master WHERE type='trigger'").fetchall():
  db.execute('DROP TRIGGER "'+name.replace('"','""')+'"')
 db.execute('CREATE TABLE IF NOT EXISTS gpkg_extensions(table_name TEXT,column_name TEXT,extension_name TEXT NOT NULL,definition TEXT NOT NULL,scope TEXT NOT NULL,UNIQUE(table_name,column_name,extension_name))')
 assert db.execute('SELECT count(*) FROM tiles').fetchone()[0] > 0
cases=[]
def add(name,sql='',extra=()):
 path=root/(str(len(cases))+'.gpkg'); shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('PRAGMA ignore_check_constraints=ON')
  if sql: db.executescript(sql)
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(1,4,0); checker.extended_pragma_info=True
 with sqlite3.connect(path) as db: checker._check_tiles(db.cursor())
 cases.append(dict(name=name,path=str(path),requirements=sorted([e[0] for e in checker.errors]+list(extra))))
add('real Landsat PNG tiles')
add('empty tile user table','DELETE FROM tiles')
add('incomplete PNG signature',"UPDATE tiles SET tile_data=X'89504e470000000000000000'",extra=[36,36])
add('negative matrix zoom','UPDATE gpkg_tile_matrix SET zoom_level=-1')
for field in ['matrix_width','matrix_height','tile_width','tile_height','pixel_x_size','pixel_y_size']:
 for value in [0,-1]: add(field+' '+str(value),'UPDATE gpkg_tile_matrix SET '+field+'='+str(value))
for field in ['tile_column','tile_row']:
 add(field+' negative','UPDATE tiles SET '+field+'=-1')
 add(field+' outside matrix','UPDATE tiles SET '+field+'=99')
 # GDAL checks only MIN, missing an out-of-range maximum when a valid tile remains.
 add(field+' invalid maximum with valid minimum',"INSERT INTO tiles(zoom_level,tile_column,tile_row,tile_data) SELECT zoom_level,"+('99,tile_row' if field=='tile_column' else 'tile_column,99')+",tile_data FROM tiles LIMIT 1",extra=[56 if field=='tile_column' else 57])
add('unregistered user zoom','UPDATE tiles SET zoom_level=42')
add('missing SRS','UPDATE gpkg_tile_matrix_set SET srs_id=999999')
add('inconsistent x extent','UPDATE gpkg_tile_matrix_set SET max_x=max_x+1000')
add('inconsistent y extent','UPDATE gpkg_tile_matrix_set SET max_y=max_y+1000')
add('orphan matrix set',"INSERT INTO gpkg_tile_matrix_set SELECT 'ghost',srs_id,min_x,min_y,max_x,max_y FROM gpkg_tile_matrix_set")
add('orphan matrix',"INSERT INTO gpkg_tile_matrix SELECT 'ghost',zoom_level,matrix_width,matrix_height,tile_width,tile_height,pixel_x_size,pixel_y_size FROM gpkg_tile_matrix")
level="INSERT INTO gpkg_tile_matrix SELECT table_name,zoom_level+1,matrix_width*2,matrix_height*2,tile_width,tile_height,pixel_x_size/2,pixel_y_size/2 FROM gpkg_tile_matrix;"
add('two consistent zoom levels',level)
add('non-halving x resolution',level+'UPDATE gpkg_tile_matrix SET pixel_x_size=pixel_x_size*1.5 WHERE zoom_level=1')
add('increasing x resolution',level+'UPDATE gpkg_tile_matrix SET pixel_x_size=pixel_x_size*4 WHERE zoom_level=1')
extension="INSERT INTO gpkg_extensions VALUES('tiles','tile_data','gpkg_zoom_other','http://www.geopackage.org/spec/#extension_zoom_other_intervals','read-write');"
add('declared other zoom intervals',level+extension+'UPDATE gpkg_tile_matrix SET pixel_x_size=pixel_x_size*1.5 WHERE zoom_level=1')
add('wrong zoom extension column',extension.replace("'tile_data'","'tile_row'"))
add('unknown tile encoding',"UPDATE tiles SET tile_data=X'000102030405060708090a0b'")
webp="UPDATE tiles SET tile_data=X'524946460000000057454250';"
add('undeclared WebP signature',webp)
webpext="INSERT INTO gpkg_extensions VALUES('tiles','tile_data','gpkg_webp','http://www.geopackage.org/spec/#extension_tiles_webp','read-write');"
add('declared WebP signature',webp+webpext)
add('wrong WebP scope',webp+webpext.replace('read-write','write-only'))
add('TIFF signature not imagery',"UPDATE tiles SET tile_data=X'49492a000800000000000000'")
add('TIFF signature gridded coverage',"UPDATE gpkg_contents SET data_type='2d-gridded-coverage'; UPDATE tiles SET tile_data=X'49492a000800000000000000'")
add('JPEG signature imagery',"UPDATE tiles SET tile_data=X'ffd8ff000000000000000000'")
columns='id INTEGER PRIMARY KEY,zoom_level INTEGER NOT NULL,tile_column INTEGER NOT NULL,tile_row INTEGER NOT NULL,tile_data BLOB NOT NULL'
def rebuild(declaration,suffix=''):
 return 'ALTER TABLE tiles RENAME TO old_tiles; CREATE TABLE tiles('+declaration+')'+suffix+'; INSERT INTO tiles(id,zoom_level,tile_column,tile_row,tile_data) SELECT id,zoom_level,tile_column,tile_row,tile_data FROM old_tiles; DROP TABLE old_tiles;'
add('extra tile column',rebuild(columns+',note TEXT'))
add('nullable tile data',rebuild(columns.replace('tile_data BLOB NOT NULL','tile_data BLOB')))
add('nullable zoom',rebuild(columns.replace('zoom_level INTEGER NOT NULL','zoom_level INTEGER')))
add('wrong tile data declaration',rebuild(columns.replace('tile_data BLOB','tile_data TEXT')))
add('unexpected zoom default',rebuild(columns.replace('zoom_level INTEGER NOT NULL','zoom_level INTEGER NOT NULL DEFAULT 0')))
# OGC requirement 54 demands an INTEGER rowid alias; GDAL misses these declarations.
add('descending INTEGER primary key',rebuild(columns.replace('id INTEGER PRIMARY KEY','id INTEGER PRIMARY KEY DESC')),extra=[54])
add('WITHOUT ROWID tile table',rebuild(columns,' WITHOUT ROWID'),extra=[54])
add('INT is not a rowid alias',rebuild(columns.replace('id INTEGER PRIMARY KEY','id INT PRIMARY KEY')),extra=[54,54])

print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() >= 35);
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let snapshot = Snapshot::read(&bytes).unwrap();
            assert!(snapshot.tables["tiles"].rows.is_empty(), "tile payloads must not be retained in metadata snapshots");
            let report = super::super::check_core(&bytes);
            let mut requirements: Vec<_> = report
                .errors
                .iter()
                .filter_map(|error| matches!(error.requirement, 19 | 35 | 36 | 38..=57 | 88 | 91).then_some(error.requirement))
                .collect();
            requirements.sort_unstable();
            assert_eq!(serde_json::json!(requirements), case["requirements"], "{case}: {report:?}");
        }
        eprintln!("{} tile comparison cases passed", cases.len());
    }
}
