//! GeoPackages in each SQLite text encoding, written by Python's sqlite3 module (SQLite is an
//! independent oracle for the file format), read whole, through the R-tree and streamed. Text
//! SQLite cannot decode reads as U+FFFD, as SQLite and GDAL 3.13 still read such files; a
//! malformed record is an error.
#![cfg(feature = "sqlite")]
use std::process::Command;
use terrio::{FieldValue, VectorOpenOptions};

const LAYER: &str = "couches_é水";
const TEXT: &str = "é水😀";

/// Writes `<encoding>.gpkg`, `<encoding>-invalid.gpkg` (one undecodable cell in the layer, in
/// `gpkg_contents` and in unrelated rows of `gpkg_spatial_ref_sys` and `gpkg_geometry_columns`),
/// `<encoding>-truncated.gpkg` (feature 2's text one byte longer than its record) and
/// `UTF-8-encoding0.gpkg` (a header text encoding of 0, which SQLite reads as UTF-8).
const FIXTURES: &str = r#"
import sqlite3, struct, sys, pathlib
out = pathlib.Path(sys.argv[1])
LAYER = 'couches_é水'
TEXT = 'é水😀'
BAD = {'UTF-8': b'\xff', 'UTF-16le': b'\x00\xd8', 'UTF-16be': b'\xd8\x00'}
def point(x, y):
    return b'GP\x00\x01' + struct.pack('<i', 4326) + struct.pack('<BIdd', 1, 1, x, y)
def make(path, encoding, bad=None):
    c = sqlite3.connect(path)
    c.execute("PRAGMA encoding='" + encoding + "'")
    c.execute('PRAGMA application_id=1196444487')
    c.executescript(f'''
CREATE TABLE gpkg_spatial_ref_sys(srs_name TEXT NOT NULL,srs_id INTEGER PRIMARY KEY,organization TEXT NOT NULL,organization_coordsys_id INTEGER NOT NULL,definition TEXT NOT NULL,description TEXT);
CREATE TABLE gpkg_contents(table_name TEXT PRIMARY KEY,data_type TEXT NOT NULL,identifier TEXT,description TEXT DEFAULT '',last_change DATETIME,min_x DOUBLE,min_y DOUBLE,max_x DOUBLE,max_y DOUBLE,srs_id INTEGER);
CREATE TABLE gpkg_geometry_columns(table_name TEXT NOT NULL,column_name TEXT NOT NULL,geometry_type_name TEXT NOT NULL,srs_id INTEGER NOT NULL,z TINYINT NOT NULL,m TINYINT NOT NULL);
CREATE TABLE "{LAYER}"(fid INTEGER PRIMARY KEY AUTOINCREMENT,geom POINT,name TEXT);
CREATE VIRTUAL TABLE "rtree_{LAYER}_geom" USING rtree(id,minx,maxx,miny,maxy);
''')
    c.execute("INSERT INTO gpkg_spatial_ref_sys VALUES('WGS 84',4326,'EPSG',4326,'GEOGCS[\"WGS 84\"]','')")
    c.execute("INSERT INTO gpkg_spatial_ref_sys VALUES('other',9999,'NONE',9999,'undefined',?)", (bad or b'ok',))
    c.execute("INSERT INTO gpkg_contents VALUES(?,'features',?,?,NULL,0,0,20,20,4326)", (LAYER, LAYER, bad or TEXT))
    c.execute("INSERT INTO gpkg_geometry_columns VALUES(?,'geom','POINT',4326,0,0)", (LAYER,))
    c.execute("INSERT INTO gpkg_geometry_columns VALUES('absent',?,'POINT',4326,0,0)", (bad or 'geom',))
    for fid, xy in [(1, 0.0), (2, 10.0), (3, 20.0)]:
        name = bad if (bad and fid == 2) else ('TRUNCATE_ME' if fid == 2 else TEXT)
        c.execute(f'INSERT INTO "{LAYER}" VALUES(?,?,?)', (fid, point(xy, xy), name))
        c.execute(f'INSERT INTO "rtree_{LAYER}_geom" VALUES(?,?,?,?,?)', (fid, xy, xy, xy, xy))
    # Bound blobs are stored as BLOB: make the injected cells TEXT holding the raw bytes.
    if bad:
        c.execute(f'UPDATE "{LAYER}" SET name=CAST(name AS TEXT) WHERE fid=2')
        c.execute('UPDATE gpkg_contents SET description=CAST(description AS TEXT)')
        c.execute('UPDATE gpkg_spatial_ref_sys SET description=CAST(description AS TEXT) WHERE srs_id=9999')
        c.execute("UPDATE gpkg_geometry_columns SET column_name=CAST(column_name AS TEXT) WHERE table_name='absent'")
        assert c.execute(f'SELECT typeof(name) FROM "{LAYER}" WHERE fid=2').fetchone() == ('text',)
    c.commit(); c.close()
for encoding in ['UTF-8', 'UTF-16le', 'UTF-16be']:
    make(out / f'{encoding}.gpkg', encoding)
    make(out / f'{encoding}-invalid.gpkg', encoding, BAD[encoding])
    # A record whose header claims one byte more text than it holds.
    src = (out / f'{encoding}.gpkg').read_bytes()
    marker = 'TRUNCATE_ME'.encode({'UTF-8': 'utf-8', 'UTF-16le': 'utf-16-le', 'UTF-16be': 'utf-16-be'}[encoding])
    at = src.index(marker)
    serial = at - len(point(0, 0)) - 1
    assert src[serial] == 13 + 2 * len(marker), (encoding, src[serial])
    b = bytearray(src); b[serial] += 2
    (out / f'{encoding}-truncated.gpkg').write_bytes(bytes(b))
# SQLite reads a header text encoding of 0 as UTF-8.
b = bytearray((out / 'UTF-8.gpkg').read_bytes()); b[56:60] = bytes(4)
(out / 'UTF-8-encoding0.gpkg').write_bytes(bytes(b))
"#;

fn features(path: &str, bbox: Option<(f64, f64, f64, f64)>) -> terrio::Result<(terrio::Layer, terrio::Layer)> {
    let options = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
    let eager = terrio::open_vector(path, &options)?.layers.remove(0);
    let streamed = terrio::stream::open_layers(path, &options, 1)?.remove(0).read_all()?;
    Ok((eager, streamed))
}

#[test]
fn reads_text_in_each_database_encoding() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Command::new("python3").args(["-c", FIXTURES, dir.path().to_str().unwrap()]).status().unwrap().success());
    let path = |name: &str| dir.path().join(format!("{name}.gpkg")).to_str().unwrap().to_string();
    let check = |name: &str, second: &str| {
        for (bbox, fids) in [(None, vec![1, 2, 3]), (Some((5.0, 5.0, 15.0, 15.0)), vec![2])] {
            let (eager, streamed) = features(&path(name), bbox).unwrap();
            assert_eq!(eager.name, LAYER, "{name}");
            assert_eq!(eager.crs_epsg(), Some(4326), "{name}");
            assert_eq!(streamed.crs_epsg(), Some(4326), "{name}");
            assert_eq!(eager.features.iter().map(|f| f.fid).collect::<Vec<_>>(), fids, "{name}");
            for f in &eager.features {
                assert!(f.geometry.is_some(), "{name}");
                let text = if f.fid == 2 { second } else { TEXT };
                assert_eq!(f.attributes, [FieldValue::Text(text.into())], "{name}");
            }
            assert_eq!(streamed.len(), eager.len(), "{name}");
            for (a, b) in streamed.features.iter().zip(&eager.features) {
                assert_eq!((a.fid, &a.geometry, &a.attributes), (b.fid, &b.geometry, &b.attributes), "{name}");
            }
        }
    };
    for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
        check(encoding, "TRUNCATE_ME");
        check(&format!("{encoding}-invalid"), "\u{fffd}");
        let truncated = path(&format!("{encoding}-truncated"));
        for bbox in [None, Some((5.0, 5.0, 15.0, 15.0))] {
            let options = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
            assert!(terrio::open_vector(&truncated, &options).is_err(), "{encoding}");
            assert!(terrio::stream::open_layers(&truncated, &options, 1).and_then(|mut r| r.remove(0).read_all()).is_err(), "{encoding}");
        }
    }
    check("UTF-8-encoding0", "TRUNCATE_ME");
}
