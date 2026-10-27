//! Editing GeoPackage files in place: renaming layers, changing a layer's CRS (without
//! reprojecting) or declared geometry type, dataset and layer metadata (GDAL's
//! `<GDALMultiDomainMetadata>` in `gpkg_metadata`), and updating features matched by feature
//! id or key fields.
//!
//! The file is read whole ([`Database`]), changed, written to a temporary file next to it and
//! renamed over it once complete: a failure at any point leaves the original untouched.
//! Read-only files, files with a hot rollback journal or a write-ahead log, and files with
//! triggers or views on a renamed table that terrio does not rewrite are refused.
//! The whole database is materialized in memory and rewriting needs space for a second file.
//! Symlinks update their resolved target and mode bits are preserved. Replacement detaches
//! hard links and does not preserve owner/group, ACLs or extended attributes. Callers must
//! exclude concurrent writers: the journal/WAL checks do not provide a transaction lock.

use std::collections::HashMap;
#[cfg(not(target_family = "wasm"))]
use std::path::Path;

use super::geopackage as gw;
use super::gpkg_update::{self as gu, Database, columns, int, text};
use super::sqlite as w;
use crate::crs::SpatialRef;
use crate::drivers::sqlite::{self as r, Db};
pub(crate) use crate::ops::vector_edit::UpdateCounts;
use crate::ops::vector_edit::{KeyValue, key_of_field};
use crate::{Error, Result};
use wbvector::Layer;

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn refuse(msg: impl std::fmt::Display) -> Error {
    Error::Unsupported(format!("GeoPackage edit: {msg}"))
}

/// Changes a GeoPackage file with `f`, safely: the file must be local, writable, without a hot
/// rollback journal or a write-ahead log; the changed database is written to a temporary file
/// in the same directory, then renamed over the original. On any error the original is left
/// as it was.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn modify_file<T>(path: &str, f: impl FnOnce(&mut Database) -> Result<T>) -> Result<T> {
    if !crate::vfs::Location::parse(path).is_local() {
        return Err(Error::Unsupported(format!("'{path}': only local GeoPackages can be changed in place")));
    }
    let resolved = std::fs::canonicalize(path)?;
    let path = resolved.to_str().ok_or_else(|| Error::InvalidArgument("GeoPackage path must be UTF-8".into()))?;
    let p = Path::new(path);
    let meta = std::fs::metadata(p)?;
    if !meta.is_file() {
        return Err(Error::InvalidArgument(format!("'{path}' is not a file")));
    }
    if meta.permissions().readonly() {
        return Err(Error::Unsupported(format!("'{path}' is read-only")));
    }
    if crate::drivers::sqlite_edit::has_hot_journal(p)? {
        return Err(Error::Unsupported(format!(
            "'{path}' has a hot rollback journal (a transaction interrupted or under way); open it with SQLite to recover it first"
        )));
    }
    let wal = format!("{path}-wal");
    if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
        return Err(Error::Unsupported(format!("'{path}' has a write-ahead log: checkpoint it with SQLite first")));
    }
    let (bytes, value) = modify_bytes(std::fs::read(p)?, f)?;
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(p.parent().unwrap_or(Path::new(".")))?;
    tmp.as_file_mut().write_all(&bytes)?;
    tmp.as_file().set_permissions(meta.permissions())?;
    tmp.as_file().sync_all()?;
    tmp.persist(p).map_err(|e| e.error)?;
    Ok(value)
}

#[cfg(target_family = "wasm")]
pub(crate) fn modify_file<T>(_path: &str, _f: impl FnOnce(&mut Database) -> Result<T>) -> Result<T> {
    Err(Error::Unsupported("in-place GeoPackage files are unavailable in WebAssembly".into()))
}

/// Changes a GeoPackage held in memory with `f`: see [`modify_file`].
pub(crate) fn modify_bytes<T>(bytes: Vec<u8>, f: impl FnOnce(&mut Database) -> Result<T>) -> Result<(Vec<u8>, T)> {
    let db = Db::new(bytes)?;
    let mut d = Database::load(&db)?;
    drop(db);
    if d.table("gpkg_contents").is_none() {
        return Err(Error::InvalidData("not a GeoPackage: no gpkg_contents table".into()));
    }
    let value = f(&mut d)?;
    Ok((d.finish()?, value))
}

/// The vector layers of a database (`features` and `attributes` tables), in file order.
pub(crate) fn layer_names(d: &Database) -> Vec<String> {
    let Some(t) = d.table("gpkg_contents") else { return Vec::new() };
    let cols = columns(&t.sql);
    let name = cols.iter().position(|c| eq(c, "table_name"));
    let kind = cols.iter().position(|c| eq(c, "data_type"));
    t.rows
        .iter()
        .filter(|(_, v)| kind.and_then(|k| v.get(k)).and_then(text).is_some_and(|k| k == "features" || k == "attributes"))
        .filter_map(|(_, v)| name.and_then(|n| v.get(n)).and_then(text).map(str::to_string))
        .filter(|n| d.table(n).is_some())
        .collect()
}

/// The layer named `name` (ignoring case), as the file spells it.
fn find_layer(d: &Database, name: &str) -> Result<String> {
    let names = layer_names(d);
    names
        .iter()
        .find(|n| eq(n, name))
        .cloned()
        .ok_or_else(|| Error::InvalidArgument(format!("no layer named '{name}' (layers: {})", names.join(", "))))
}

/// Sets `column` to `new` in the rows of `table` where it is `old` (ignoring case).
fn rename_rows(d: &mut Database, table: &str, column: &str, old: &str, new: &str) {
    d.set(table, column, old, column, w::Value::Text(new.into()));
}

/// Whether `sql` names `name` as an identifier (quoted or not), ignoring case.
fn mentions(sql: &str, name: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii();
    let mut start = 0;
    while let Some(i) = lower[start..].find(&n) {
        let at = start + i;
        let before = lower[..at].chars().next_back();
        let after = lower[at + n.len()..].chars().next();
        if !before.is_some_and(ident) && !after.is_some_and(ident) {
            return true;
        }
        start = at + n.len().max(1);
    }
    false
}

/// `CREATE [UNIQUE] INDEX name ON table (...)` with another table name.
fn retarget_index(sql: &str, new: &str) -> Option<String> {
    let upper = sql.to_ascii_uppercase();
    let on = upper.find(" ON ")? + 4;
    let rest = sql[on..].trim_start();
    let skipped = sql.len() - on - rest.len();
    let end = match rest.chars().next()? {
        q @ ('"' | '`' | '\'') => rest[1..].find(q)? + 2,
        '[' => rest.find(']')? + 1,
        _ => rest.find(|c: char| c.is_whitespace() || c == '(')?,
    };
    Some(format!("{}{}{}", &sql[..on + skipped], quote(new), &rest[end..]))
}

/// Renames a layer in place, as SQLite's `ALTER TABLE ... RENAME` and GDAL's GeoPackage driver
/// rename one: its table, R-tree index and triggers, indexes, and its rows in `gpkg_contents`
/// (and the identifier, when it was the name), `gpkg_geometry_columns`, `gpkg_ogr_contents`,
/// `gpkg_extensions`, `gpkg_metadata_reference`, `gpkg_data_columns` and `sqlite_sequence`.
pub(crate) fn rename_layer(d: &mut Database, old: &str, new: &str) -> Result<()> {
    if old.contains('(') || new.contains('(') {
        return Err(refuse("layer names containing '(' cannot be safely rewritten"));
    }
    let old = find_layer(d, old)?;
    if new.is_empty() {
        return Err(Error::InvalidArgument("the new layer name is empty".into()));
    }
    if old == new {
        return Ok(());
    }
    if !eq(&old, new) && d.has(new) {
        return Err(Error::InvalidArgument(format!("the GeoPackage already has a table named '{new}'")));
    }
    let geometry = d.geometry_column(&old).map(|(g, ..)| g);
    let rtree = geometry.as_ref().map(|g| format!("rtree_{old}_{g}"));
    let has_rtree = rtree.as_ref().is_some_and(|rt| d.table(&format!("{rt}_node")).is_some());
    let fid = {
        let t = d.table(&old).ok_or_else(|| Error::InvalidData(format!("no table '{old}'")))?;
        r::rowid_column(&t.sql).and_then(|i| columns(&t.sql).get(i).cloned())
    };
    // The triggers and views this does not know how to rewrite.
    let standard = |n: &str| {
        eq(n, &format!("trigger_insert_feature_count_{old}"))
            || eq(n, &format!("trigger_delete_feature_count_{old}"))
            || rtree.as_ref().is_some_and(|rt| n.to_ascii_lowercase().starts_with(&format!("{}_", rt.to_ascii_lowercase())))
    };
    for o in &d.objects {
        let own_rtree = rtree.as_ref().is_some_and(|rt| eq(&o.name, rt));
        if own_rtree {
            continue;
        }
        let on_table = o.kind == "trigger" && eq(&o.table, &old);
        if (on_table && !standard(&o.name)) || (!on_table && mentions(&o.sql, &old)) {
            return Err(refuse(format!("the {} '{}' refers to '{old}', which terrio does not rewrite", o.kind, o.name)));
        }
    }
    let had_count_triggers = d.objects.iter().any(|o| eq(&o.name, &format!("trigger_insert_feature_count_{old}")));

    // The table and its indexes.
    {
        let t = d.table_mut(&old).expect("the table exists");
        let open = t.sql.find('(').ok_or_else(|| refuse(format!("cannot parse the table of '{old}'")))?;
        t.sql = format!("CREATE TABLE {} {}", quote(new), &t.sql[open..]);
        t.name = new.to_string();
    }
    for i in d.indexes.iter_mut().filter(|i| eq(&i.table, &old)) {
        i.sql = retarget_index(&i.sql, new).ok_or_else(|| refuse(format!("cannot rewrite index '{}'", i.name)))?;
        i.table = new.to_string();
    }

    // Triggers and the R-tree, made anew under the new name.
    d.objects.retain(|o| !(o.kind == "trigger" && eq(&o.table, &old)) && !rtree.as_ref().is_some_and(|rt| eq(&o.name, rt)));
    if had_count_triggers {
        d.objects.extend(gw::feature_count_triggers(new));
    }
    if let (true, Some(g), Some(rt)) = (has_rtree, &geometry, &rtree) {
        let (_, tables, objects) = gw::rtree_index(new, g, fid.as_deref().unwrap_or("fid"), Vec::new());
        for ((name, sql), suffix) in tables.into_iter().zip(["_rowid", "_node", "_parent"]) {
            let t = d.table_mut(&format!("{rt}{suffix}")).ok_or_else(|| refuse(format!("'{rt}{suffix}' is missing")))?;
            t.name = name;
            t.sql = sql;
        }
        d.objects.extend(objects);
    }

    // The rows describing the layer.
    let identifier = d.lookup("gpkg_contents", "table_name", &old, "identifier");
    for t in
        ["gpkg_contents", "gpkg_geometry_columns", "gpkg_ogr_contents", "gpkg_extensions", "gpkg_metadata_reference", "gpkg_data_columns"]
    {
        rename_rows(d, t, "table_name", &old, new);
    }
    if identifier.as_ref().and_then(text).is_some_and(|i| i == old) {
        d.set("gpkg_contents", "table_name", new, "identifier", w::Value::Text(new.into()));
    }
    rename_rows(d, "sqlite_sequence", "name", &old, new);
    d.set("gpkg_contents", "table_name", new, "last_change", w::Value::Text(gw::now()));
    Ok(())
}

/// The `srs_id` of a CRS in the file (`None`: GDAL's undefined SRS), added to
/// `gpkg_spatial_ref_sys` when the file lacks it.
fn srs_id(d: &mut Database, srs: Option<&SpatialRef>) -> Result<i64> {
    let mut probe = Layer::new("srs_probe");
    probe.geom_type = Some(wbvector::GeometryType::Point);
    if let Some(s) = srs {
        s.apply_to_layer(&mut probe);
    }
    let mut b = gw::Builder::new();
    b.add_layer(&probe)?;
    let parts = b.into_layer_parts();
    let assumed = parts.geometry_columns.first().and_then(|r| r.get(3)).and_then(int).unwrap_or(0);
    let remap = d.merge_srs(&parts.srs)?;
    Ok(remap.iter().find(|(from, _)| *from == assumed).map_or(assumed, |(_, to)| *to))
}

/// Sets the CRS of a layer without reprojecting: its SRS in `gpkg_geometry_columns` and
/// `gpkg_contents`, and in the header of every geometry.
pub(crate) fn set_layer_crs(d: &mut Database, name: &str, srs: Option<&SpatialRef>) -> Result<()> {
    let name = find_layer(d, name)?;
    let (g, ..) = d.geometry_column(&name).ok_or_else(|| Error::InvalidArgument(format!("layer '{name}' has no geometry column")))?;
    let id = srs_id(d, srs)?;
    d.set("gpkg_geometry_columns", "table_name", &name, "srs_id", w::Value::Int(id));
    d.set("gpkg_contents", "table_name", &name, "srs_id", w::Value::Int(id));
    let t = d.table_mut(&name).expect("the layer has a table");
    if let Some(gi) = columns(&t.sql).iter().position(|c| eq(c, &g)) {
        for (_, v) in t.rows.iter_mut() {
            if let Some(w::Value::Blob(b)) = v.get_mut(gi)
                && b.len() >= 8
                && &b[..2] == b"GP"
            {
                let id = id as i32;
                let bytes = if b[3] & 1 == 1 { id.to_le_bytes() } else { id.to_be_bytes() };
                b[4..8].copy_from_slice(&bytes);
            }
        }
    }
    d.set("gpkg_contents", "table_name", &name, "last_change", w::Value::Text(gw::now()));
    Ok(())
}

/// Whether a GeoPackage layer has a geometry column rather than an attribute table.
pub(crate) fn has_geometry(d: &Database, name: &str) -> bool {
    d.geometry_column(name).is_some()
}

/// Sets the declared geometry type of a layer (`None`: any), with its Z and M flags, in
/// `gpkg_geometry_columns` and the table's declaration. Geometries are not changed.
pub(crate) fn set_geometry_type(d: &mut Database, name: &str, t: Option<wbvector::GeometryType>, z: bool, m: bool) -> Result<()> {
    let name = find_layer(d, name)?;
    let (g, ..) = d.geometry_column(&name).ok_or_else(|| Error::InvalidArgument(format!("layer '{name}' has no geometry column")))?;
    let type_name = t.map_or("GEOMETRY".to_string(), |t| t.as_str().to_ascii_uppercase());
    let table = d.table_mut(&name).expect("the layer has a table");
    let decl = r::column_types(&table.sql).into_iter().find(|(n, _)| eq(n, &g)).map(|(_, t)| t).unwrap_or_default();
    if !decl.is_empty() {
        let candidates = [format!("{} {decl}", quote(&g)), format!("{g} {decl}"), format!("`{g}` {decl}"), format!("[{g}] {decl}")];
        let found = candidates.iter().find_map(|candidate| {
            table.sql.match_indices(candidate).find_map(|(at, _)| {
                let before = table.sql[..at].chars().next_back();
                before.is_none_or(|ch| ch.is_whitespace() || matches!(ch, '(' | ',')).then_some((at, candidate))
            })
        });
        let Some((at, found)) = found else { return Err(refuse(format!("cannot rewrite the declaration of '{g}'"))) };
        let replacement = format!("{} {type_name}", &found[..found.len() - decl.len() - 1]);
        table.sql.replace_range(at..at + found.len(), &replacement);
    }
    d.set("gpkg_geometry_columns", "table_name", &name, "geometry_type_name", w::Value::Text(type_name));
    d.set("gpkg_geometry_columns", "table_name", &name, "z", w::Value::Int(i64::from(z)));
    d.set("gpkg_geometry_columns", "table_name", &name, "m", w::Value::Int(i64::from(m)));
    d.set("gpkg_contents", "table_name", &name, "last_change", w::Value::Text(gw::now()));
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Metadata
// ------------------------------------------------------------------------------------------------

/// Metadata items by domain (`""` for the default domain), in order.
pub(crate) type Metadata = Vec<(String, Vec<(String, String)>)>;

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The domains and items of GDAL's `<GDALMultiDomainMetadata>` XML.
pub(crate) fn parse_metadata_xml(xml: &str) -> Metadata {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<Metadata") {
        let after = &rest[i + 9..];
        let Some(close) = after.find('>') else { break };
        let head = &after[..close];
        let domain = head.split("domain=\"").nth(1).and_then(|d| d.split('"').next()).map(unescape).unwrap_or_default();
        let body_end =
            if head.ends_with('/') { close + 1 } else { after[close + 1..].find("</Metadata>").map_or(after.len(), |end| close + 1 + end) };
        let mut items = Vec::new();
        if !head.ends_with('/') {
            let mut body = &after[close + 1..body_end];
            while let Some(j) = body.find("<MDI key=\"") {
                let b = &body[j + 10..];
                let Some(kq) = b.find('"') else { break };
                let key = unescape(&b[..kq]);
                let Some(gt) = b[kq..].find('>') else { break };
                let v = &b[kq + gt + 1..];
                let Some(end) = v.find("</MDI>") else { break };
                items.push((key, unescape(&v[..end])));
                body = &v[end + 6..];
            }
        }
        out.push((domain, items));
        rest = &after[body_end.min(after.len())..];
    }
    out
}

/// GDAL's `<GDALMultiDomainMetadata>` XML of domains and items.
pub(crate) fn metadata_xml(md: &Metadata) -> String {
    let mut s = String::from("<GDALMultiDomainMetadata>\n");
    for (domain, items) in md.iter().filter(|(_, i)| !i.is_empty()) {
        if domain.is_empty() {
            s.push_str("  <Metadata>\n");
        } else {
            s.push_str(&format!("  <Metadata domain=\"{}\">\n", escape(domain)));
        }
        for (k, v) in items {
            s.push_str(&format!("    <MDI key=\"{}\">{}</MDI>\n", escape(k), escape(v)));
        }
        s.push_str("  </Metadata>\n");
    }
    s.push_str("</GDALMultiDomainMetadata>\n");
    s
}

/// The `gpkg_metadata` row holding GDAL's metadata of the dataset (`layer` `None`) or a layer.
fn metadata_row(d: &Database, layer: Option<&str>) -> Option<i64> {
    let refs = d.table("gpkg_metadata_reference")?;
    let md = d.table("gpkg_metadata")?;
    let rc = columns(&refs.sql);
    let mc = columns(&md.sql);
    let col = |cols: &[String], n: &str| cols.iter().position(|c| eq(c, n));
    let (scope, table, file) = (col(&rc, "reference_scope")?, col(&rc, "table_name")?, col(&rc, "md_file_id")?);
    let (uri, mime) = (col(&mc, "md_standard_uri")?, col(&mc, "mime_type")?);
    refs.rows.iter().find_map(|(_, v)| {
        let s = v.get(scope).and_then(text)?;
        let ok = match layer {
            None => eq(s, "geopackage"),
            Some(l) => eq(s, "table") && v.get(table).and_then(text).is_some_and(|t| eq(t, l)),
        };
        let id = v.get(file).and_then(int)?;
        let m = md.rows.iter().find(|(rid, _)| *rid == id)?;
        (ok && m.1.get(uri).and_then(text) == Some("http://gdal.org") && m.1.get(mime).and_then(text) == Some("text/xml")).then_some(id)
    })
}

/// GDAL's metadata of the dataset (`layer` `None`) or a layer: the default domain's items, with
/// a layer's `DESCRIPTION` and `IDENTIFIER` from `gpkg_contents` as GDAL reports them.
pub(crate) fn metadata(d: &Database, layer: Option<&str>) -> Metadata {
    let mut md = match metadata_row(d, layer) {
        Some(id) => {
            let t = d.table("gpkg_metadata").expect("the row was found");
            let c = columns(&t.sql).iter().position(|c| eq(c, "metadata"));
            let xml = t.rows.iter().find(|(r, _)| *r == id).and_then(|(_, v)| c.and_then(|c| v.get(c)).and_then(text)).unwrap_or_default();
            parse_metadata_xml(xml)
        }
        None => Vec::new(),
    };
    if let Some(l) = layer {
        let mut extra = Vec::new();
        for (key, column) in [("DESCRIPTION", "description"), ("IDENTIFIER", "identifier")] {
            if let Some(v) = d.lookup("gpkg_contents", "table_name", l, column).as_ref().and_then(text).filter(|v| !v.is_empty()) {
                if key == "IDENTIFIER" && v == l {
                    continue;
                }
                extra.push((key.to_string(), v.to_string()));
            }
        }
        if !extra.is_empty() {
            match md.iter_mut().find(|(dom, _)| dom.is_empty()) {
                Some((_, items)) => items.extend(extra),
                None => md.push((String::new(), extra)),
            }
        }
    }
    md
}

/// Sets and removes items of the default metadata domain of the dataset (`layer` `None`) or a
/// layer, as GDAL stores them: a `gpkg_metadata` row of `<GDALMultiDomainMetadata>` referenced
/// by `gpkg_metadata_reference`, and a layer's `DESCRIPTION` and `IDENTIFIER` in
/// `gpkg_contents`.
pub(crate) fn edit_metadata(d: &mut Database, layer: Option<&str>, set: &[(String, String)], unset: &[String]) -> Result<()> {
    if set.is_empty() && unset.is_empty() {
        return Ok(());
    }
    let layer = layer.map(|l| find_layer(d, l)).transpose()?;
    let mut set = set.to_vec();
    if let Some(l) = &layer {
        // DESCRIPTION and IDENTIFIER live in gpkg_contents.
        for (key, column) in [("DESCRIPTION", "description"), ("IDENTIFIER", "identifier")] {
            if let Some(i) = set.iter().rposition(|(k, _)| eq(k, key)) {
                let (_, v) = set.remove(i);
                set.retain(|(k, _)| !eq(k, key));
                d.set("gpkg_contents", "table_name", l, column, w::Value::Text(v));
            } else if unset.iter().any(|k| eq(k, key)) {
                let v = if key == "IDENTIFIER" { w::Value::Text(l.clone()) } else { w::Value::Text(String::new()) };
                d.set("gpkg_contents", "table_name", l, column, v);
            }
        }
    }
    let row = metadata_row(d, layer.as_deref());
    let mut md = match row {
        Some(_) => {
            let mut all = metadata(d, None);
            if layer.is_some() {
                all = metadata_row(d, layer.as_deref())
                    .and_then(|id| {
                        let t = d.table("gpkg_metadata")?;
                        let c = columns(&t.sql).iter().position(|c| eq(c, "metadata"))?;
                        t.rows.iter().find(|(r, _)| *r == id).and_then(|(_, v)| v.get(c)).and_then(text).map(parse_metadata_xml)
                    })
                    .unwrap_or_default();
            }
            all
        }
        None => Vec::new(),
    };
    if !md.iter().any(|(dom, _)| dom.is_empty()) {
        md.insert(0, (String::new(), Vec::new()));
    }
    let items = &mut md.iter_mut().find(|(dom, _)| dom.is_empty()).expect("the default domain exists").1;
    for k in unset {
        items.retain(|(key, _)| key != k);
    }
    for (k, v) in set {
        match items.iter_mut().find(|(key, _)| *key == k) {
            Some(item) => item.1 = v,
            None => items.push((k, v)),
        }
    }
    let empty = md.iter().all(|(_, i)| i.is_empty());
    match (row, empty) {
        (Some(id), true) => {
            if let Some(t) = d.table_mut("gpkg_metadata") {
                t.rows.retain(|(r, _)| *r != id);
            }
            if let Some(t) = d.table_mut("gpkg_metadata_reference") {
                let c = columns(&t.sql).iter().position(|c| eq(c, "md_file_id"));
                t.rows.retain(|(_, v)| c.and_then(|c| v.get(c)).and_then(int) != Some(id));
            }
        }
        (Some(id), false) => {
            let xml = metadata_xml(&md);
            let t = d.table_mut("gpkg_metadata").expect("the row was found");
            let c = columns(&t.sql).iter().position(|c| eq(c, "metadata")).ok_or_else(|| refuse("gpkg_metadata has no metadata column"))?;
            if let Some((_, v)) = t.rows.iter_mut().find(|(r, _)| *r == id) {
                v[c] = w::Value::Text(xml);
            }
        }
        (None, true) => {}
        (None, false) => {
            let xml = metadata_xml(&md);
            let t = d.ensure("gpkg_metadata", gw::METADATA_SQL);
            let id = t.rows.iter().map(|(r, _)| *r).max().unwrap_or(0) + 1;
            let values = [
                ("id", w::Value::Int(id)),
                ("md_scope", w::Value::Text("dataset".into())),
                ("md_standard_uri", w::Value::Text("http://gdal.org".into())),
                ("mime_type", w::Value::Text("text/xml".into())),
                ("metadata", w::Value::Text(xml)),
            ]
            .map(|(k, v)| (k.to_string(), v));
            Database::insert(t, &values);
            let t = d.ensure("gpkg_metadata_reference", gw::METADATA_REFERENCE_SQL);
            let values = [
                ("reference_scope", w::Value::Text(if layer.is_some() { "table" } else { "geopackage" }.into())),
                ("table_name", layer.clone().map_or(w::Value::Null, w::Value::Text)),
                ("column_name", w::Value::Null),
                ("row_id_value", w::Value::Null),
                ("timestamp", w::Value::Text(gw::now())),
                ("md_file_id", w::Value::Int(id)),
                ("md_parent_id", w::Value::Null),
            ]
            .map(|(k, v)| (k.to_string(), v));
            Database::insert(t, &values);
            for table in ["gpkg_metadata", "gpkg_metadata_reference"] {
                add_extension(d, table, "gpkg_metadata", gw::METADATA_EXTENSION);
            }
        }
    }
    if let Some(l) = &layer {
        d.set("gpkg_contents", "table_name", l, "last_change", w::Value::Text(gw::now()));
    }
    Ok(())
}

/// Adds a `gpkg_extensions` row (`read-write` scope) when the file lacks it.
fn add_extension(d: &mut Database, table: &str, extension: &str, definition: &str) {
    let t = d.ensure("gpkg_extensions", gw::EXTENSIONS_SQL);
    let cols = columns(&t.sql);
    let pos = |n: &str| cols.iter().position(|c| eq(c, n));
    let (tc, ec) = (pos("table_name"), pos("extension_name"));
    let exists = t.rows.iter().any(|(_, v)| {
        tc.and_then(|c| v.get(c)).and_then(text).is_some_and(|x| eq(x, table))
            && ec.and_then(|c| v.get(c)).and_then(text) == Some(extension)
    });
    if !exists {
        let values = [
            ("table_name", w::Value::Text(table.into())),
            ("column_name", w::Value::Null),
            ("extension_name", w::Value::Text(extension.into())),
            ("definition", w::Value::Text(definition.into())),
            ("scope", w::Value::Text("read-write".into())),
        ]
        .map(|(k, v)| (k.to_string(), v));
        Database::insert(t, &values);
    }
}

// ------------------------------------------------------------------------------------------------
// Feature updates
// ------------------------------------------------------------------------------------------------

/// How features of the input are matched with and written to the output layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Upsert {
    /// Update matched features, append the others.
    Merge,
    /// Update matched features only.
    UpdateOnly,
    /// Append unmatched features only.
    AppendOnly,
}

fn key_of_value(v: &w::Value) -> Option<KeyValue> {
    Some(match v {
        w::Value::Int(i) => KeyValue::Int(*i),
        w::Value::Real(f) => KeyValue::Real(f.to_bits()),
        w::Value::Text(s) => KeyValue::Text(s.clone()),
        _ => return None,
    })
}

/// Updates the features of layer `name` from those of `src`, as `gdal vector update` does:
/// each input feature is matched with the output feature of the same feature id (`keys`
/// empty) or the same values of the key fields (input features with a null key match nothing and are appended unless update-only;
/// a key matching several output features matches none); matched features take the input's
/// geometry and the values of the fields both layers have (matched by name, ignoring case),
/// unmatched ones are appended with new feature ids, as `mode` says. The R-tree, extent,
/// feature count and sequence follow.
pub(crate) fn update_features(d: &mut Database, name: &str, src: &Layer, keys: &[String], mode: Upsert) -> Result<UpdateCounts> {
    let name = find_layer(d, name)?;
    let geometry = d.geometry_column(&name);
    let t = d.table(&name).expect("the layer has a table");
    let sql = t.sql.clone();
    let decls = r::column_types(&sql);
    let defaults = r::column_defaults(&sql);
    let ipk = r::rowid_column(&sql);
    let geom_col = geometry.as_ref().and_then(|(g, ..)| decls.iter().position(|(n, _)| eq(n, g)));
    let srs = geometry.as_ref().map_or(0, |(_, s, _)| *s);
    let fields = src.schema.fields();
    // For each column, the input field giving its values.
    let map: Vec<Option<usize>> = decls
        .iter()
        .enumerate()
        .map(|(i, (n, _))| if Some(i) == ipk || Some(i) == geom_col { None } else { fields.iter().position(|f| eq(&f.name, n)) })
        .collect();

    // The key columns, in input and output.
    let mut key_cols = Vec::new();
    for k in keys {
        let si = fields
            .iter()
            .position(|f| eq(&f.name, k))
            .ok_or_else(|| Error::InvalidArgument(format!("cannot find field '{k}' in input layer")))?;
        let st = fields[si].field_type;
        if !matches!(st, wbvector::FieldType::Integer | wbvector::FieldType::Float | wbvector::FieldType::Text) {
            return Err(Error::Unsupported(format!(
                "type of field '{k}' is not one of those supported for a key field: String, Integer, Integer64, Real"
            )));
        }
        let di = decls
            .iter()
            .position(|(n, _)| eq(n, k))
            .filter(|i| Some(*i) != ipk && Some(*i) != geom_col)
            .ok_or_else(|| Error::InvalidArgument(format!("cannot find field '{k}' in output layer")))?;
        let dt = crate::drivers::vector::gpkg_read::declared_field_type(&decls[di].1);
        if dt != st {
            return Err(Error::Unsupported(format!("type of field '{k}' is not the same in input and output layers")));
        }
        key_cols.push((si, di));
    }

    // Output features by key (or by rowid).
    let mut index: HashMap<Vec<KeyValue>, Vec<i64>> = HashMap::new();
    let mut rowids: std::collections::HashSet<i64> = Default::default();
    for (id, v) in &t.rows {
        rowids.insert(*id);
        if !key_cols.is_empty() {
            let key: Option<Vec<KeyValue>> = key_cols.iter().map(|(_, di)| v.get(*di).and_then(key_of_value)).collect();
            if let Some(key) = key {
                index.entry(key).or_default().push(*id);
            }
        }
    }
    let encode = |f: &wbvector::Feature, base: Option<&Vec<w::Value>>| -> (Vec<w::Value>, Option<wbvector::BBox>, bool) {
        let mut values = match base {
            Some(b) => b.clone(),
            None => {
                let mut v: Vec<w::Value> = defaults.iter().map(|d| d.clone().map_or(w::Value::Null, gu::to_w)).collect();
                v.resize(decls.len(), w::Value::Null);
                v
            }
        };
        if let Some(i) = ipk {
            values[i] = w::Value::Null;
        }
        let mut bbox = None;
        let mut z = false;
        if let Some(gi) = geom_col {
            values[gi] = match &f.geometry {
                Some(g) => {
                    bbox = g.bbox().filter(|b| b.min_x.is_finite() && b.min_y.is_finite());
                    z = crate::vector::geometry_has_z(g);
                    w::Value::Blob(gw::gpkg_geometry(g, srs as i32))
                }
                None => w::Value::Null,
            };
        }
        for (i, m) in map.iter().enumerate() {
            if let Some(fi) = m {
                values[i] = match f.attributes.get(*fi) {
                    Some(v) if !v.is_null() => gu::convert(v, &decls[i].1),
                    _ => w::Value::Null,
                };
            }
        }
        (values, bbox, z)
    };

    let mut counts = UpdateCounts::default();
    let mut changed: Vec<(i64, Vec<w::Value>)> = Vec::new();
    let mut added: Vec<(i64, Vec<w::Value>)> = Vec::new();
    let mut bounds: Option<[f64; 4]> = None;
    let mut has_z = false;
    let max_rowid = t.rows.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let mut next = d.sequence(&name).max(max_rowid) + 1;
    let existing: HashMap<i64, &Vec<w::Value>> = t.rows.iter().map(|(id, v)| (*id, v)).collect();
    for f in &src.features {
        let target = if key_cols.is_empty() {
            let id = f.fid as i64;
            rowids.contains(&id).then_some(id)
        } else {
            let key: Option<Vec<KeyValue>> = key_cols.iter().map(|(si, _)| f.attributes.get(*si).and_then(key_of_field)).collect();
            // SQL equality does not match NULL keys: merge appends them,
            // update-only skips them, just as an ordinary unmatched key.
            match key.as_ref().and_then(|key| index.get(key)).map(Vec::as_slice) {
                Some([id]) => Some(*id),
                _ => None,
            }
        };
        let (bbox, z) = match target {
            Some(id) => {
                if mode == Upsert::AppendOnly {
                    counts.unchanged += 1;
                    continue;
                }
                let base = existing[&id];
                let (values, bbox, z) = encode(f, Some(base));
                if &values == base {
                    counts.unchanged += 1;
                    continue;
                }
                counts.updated += 1;
                changed.push((id, values));
                (bbox, z)
            }
            None => {
                if mode == Upsert::UpdateOnly {
                    counts.unchanged += 1;
                    continue;
                }
                let (values, bbox, z) = encode(f, None);
                counts.appended += 1;
                added.push((next, values));
                next += 1;
                (bbox, z)
            }
        };
        if let Some(b) = bbox {
            bounds = Some(match bounds {
                None => [b.min_x, b.min_y, b.max_x, b.max_y],
                Some(a) => [a[0].min(b.min_x), a[1].min(b.min_y), a[2].max(b.max_x), a[3].max(b.max_y)],
            });
        }
        has_z |= z;
    }
    if changed.is_empty() && added.is_empty() {
        return Ok(counts);
    }
    let total = {
        let t = d.table_mut(&name).expect("the layer has a table");
        let positions: HashMap<i64, usize> = t.rows.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
        for (id, v) in changed {
            if let Some(&i) = positions.get(&id) {
                t.rows[i].1 = v;
            }
        }
        t.rows.extend(added.iter().cloned());
        t.rows.len()
    };
    if let (Some((g, ..)), Some(gi)) = (&geometry, geom_col) {
        d.rebuild_rtree(&name, g, gi);
    }
    let enc = gu::Encoded { rows: added, geom_col, bounds, has_z };
    d.appended(&name, &sql, &enc, total as i64);
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry};

    fn layer(name: &str, n: u64) -> Layer {
        let mut l = Layer::new(name).with_epsg(4326);
        l.add_field(FieldDef::new("code", FieldType::Text));
        l.add_field(FieldDef::new("v", FieldType::Integer));
        for i in 0..n {
            let x = i as f64;
            l.push(Feature {
                fid: i + 1,
                geometry: Some(Geometry::polygon(vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 0.0), Coord::xy(x, 1.0)], vec![])),
                attributes: vec![FieldValue::Text(format!("c{i}")), FieldValue::Integer(i as i64)],
            });
        }
        l
    }

    fn read(bytes: &[u8]) -> Vec<Layer> {
        super::super::gpkg_read::read_db(&Db::new(bytes.to_vec()).unwrap(), &[], None).unwrap()
    }

    #[test]
    fn renames_layers_with_their_index_and_triggers() {
        let base = gw::to_bytes(&[layer("a", 30), layer("b", 2)]).unwrap();
        let (out, ()) = modify_bytes(base, |d| rename_layer(d, "A", "renamed")).unwrap();
        let layers = read(&out);
        assert_eq!(layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["renamed", "b"]);
        assert_eq!(layers[0].len(), 30);
        let db = Db::new(out.clone()).unwrap();
        let names: Vec<String> = db.schema().unwrap().into_iter().map(|e| e.1).collect();
        assert!(names.iter().any(|n| n == "rtree_renamed_geom_node"), "{names:?}");
        assert!(names.iter().any(|n| n == "trigger_insert_feature_count_renamed"));
        assert!(!names.iter().any(|n| n.contains("_a_") || n == "a"), "{names:?}");
        // The R-tree still finds features.
        let hits = super::super::gpkg_read::read_db(&db, &["renamed".into()], Some((10.1, 0.1, 10.2, 0.2))).unwrap();
        assert_eq!(hits[0].features.iter().map(|f| f.fid).collect::<Vec<_>>(), [11]);
        // Refusals: an existing name, a missing layer.
        let (out2, _) = modify_bytes(out.clone(), |_| Ok(())).unwrap();
        assert!(modify_bytes(out2.clone(), |d| rename_layer(d, "renamed", "b")).is_err());
        assert!(modify_bytes(out2.clone(), |d| rename_layer(d, "renamed", "parcels (2020)")).is_err());
        assert!(modify_bytes(out2, |d| rename_layer(d, "nope", "x")).is_err());
    }

    #[test]
    fn empty_metadata_domain_keeps_following_domains() {
        let parsed = parse_metadata_xml(
            r#"<GDALMultiDomainMetadata><Metadata/><Metadata domain="next"><MDI key="name">value</MDI></Metadata></GDALMultiDomainMetadata>"#,
        );
        assert_eq!(parsed, vec![(String::new(), vec![]), ("next".into(), vec![("name".into(), "value".into())])]);
    }

    #[test]
    fn crs_updates_respect_big_endian_geometry_headers() {
        let base = gw::to_bytes(&[layer("a", 1)]).unwrap();
        let (out, ()) = modify_bytes(base, |d| {
            let (g, ..) = d.geometry_column("a").unwrap();
            let t = d.table_mut("a").unwrap();
            let index = columns(&t.sql).iter().position(|name| name == &g).unwrap();
            let w::Value::Blob(blob) = &mut t.rows[0].1[index] else { panic!() };
            let id = i32::from_le_bytes(blob[4..8].try_into().unwrap());
            blob[3] &= !1;
            blob[4..8].copy_from_slice(&id.to_be_bytes());
            let doubles = match (blob[3] >> 1) & 7 {
                0 => 0,
                1 => 4,
                2 | 3 => 6,
                4 => 8,
                _ => panic!(),
            };
            for value in blob[8..8 + doubles * 8].as_chunks_mut::<8>().0 {
                value.reverse();
            }
            set_layer_crs(d, "a", Some(&SpatialRef::from_epsg(32633).unwrap()))?;
            let w::Value::Blob(blob) = &d.table("a").unwrap().rows[0].1[index] else { panic!() };
            assert_eq!(i32::from_be_bytes(blob[4..8].try_into().unwrap()), 32633);
            Ok(())
        })
        .unwrap();
        assert_eq!(read(&out)[0].crs_epsg(), Some(32633));
    }

    #[test]
    fn geometry_declaration_does_not_match_a_longer_column_name() {
        let mut input = layer("a", 1);
        input.add_field(FieldDef::new("mygeom", FieldType::Text));
        input.features[0].attributes.push(FieldValue::Text("kept".into()));
        let base = gw::to_bytes(&[input]).unwrap();
        let (_, ()) = modify_bytes(base, |d| {
            let table = d.table_mut("a").unwrap();
            // Put the similarly suffixed attribute before the unquoted geometry declaration.
            let names = columns(&table.sql);
            let geometry = names.iter().position(|name| name == "geom").unwrap();
            let attribute = names.iter().position(|name| name == "mygeom").unwrap();
            table.sql = table.sql.replace("\"geom\" POLYGON", "mygeom POLYGON").replace("\"mygeom\" TEXT", "geom POLYGON");
            for (_, values) in &mut table.rows {
                values.swap(geometry, attribute);
            }
            let before = table.sql.clone();
            // This test checks the SQL rewrite; the attribute's physical value stays text.
            set_geometry_type(d, "a", Some(wbvector::GeometryType::MultiPolygon), false, false)?;
            let after = &d.table("a").unwrap().sql;
            assert!(after.contains("geom MULTIPOLYGON"), "{before} => {after}");
            assert!(!after.contains("mygeom MULTIPOLYGON"), "{after}");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sets_crs_geometry_type_and_metadata() {
        let base = gw::to_bytes(&[layer("a", 3)]).unwrap();
        let (out, ()) = modify_bytes(base, |d| {
            set_layer_crs(d, "a", Some(&SpatialRef::from_epsg(32633).unwrap()))?;
            set_geometry_type(d, "a", Some(wbvector::GeometryType::MultiPolygon), false, false)?;
            edit_metadata(d, None, &[("A".into(), "1 & 2".into())], &[])?;
            edit_metadata(d, Some("a"), &[("K".into(), "v".into()), ("DESCRIPTION".into(), "about".into())], &[])
        })
        .unwrap();
        let layers = read(&out);
        assert_eq!(layers[0].crs_epsg(), Some(32633));
        assert_eq!(layers[0].geom_type, Some(wbvector::GeometryType::MultiPolygon));
        let (out, (ds, lyr)) = modify_bytes(out, |d| Ok((metadata(d, None), metadata(d, Some("a"))))).unwrap();
        assert_eq!(ds, [(String::new(), vec![("A".to_string(), "1 & 2".to_string())])]);
        assert_eq!(lyr[0].1, [("K".to_string(), "v".to_string()), ("DESCRIPTION".to_string(), "about".to_string())]);
        // Unsetting the last items removes the rows.
        let (out, ()) = modify_bytes(out, |d| edit_metadata(d, None, &[], &["A".into()])).unwrap();
        let (_, ds) = modify_bytes(out, |d| Ok(metadata(d, None))).unwrap();
        assert!(ds.is_empty());
    }

    #[test]
    fn updates_by_fid_and_key() {
        let base = gw::to_bytes(&[layer("a", 3)]).unwrap();
        let mut src = layer("a", 0);
        for (fid, code, v) in [(2, "c1", 10), (9, "new", 20)] {
            src.push(Feature {
                fid,
                geometry: Some(Geometry::point(5.0, 5.0)),
                attributes: vec![FieldValue::Text(code.into()), FieldValue::Integer(v)],
            });
        }
        let mut null_source = src.clone();
        null_source.features.truncate(1);
        null_source.features[0].attributes[0] = FieldValue::Null;
        let (with_null, counts) =
            modify_bytes(base.clone(), |d| update_features(d, "a", &null_source, &["code".into()], Upsert::Merge)).unwrap();
        assert_eq!(counts.appended, 1);
        assert_eq!(read(&with_null)[0].len(), 4);
        let (out, counts) = modify_bytes(base.clone(), |d| update_features(d, "a", &src, &[], Upsert::Merge)).unwrap();
        assert_eq!(counts, UpdateCounts { updated: 1, appended: 1, unchanged: 0 });
        let l = &read(&out)[0];
        assert_eq!(l.len(), 4);
        assert_eq!(l.features[1].attributes[1], FieldValue::Integer(10));
        assert_eq!(l.features[3].fid, 4);
        // By key, update only.
        let (out, counts) = modify_bytes(base.clone(), |d| update_features(d, "a", &src, &["code".into()], Upsert::UpdateOnly)).unwrap();
        assert_eq!(counts, UpdateCounts { updated: 1, appended: 0, unchanged: 1 });
        assert_eq!(read(&out)[0].len(), 3);
        // Append only.
        let (out, counts) = modify_bytes(base, |d| update_features(d, "a", &src, &["code".into()], Upsert::AppendOnly)).unwrap();
        assert_eq!(counts, UpdateCounts { updated: 0, appended: 1, unchanged: 1 });
        let l = &read(&out)[0];
        assert_eq!(l.features[1].attributes[1], FieldValue::Integer(1));
        assert_eq!(l.len(), 4);
    }

    #[test]
    fn modify_file_leaves_the_original_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.gpkg");
        let base = gw::to_bytes(&[layer("a", 3)]).unwrap();
        std::fs::write(&path, &base).unwrap();
        let p = path.to_str().unwrap();
        let before_entries =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>();
        assert!(modify_file(p, |d| rename_layer(d, "missing", "x")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), base);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>(),
            before_entries
        );
        // Read-only files are refused.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms.clone()).unwrap();
        assert!(matches!(modify_file(p, |d| rename_layer(d, "a", "x")), Err(Error::Unsupported(_))));
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        // A write-ahead log is refused.
        std::fs::write(dir.path().join("a.gpkg-wal"), b"x").unwrap();
        assert!(modify_file(p, |d| rename_layer(d, "a", "x")).is_err());
        std::fs::remove_file(dir.path().join("a.gpkg-wal")).unwrap();
        modify_file(p, |d| rename_layer(d, "a", "x")).unwrap();
        assert_eq!(super::super::gpkg_read::list_layers(p).unwrap(), ["x"]);
    }
    #[cfg(unix)]
    #[test]
    fn modify_file_follows_symlinks_and_preserves_mode_without_clobbering_stale_temp() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.gpkg");
        std::fs::write(&target, gw::to_bytes(&[layer("a", 1)]).unwrap()).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let alias = dir.path().join("alias.gpkg");
        symlink(&target, &alias).unwrap();
        let stale = dir.path().join("a.gpkg.terrio-edit");
        std::fs::write(&stale, "leave alone").unwrap();
        let entries = |directory: &std::path::Path| {
            std::fs::read_dir(directory).unwrap().map(|entry| entry.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>()
        };
        let before_entries = entries(dir.path());
        modify_file(alias.to_str().unwrap(), |d| rename_layer(d, "a", "renamed")).unwrap();
        assert!(std::fs::symlink_metadata(alias).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read_to_string(stale).unwrap(), "leave alone");
        assert_eq!(entries(dir.path()), before_entries);
        let bytes = std::fs::read(target).unwrap();
        assert_eq!(read(&bytes)[0].name, "renamed");
    }
}
