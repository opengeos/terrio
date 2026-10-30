//! GeoPackage tiled gridded coverage metadata, references and image tiles.
//! Follows GDAL 3.13.3 validate_gpkg.py (see NOTICE) and OGC 17-066r1.
use super::*;
use std::collections::BTreeSet;
use std::io::Cursor;

const COVERAGE: &str = "gpkg_2d_gridded_coverage_ancillary";
const TILE: &str = "gpkg_2d_gridded_tile_ancillary";
fn rule(number: u32) -> Requirement {
    Requirement::Extension(match number {
        1 => "gpkg_2d_gridded_coverage#1",
        2 => "gpkg_2d_gridded_coverage#2",
        3 => "gpkg_2d_gridded_coverage#3",
        6 => "gpkg_2d_gridded_coverage#6",
        7 => "gpkg_2d_gridded_coverage#7",
        8 => "gpkg_2d_gridded_coverage#8",
        9 => "gpkg_2d_gridded_coverage#9",
        10 => "gpkg_2d_gridded_coverage#10",
        11 => "gpkg_2d_gridded_coverage#11",
        12 => "gpkg_2d_gridded_coverage#12",
        13 => "gpkg_2d_gridded_coverage#13",
        14 => "gpkg_2d_gridded_coverage#14",
        15 => "gpkg_2d_gridded_coverage#15",
        16 => "gpkg_2d_gridded_coverage#16",
        17 => "gpkg_2d_gridded_coverage#17",
        18 => "gpkg_2d_gridded_coverage#18",
        19 => "gpkg_2d_gridded_coverage#19",
        20 => "gpkg_2d_gridded_coverage#20",
        _ => unreachable!("internal gridded-coverage rule"),
    })
}
struct Coverage {
    name: String,
    kinds: Vec<String>,
    tiles: u64,
    ancillary: u64,
    missing_ids: BTreeSet<i64>,
    invalid_id: bool,
}

#[derive(Debug)]
struct Image {
    bands: usize,
    uint16: bool,
    float32: bool,
    compression: u16,
    multiple_images: bool,
    full_width_blocks: bool,
}
/// Decode row-by-row or chunk-by-chunk so tile dimensions cannot demand a
/// whole-image allocation. Decoder limits also bound individual rows/chunks.
fn image(blob: &[u8]) -> std::result::Result<Image, String> {
    if blob.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut reader = png::Decoder::new(Cursor::new(blob)).read_info().map_err(|e| e.to_string())?;
        let info = reader.info();
        let result = Image {
            bands: info.color_type.samples(),
            uint16: info.bit_depth == png::BitDepth::Sixteen,
            float32: false,
            compression: 1,
            multiple_images: false,
            full_width_blocks: true,
        };
        while reader.next_row().map_err(|e| e.to_string())?.is_some() {}
        reader.finish().map_err(|e| e.to_string())?;
        return Ok(result);
    }
    use tiff::decoder::{ChunkType, Decoder};
    use tiff::tags::Tag;
    let mut decoder = Decoder::new(Cursor::new(blob)).map_err(|e| e.to_string())?.with_limits(crate::cog::TiffReadLimits::default().tiff());
    let (width, _) = decoder.dimensions().map_err(|e| e.to_string())?;
    let bits = decoder.get_tag_u16_vec(Tag::BitsPerSample).unwrap_or_else(|_| vec![1]);
    let format = decoder.get_tag_u16_vec(Tag::SampleFormat).unwrap_or_else(|_| vec![1]);
    let bands = decoder.get_tag_unsigned::<u16>(Tag::SamplesPerPixel).unwrap_or(1) as usize;
    let image = Image {
        bands,
        uint16: bits.first() == Some(&16) && format.first() == Some(&1),
        float32: bits.first() == Some(&32) && format.first() == Some(&3),
        compression: decoder.get_tag_unsigned::<u16>(Tag::Compression).unwrap_or(1),
        multiple_images: decoder.more_images() || decoder.get_tag_u64_vec(Tag::SubIfd).is_ok_and(|ifds| !ifds.is_empty()),
        full_width_blocks: decoder.chunk_dimensions().0 == width,
    };
    let count = match decoder.get_chunk_type() {
        ChunkType::Strip => decoder.strip_count(),
        ChunkType::Tile => decoder.tile_count(),
    }
    .map_err(|e| e.to_string())?;
    for index in 0..count {
        decoder.read_chunk(index).map_err(|e| e.to_string())?;
    }
    Ok(image)
}

impl Checker {
    pub(super) fn gridded(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(contents) = self.db.tables.get("gpkg_contents") else { return Ok(()) };
        let mut coverages: Vec<_> = contents
            .rows
            .iter()
            .filter(|row| contents.get(row, "data_type").and_then(Value::as_str) == Some("2d-gridded-coverage"))
            .filter_map(|row| contents.get(row, "table_name").and_then(Value::as_str))
            .map(|name| Coverage {
                name: name.into(),
                kinds: vec![],
                tiles: 0,
                ancillary: 0,
                missing_ids: BTreeSet::new(),
                invalid_id: false,
            })
            .collect();
        if coverages.is_empty() {
            return Ok(());
        }
        self.structure_rule(
            COVERAGE,
            rule(1),
            &[
                column("id", "INTEGER", true, None, 1),
                column("tile_matrix_set_name", "TEXT", true, None, 0),
                column("datatype", "TEXT", true, Some("'integer'"), 0),
                column("scale", "REAL", true, Some("1.0"), 0),
                column("offset", "REAL", true, Some("0.0"), 0),
                column("precision", "REAL", false, Some("1.0"), 0),
                column("data_null", "REAL", false, None, 0),
                column("grid_cell_encoding", "TEXT", false, Some("'grid-value-is-center'"), 0),
                column("uom", "TEXT", false, None, 0),
                column("field_name", "TEXT", false, Some("'Height'"), 0),
                column("quantity_definition", "TEXT", false, Some("'Height'"), 0),
            ],
            false,
        );
        self.structure_rule(
            TILE,
            rule(2),
            &[
                column("id", "INTEGER", true, None, 1),
                column("tpudt_name", "TEXT", true, None, 0),
                column("tpudt_id", "INTEGER", true, None, 0),
                column("scale", "REAL", true, Some("1.0"), 0),
                column("offset", "REAL", true, Some("0.0"), 0),
                column("min", "REAL", false, Some("NULL"), 0),
                column("max", "REAL", false, Some("NULL"), 0),
                column("mean", "REAL", false, Some("NULL"), 0),
                column("std_dev", "REAL", false, Some("NULL"), 0),
            ],
            false,
        );
        let srs = self.db.tables.get("gpkg_spatial_ref_sys");
        let records: Vec<_> = srs
            .map(|table| table.rows.iter().filter(|row| table.get(row, "srs_id") == Some(&Value::Int(4979))).collect())
            .unwrap_or_default();
        self.report.require_rule(records.len() == 1, rule(3), "gridded coverage requires exactly one EPSG:4979 SRS record");
        if let (Some(table), [row]) = (srs, records.as_slice()) {
            self.report.require_rule(
                table.get(row, "organization").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("epsg")),
                rule(3),
                "EPSG:4979 SRS organization must be EPSG",
            );
            self.report.require_rule(
                table.get(row, "organization_coordsys_id") == Some(&Value::Int(4979)),
                rule(3),
                "EPSG:4979 SRS organization_coordsys_id must be 4979",
            );
        }
        let extensions = self.db.tables.get("gpkg_extensions");
        self.report.require_rule(extensions.is_some(), rule(6), "gridded coverage requires gpkg_extensions");
        let declarations: Vec<_> = extensions
            .map(|table| {
                table
                    .rows
                    .iter()
                    .filter(|row| table.get(row, "extension_name").and_then(Value::as_str) == Some("gpkg_2d_gridded_coverage"))
                    .collect()
            })
            .unwrap_or_default();
        self.report.require_rule(
            declarations.len() == 2 + coverages.len(),
            rule(6),
            "wrong number of gridded-coverage extension declarations",
        );
        let mut found = BTreeSet::new();
        if let Some(ext) = extensions {
            for row in declarations {
                let name = ext.get(row, "table_name").and_then(Value::as_str);
                found.insert(name);
                if matches!(name, Some(COVERAGE | TILE)) {
                    self.report.require_rule(
                        matches!(ext.get(row, "column_name"), Some(Value::Null)),
                        rule(6),
                        "ancillary extension column_name must be null",
                    );
                } else {
                    self.report.require_rule(
                        name.is_some_and(|name| coverages.iter().any(|c| c.name == name)),
                        rule(6),
                        "gridded-coverage extension references an unregistered coverage",
                    );
                    self.report.require_rule(
                        ext.get(row, "column_name").and_then(Value::as_str) == Some("tile_data"),
                        rule(6),
                        "coverage extension column_name must be tile_data",
                    );
                }
                self.report.require_rule(
                    ext.get(row, "definition").and_then(Value::as_str) == Some("http://docs.opengeospatial.org/is/17-066r1/17-066r1.html"),
                    rule(6),
                    "wrong gridded-coverage extension definition",
                );
                self.report.require_rule(
                    ext.get(row, "scope").and_then(Value::as_str) == Some("read-write"),
                    rule(6),
                    "gridded-coverage extension scope must be read-write",
                );
            }
        }
        for name in [COVERAGE, TILE] {
            self.report.require_rule(found.contains(&Some(name)), rule(6), format!("missing extension declaration for {name}"));
        }
        if let Some(table) = self.db.tables.get(COVERAGE) {
            self.report.require_rule(table.rows.len() == coverages.len(), rule(7), "wrong number of coverage ancillary rows");
            for row in &table.rows {
                let name = table.get(row, "tile_matrix_set_name").and_then(Value::as_str);
                self.report.require_rule(
                    name.is_some_and(|name| coverages.iter().any(|c| c.name == name)),
                    rule(7),
                    "ancillary row refers to an unregistered coverage",
                );
                let registered = self
                    .db
                    .tables
                    .get("gpkg_tile_matrix_set")
                    .is_some_and(|matrix| matrix.rows.iter().any(|row| matrix.get(row, "table_name").and_then(Value::as_str) == name));
                self.report.require_rule(
                    name.is_some() && registered,
                    rule(8),
                    "coverage ancillary row has no tile-matrix-set registration",
                );
                let kind = table.get(row, "datatype").and_then(Value::as_str);
                self.report.require_rule(matches!(kind, Some("integer" | "float")), rule(9), "coverage datatype must be integer or float");
                if let Some(c) = coverages.iter_mut().find(|c| Some(c.name.as_str()) == name)
                    && let Some(kind) = kind
                {
                    c.kinds.push(kind.into());
                }
            }
        }
        if let Some(table) = self.db.tables.get(TILE) {
            let mut unknown = BTreeSet::new();
            for row in &table.rows {
                let name = table.get(row, "tpudt_name").and_then(Value::as_str);
                let Some(c) = coverages.iter_mut().find(|c| Some(c.name.as_str()) == name) else {
                    if unknown.insert(name) {
                        self.report.require_rule(false, rule(11), format!("tile ancillary row refers to unregistered coverage {name:?}"));
                    }
                    continue;
                };
                c.ancillary += 1;
                if let Some(id) = table.get(row, "tpudt_id").and_then(metadata::integer) {
                    c.missing_ids.insert(id);
                } else {
                    c.invalid_id = true;
                }
            }
            for c in &coverages {
                for _ in c.kinds.iter().filter(|kind| kind.as_str() == "float") {
                    let valid =
                        table.rows.iter().filter(|row| table.get(row, "tpudt_name").and_then(Value::as_str) == Some(c.name.as_str())).all(
                            |row| {
                                table.get(row, "scale").and_then(Value::as_f64) == Some(1.0)
                                    && table.get(row, "offset").and_then(Value::as_f64) == Some(0.0)
                            },
                        );
                    self.report.require_rule(valid, rule(9), format!("{}: float tile scale/offset must be 1/0", c.name));
                }
            }
        }
        let encoding = u32::from_be_bytes(
            bytes.get(56..60).and_then(|v| v.try_into().ok()).ok_or_else(|| Error::InvalidData("truncated SQLite header".into()))?,
        );
        sqlite_repack::inspect(bytes, |entry, rowid, record| {
            if entry.0 != "table" {
                return Ok(());
            }
            let Some(c) = coverages.iter_mut().find(|c| c.name.eq_ignore_ascii_case(&entry.1)) else {
                return Ok(());
            };
            let table = &self.db.tables[&entry.1];
            let row = table.decode(&entry.1, record, rowid, encoding)?;
            c.tiles += 1;
            if let Some(id) = table.get(&row, "id").and_then(metadata::integer) {
                c.missing_ids.remove(&id);
            }
            let blob = match table.get(&row, "tile_data") {
                Some(Value::Blob(blob)) => blob.as_slice(),
                _ => &[],
            };
            if c.kinds.is_empty() {
                return Ok(());
            }
            self.report.require(blob.len() >= 12, 19, format!("{}: invalid gridded tile blob", c.name));
            let decoded = image(blob);
            for kind in &c.kinds {
                if kind == "integer" {
                    self.report.require_rule(
                        blob.starts_with(b"\x89PNG\r\n\x1a\n"),
                        rule(13),
                        format!("{}: integer tile must be PNG", c.name),
                    );
                    match &decoded {
                        Ok(image) => {
                            self.report.require_rule(image.bands == 1, rule(13), format!("{}: integer tile must have one band", c.name));
                            self.report.require_rule(image.uint16, rule(13), format!("{}: integer tile must be UInt16", c.name));
                        }
                        Err(error) => self.report.require_rule(false, rule(13), format!("{}: invalid integer tile: {error}", c.name)),
                    }
                } else if kind == "float" {
                    self.report.require_rule(
                        blob.starts_with(b"II*\0") || blob.starts_with(b"MM\0*"),
                        rule(14),
                        format!("{}: float tile must be TIFF", c.name),
                    );
                    match &decoded {
                        Ok(image) => {
                            for (condition, number, message) in [
                                (image.bands == 1, 16, "must have one band"),
                                (image.float32, 17, "must be Float32"),
                                (matches!(image.compression, 1 | 5), 18, "compression must be none or LZW"),
                                (!image.multiple_images, 19, "must not contain multiple images or overviews"),
                                (image.full_width_blocks, 20, "blocks must span the image width"),
                            ] {
                                self.report.require_rule(condition, rule(number), format!("{}: float tile {message}", c.name));
                            }
                        }
                        Err(error) => self.report.require_rule(false, rule(15), format!("{}: invalid float tile: {error}", c.name)),
                    }
                }
            }
            Ok(())
        })?;
        for c in coverages {
            self.report.require_rule(c.tiles == c.ancillary, rule(10), format!("{}: tile and ancillary row counts disagree", c.name));
            self.report.require_rule(
                c.missing_ids.is_empty() && !c.invalid_id,
                rule(12),
                format!("{}: ancillary tpudt_id does not reference a tile", c.name),
            );
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    fn image_checks_decode_payloads_and_identify_types() {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 2, 2);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 1, 0, 2, 0, 3, 0, 4]).unwrap();
        }
        let info = image(&png).unwrap();
        assert!(info.uint16 && !info.float32 && info.bands == 1);
        assert!(image(&png[..png.len() / 2]).is_err());
        let mut tiff = Cursor::new(Vec::new());
        {
            let mut writer = tiff::encoder::TiffEncoder::new(&mut tiff).unwrap();
            writer.write_image::<tiff::encoder::colortype::Gray32Float>(2, 2, &[1., 2., 3., 4.]).unwrap();
        }
        let info = image(tiff.get_ref()).unwrap();
        assert!(info.float32 && !info.uint16 && info.bands == 1 && !info.multiple_images && info.full_width_blocks);
        assert!(image(&[]).is_err());
        assert!(image(b"II*\0\x08\0\0\0\0\0\0\0").is_err());
    }

    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn gridded_rules_match_gdal_and_detect_damaged_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys,struct
from osgeo import gdal
from osgeo_utils.samples.validate_gpkg import GPKGChecker
gdal.UseExceptions();gdal.PushErrorHandler('CPLQuietErrorHandler')
root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);cases=[]
coverage='gpkg_2d_gridded_coverage_ancillary';tile='gpkg_2d_gridded_tile_ancillary';prefix='gpkg_2d_gridded_coverage#'
def add(name,base,sql='',blob=None,extra=(),raises=False):
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:
  db.execute('PRAGMA ignore_check_constraints=ON')
  if sql:db.executescript(sql)
  if blob is not None:db.execute('UPDATE elevation SET tile_data=?',(blob,))
 checker=GPKGChecker(str(path),abort_at_first_error=False,warning_msg=False)
 checker.version=(1,4,0);checker.extended_pragma_info=True
 try:
  with sqlite3.connect(path) as db:checker._check_tiled_gridded_coverage_data(db.cursor())
 except (sqlite3.Error,RuntimeError,IndexError,struct.error,AttributeError):assert raises,name
 else:assert not raises,name
 cases.append(dict(name=name,path=str(path),requirements=sorted(str(e[0]) for e in checker.errors)+sorted(str(e) for e in extra),raises=raises))
 return path
sources={}
for kind,dtype,fmt in [('integer',gdal.GDT_UInt16,'PNG'),('float',gdal.GDT_Float32,'TIFF')]:
 base=root/(kind+'.gpkg');sources[kind]=base
 gdal.Translate(str(base),str(data/'dem.tif'),format='GPKG',srcWin=[0,0,64,64],outputType=dtype,creationOptions=['RASTER_TABLE=elevation','TILE_FORMAT='+fmt]).Close()
 with sqlite3.connect(base) as db:
  for name, in db.execute("SELECT name FROM sqlite_master WHERE type='trigger'").fetchall():db.execute('DROP TRIGGER "'+name.replace('"','""')+'"')
 add('USGS '+kind+' coverage',base)
 add(kind+' empty tiles',base,'DELETE FROM elevation;DELETE FROM '+tile)
 add(kind+' missing coverage table',base,'DROP TABLE '+coverage,raises=True)
 add(kind+' missing tile ancillary table',base,'DROP TABLE '+tile,raises=True)
 add(kind+' missing EPSG4979',base,'DELETE FROM gpkg_spatial_ref_sys WHERE srs_id=4979')
 add(kind+' wrong EPSG4979 organization',base,"UPDATE gpkg_spatial_ref_sys SET organization='bad' WHERE srs_id=4979")
 add(kind+' wrong EPSG4979 code',base,'UPDATE gpkg_spatial_ref_sys SET organization_coordsys_id=0 WHERE srs_id=4979')
 add(kind+' missing extensions table',base,'DROP TABLE gpkg_extensions',raises=True)
 for target in [coverage,tile,'elevation']:
  add(kind+' missing declaration '+target,base,"DELETE FROM gpkg_extensions WHERE table_name='"+target+"'")
 add(kind+' wrong declaration definition',base,"UPDATE gpkg_extensions SET definition='wrong' WHERE extension_name='gpkg_2d_gridded_coverage'")
 add(kind+' wrong declaration scope',base,"UPDATE gpkg_extensions SET scope='write-only' WHERE extension_name='gpkg_2d_gridded_coverage'")
 add(kind+' wrong declaration column',base,"UPDATE gpkg_extensions SET column_name='wrong' WHERE extension_name='gpkg_2d_gridded_coverage'")
 add(kind+' missing coverage row',base,'DELETE FROM '+coverage)
 add(kind+' unknown coverage row',base,"UPDATE "+coverage+" SET tile_matrix_set_name='missing'",raises=True)
 add(kind+' missing tile matrix set',base,'DELETE FROM gpkg_tile_matrix_set')
 add(kind+' invalid datatype',base,"UPDATE "+coverage+" SET datatype='invalid'")
 add(kind+' ancillary count',base,'DELETE FROM '+tile)
 add(kind+' unknown ancillary table',base,"UPDATE "+tile+" SET tpudt_name='missing'")
 for value in [99999,1.5]:add(kind+' missing ancillary tile '+str(value),base,'UPDATE '+tile+' SET tpudt_id='+str(value))
 for field,value in [('scale',2),('offset',3)]:add(kind+' tile '+field,base,'UPDATE '+tile+' SET '+field+'='+str(value))
 add(kind+' wrong coverage schema',base,"PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'datatype TEXT','datatype BLOB') WHERE name='"+coverage+"'")
 add(kind+' wrong ancillary default',base,"PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'1.0','2.0') WHERE name='"+tile+"'")
with sqlite3.connect(sources['integer']) as db:png_blob=db.execute('SELECT tile_data FROM elevation').fetchone()[0]
with sqlite3.connect(sources['float']) as db:tiff_blob=db.execute('SELECT tile_data FROM elevation').fetchone()[0]
add('float in integer coverage',sources['integer'],blob=tiff_blob)
add('PNG in float coverage',sources['float'],blob=png_blob)
for name,fmt,dtype,bands,options,overview,append in [
 ('byte PNG','PNG',gdal.GDT_Byte,[1],[],False,False),
 ('RGB16 PNG','PNG',gdal.GDT_UInt16,[1,1,1],[],False,False),
 ('RGB8 PNG','PNG',gdal.GDT_Byte,[1,1,1],[],False,False),
 ('plain float TIFF','GTiff',gdal.GDT_Float32,[1],[],False,False),
 ('LZW float TIFF','GTiff',gdal.GDT_Float32,[1],['COMPRESS=LZW'],False,False),
 ('DEFLATE float TIFF','GTiff',gdal.GDT_Float32,[1],['COMPRESS=DEFLATE'],False,False),
 ('UInt16 TIFF','GTiff',gdal.GDT_UInt16,[1],[],False,False),
 ('Float64 TIFF','GTiff',gdal.GDT_Float64,[1],[],False,False),
 ('RGB float TIFF','GTiff',gdal.GDT_Float32,[1,1,1],[],False,False),
 ('small tiles TIFF','GTiff',gdal.GDT_Float32,[1],['TILED=YES','BLOCKXSIZE=16','BLOCKYSIZE=16'],False,False),
 ('full-width tiles TIFF','GTiff',gdal.GDT_Float32,[1],['TILED=YES','BLOCKXSIZE=64','BLOCKYSIZE=16'],False,False),
 ('overview TIFF','GTiff',gdal.GDT_Float32,[1],[],True,False),
 ('multiple image TIFF','GTiff',gdal.GDT_Float32,[1],[],False,True),
]:
 path='/vsimem/gridded-validation.'+('png' if fmt=='PNG' else 'tif')
 ds=gdal.Translate(path,str(data/'dem.tif'),format=fmt,srcWin=[0,0,64,64],outputType=dtype,bandList=bands,creationOptions=options)
 if overview:ds.BuildOverviews('NEAREST',[2])
 ds.Close()
 if append:gdal.Translate(path,str(data/'dem.tif'),format=fmt,srcWin=[0,0,64,64],outputType=dtype,creationOptions=['APPEND_SUBDATASET=YES']).Close()
 blob=bytes(gdal.VSIGetMemFileBuffer_unsafe(path));gdal.Unlink(path)
 add(name,sources['integer' if fmt=='PNG' else 'float'],blob=blob)
# GDAL's metadata-only TIFF open does not decode samples. Damage the first
# compressed strip while keeping the IFD readable; Terrio must reject it.
path='/vsimem/damaged-gridded.tif'
ds=gdal.Translate(path,str(data/'dem.tif'),format='GTiff',srcWin=[0,0,64,64],outputType=gdal.GDT_Float32,creationOptions=['COMPRESS=LZW'])
ds.Close();ds=gdal.Open(path)
offset=int(ds.GetRasterBand(1).GetMetadataItem('BLOCK_OFFSET_0_0','TIFF'));ds.Close()
blob=bytearray(gdal.VSIGetMemFileBuffer_unsafe(path));gdal.Unlink(path)
blob[offset:offset+16]=bytes(16)
add('damaged compressed TIFF payload',sources['float'],blob=bytes(blob),extra=[prefix+'15'])
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(cases.len() > 60);
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let db = Snapshot::read(&bytes).unwrap();
            assert!(db.tables["elevation"].rows.is_empty(), "tile payloads must not be retained");
            let mut checker = Checker { db, report: CoreReport::default() };
            checker.gridded(&bytes).unwrap();
            if case["raises"].as_bool() == Some(true) {
                assert!(!checker.report.errors.is_empty(), "{case}");
                continue;
            }
            let mut actual: Vec<String> = checker
                .report
                .errors
                .iter()
                .map(|e| match e.requirement {
                    Requirement::Core(number) => number.to_string(),
                    Requirement::Extension(name) => name.into(),
                })
                .collect();
            actual.sort_unstable();
            let mut expected: Vec<String> = serde_json::from_value(case["requirements"].clone()).unwrap();
            expected.sort_unstable();
            assert_eq!(actual, expected, "{}: {:?}", case["name"], checker.report.errors);
        }
        eprintln!("{} gridded-coverage comparison cases passed", cases.len());
    }
}
