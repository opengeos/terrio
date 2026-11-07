//! SQLite-generated fixtures; Python's SQLite is an independent file-format oracle.
#![cfg(feature = "sqlite")]
use std::process::Command;

#[test]
fn reads_text_in_each_database_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"
import sqlite3,sys,pathlib
for encoding in ['UTF-8','UTF-16le','UTF-16be']:
 p=pathlib.Path(sys.argv[1])/(encoding+'.gpkg')
 c=sqlite3.connect(p)
 c.execute("PRAGMA encoding='"+encoding+"'")
 c.executescript('CREATE TABLE gpkg_contents(table_name TEXT,data_type TEXT,description TEXT);CREATE TABLE gpkg_geometry_columns(table_name TEXT,column_name TEXT,geometry_type_name TEXT,srs_id INTEGER,z INTEGER,m INTEGER);CREATE TABLE gpkg_spatial_ref_sys(srs_id INTEGER,organization TEXT,organization_coordsys_id INTEGER,definition TEXT);CREATE TABLE features(fid INTEGER PRIMARY KEY,name TEXT);')
 c.execute("INSERT INTO gpkg_contents VALUES('features','attributes',?)",('é水😀',))
 for fid in [-9223372036854775808,-1,9223372036854775807]:
  c.execute('INSERT INTO features VALUES(?,?)',(fid,'é水😀'))
 c.commit();c.close()
"#;
    assert!(Command::new("python3").args(["-c", script, dir.path().to_str().unwrap()]).status().unwrap().success());
    for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
        let path = dir.path().join(format!("{encoding}.gpkg"));
        let path = path.to_str().unwrap();
        let eager = terrio::open_vector(path, &Default::default()).unwrap();
        assert_eq!(eager.layers[0].len(), 3);
        for f in &eager.layers[0].features {
            assert_eq!(f.attributes[0], terrio::FieldValue::Text("é水😀".into()));
        }
        let mut readers = terrio::stream::open_layers(path, &Default::default(), 1).unwrap();
        let streamed = readers.remove(0).read_all().unwrap();
        assert_eq!(streamed.len(), 3);
        for (a, b) in streamed.features.iter().zip(&eager.layers[0].features) {
            assert_eq!(a.fid, b.fid);
            assert_eq!(a.attributes, b.attributes);
        }
    }
}
