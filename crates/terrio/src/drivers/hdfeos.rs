// SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2004, Andrey Kiselev <dron@ak4719.spb.edu>
// SPDX-FileCopyrightText: Copyright (c) 2008-2009, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2023, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     port/cpl_conv.cpp
//     ogr/ogr_srs_usgs.cpp
//     frmts/hdf5/hdf5eosparser.cpp

//! HDF-EOS5 structural metadata, as GDAL's HDF5 driver reads it (`HDF5EOSParser`): the grids and
//! swaths that `HDFEOS INFORMATION/StructMetadata.0` describes in ODL.
//!
//! - A grid data field (`//HDFEOS/GRIDS/<grid>/Data_Fields/<field>`) gets the grid's geotransform
//!   (from `UpperLeftPointMtrs` and `LowerRightMtrs`, in packed degrees-minutes-seconds for
//!   geographic grids) and its CRS (the GCTP projection, parameters and sphere code, as GDAL's
//!   `importFromUSGS`), and its `XDim`/`YDim` dimensions tell its columns and rows (the third
//!   dimension, wherever it is, its bands).
//! - A swath field (`//HDFEOS/SWATHS/<swath>/Data_Fields/<field>` or `Geolocation_Fields`) gets
//!   the dimensions it shares with the swath's `Longitude` and `Latitude` as rows and columns
//!   (through the dimension maps), and those arrays as its geolocation arrays (GDAL's
//!   `GEOLOCATION` metadata), with the maps' offsets and increments.

use std::collections::HashMap;

/// A value of an ODL statement: a string (quotes removed), or a list of them.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Text(String),
    List(Vec<String>),
}

/// An ODL `GROUP` or `OBJECT`: its statements and children in file order.
#[derive(Debug, Default, Clone)]
struct Node {
    name: String,
    values: Vec<(String, Value)>,
    children: Vec<Node>,
}

impl Node {
    fn get(&self, key: &str) -> Option<&Value> {
        self.values.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Text(s) => Some(s),
            Value::List(_) => None,
        }
    }

    fn int(&self, key: &str) -> Option<i64> {
        let s = self.text(key)?;
        s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().map(|v| v as i64))
    }

    fn list(&self, key: &str) -> Vec<String> {
        match self.get(key) {
            Some(Value::List(l)) => l.clone(),
            Some(Value::Text(s)) => vec![s.clone()],
            None => Vec::new(),
        }
    }

    fn numbers(&self, key: &str) -> Vec<f64> {
        self.list(key).iter().filter_map(|s| s.trim().parse().ok()).collect()
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
}

/// Removes the quotes around a string value.
fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s).to_string()
}

/// Parses ODL (`GROUP=`, `OBJECT=`, `KEY=VALUE`, lists in parentheses, values continued over
/// several lines), as GDAL's `NASAKeywordHandler`.
fn parse_odl(text: &str) -> Node {
    let mut stack = vec![Node::default()];
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    while let Some(line) = lines.next() {
        if line == "END" {
            break;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, mut value) = (key.trim(), value.trim().to_string());
        match key {
            "GROUP" | "OBJECT" => stack.push(Node { name: unquote(&value), ..Default::default() }),
            "END_GROUP" | "END_OBJECT" => {
                if stack.len() > 1 {
                    let node = stack.pop().unwrap_or_default();
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(node);
                    }
                }
            }
            _ => {
                // A list continued on the next lines, up to its closing parenthesis.
                if value.starts_with('(') {
                    while !value.ends_with(')') {
                        match lines.next() {
                            Some(next) => value.push_str(next),
                            None => break,
                        }
                    }
                    let inner = value.trim_start_matches('(').trim_end_matches(')');
                    let items = split_list(inner).into_iter().map(|s| unquote(&s)).collect();
                    if let Some(node) = stack.last_mut() {
                        node.values.push((key.to_string(), Value::List(items)));
                    }
                } else if let Some(node) = stack.last_mut() {
                    node.values.push((key.to_string(), Value::Text(unquote(&value))));
                }
            }
        }
    }
    while stack.len() > 1 {
        let node = stack.pop().unwrap_or_default();
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        }
    }
    stack.pop().unwrap_or_default()
}

/// Splits a list's items at commas outside quotes.
fn split_list(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => items.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() || !items.is_empty() {
        items.push(current);
    }
    items
}

/// A named dimension and its size.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Dim {
    pub name: String,
    pub size: usize,
}

/// A grid: its dimensions, projection and corners.
#[derive(Debug, Clone, Default)]
pub(crate) struct Grid {
    pub dims: Vec<Dim>,
    /// The GCTP projection code (`HE5_GCTP_GEO` is 0), -1 when unknown.
    pub proj_code: i32,
    pub origin: String,
    pub proj_params: Vec<f64>,
    pub zone: i64,
    pub sphere: i64,
    pub upper_left: Vec<f64>,
    pub lower_right: Vec<f64>,
}

/// Where a field's rows, columns and bands are, and how it is georeferenced.
#[derive(Debug, Clone)]
pub(crate) struct Field {
    /// The field's dimensions, in storage order.
    pub dims: Vec<Dim>,
    /// The dimension of the columns, rows and bands (when there is a third).
    pub x: Option<usize>,
    pub y: Option<usize>,
    pub other: Option<usize>,
    pub kind: FieldKind,
}

#[derive(Debug, Clone)]
pub(crate) enum FieldKind {
    Grid(Box<Grid>),
    Swath(Option<SwathGeolocation>),
}

/// The geolocation arrays of a swath field: GDAL paths of the swath's `Longitude` and
/// `Latitude`, and the dimension maps' offsets and increments.
#[derive(Debug, Clone)]
pub(crate) struct SwathGeolocation {
    pub longitude: String,
    pub latitude: String,
    pub line_offset: i64,
    pub line_step: i64,
    pub pixel_offset: i64,
    pub pixel_step: i64,
}

/// The HDF-EOS5 structure of a file: its fields by GDAL path (`//HDFEOS/GRIDS/...`).
#[derive(Debug, Default)]
pub(crate) struct Eos {
    pub fields: HashMap<String, Field>,
    /// Whether it is a swath file (GDAL reads one model per file, grids first).
    pub swath: bool,
}

/// GCTP's projection names, by code (GDAL's `GetGTCPProjectionCode`).
const GCTP: [&str; 31] = [
    "HE5_GCTP_GEO",
    "HE5_GCTP_UTM",
    "HE5_GCTP_SPCS",
    "HE5_GCTP_ALBERS",
    "HE5_GCTP_LAMCC",
    "HE5_GCTP_MERCAT",
    "HE5_GCTP_PS",
    "HE5_GCTP_POLYC",
    "HE5_GCTP_EQUIDC",
    "HE5_GCTP_TM",
    "HE5_GCTP_STEREO",
    "HE5_GCTP_LAMAZ",
    "HE5_GCTP_AZMEQD",
    "HE5_GCTP_GNOMON",
    "HE5_GCTP_ORTHO",
    "HE5_GCTP_GVNSP",
    "HE5_GCTP_SNSOID",
    "HE5_GCTP_EQRECT",
    "HE5_GCTP_MILLER",
    "HE5_GCTP_VGRINT",
    "HE5_GCTP_HOM",
    "HE5_GCTP_ROBIN",
    "HE5_GCTP_SOM",
    "HE5_GCTP_ALASKA",
    "HE5_GCTP_GOOD",
    "HE5_GCTP_MOLL",
    "HE5_GCTP_IMOLL",
    "HE5_GCTP_HAMMER",
    "HE5_GCTP_WAGIV",
    "HE5_GCTP_WAGVII",
    "HE5_GCTP_OBLEQA",
];

fn dims_of(node: Option<&Node>) -> Vec<Dim> {
    let Some(node) = node else { return Vec::new() };
    node.children
        .iter()
        .filter_map(|d| Some(Dim { name: d.text("DimensionName")?.to_string(), size: d.int("Size").unwrap_or(0).max(0) as usize }))
        .collect()
}

/// Looks up a field's dimension names among the structure's dimensions; `None` when one is
/// unknown.
fn field_dims(names: &[String], known: &[Dim]) -> Option<Vec<Dim>> {
    names.iter().map(|n| known.iter().find(|d| &d.name == n).cloned()).collect()
}

impl Eos {
    /// Parses `StructMetadata.0`. `None` when it describes neither grids nor swaths.
    pub fn parse(text: &str) -> Option<Eos> {
        let root = parse_odl(text);
        let mut eos = Eos::default();
        let non_empty = |n: &&Node| !n.children.is_empty() || !n.values.is_empty();
        if let Some(grids) = root.child("GridStructure").filter(non_empty) {
            for g in &grids.children {
                eos.add_grid(g);
            }
        } else {
            let swaths = root.child("SwathStructure").filter(non_empty)?;
            eos.swath = true;
            for s in &swaths.children {
                eos.add_swath(s);
            }
        }
        Some(eos)
    }

    fn add_grid(&mut self, g: &Node) {
        let Some(name) = g.text("GridName") else { return };
        let data_fields = g.child("DataField");
        let mut dims = dims_of(g.child("Dimension"));
        // `XDim` and `YDim` given as the grid's sizes rather than as dimensions (AMSR2 polar
        // grids), when a field is laid out on them.
        let (nx, ny) = (g.int("XDim").unwrap_or(0), g.int("YDim").unwrap_or(0));
        let named = |n: &str| dims.iter().any(|d| d.name == n);
        if nx > 0 && ny > 0 && !named("XDim") && !named("YDim") {
            let on_grid = data_fields.is_some_and(|f| f.children.iter().any(|c| c.list("DimList") == ["YDim", "XDim"]));
            if on_grid {
                dims.push(Dim { name: "YDim".into(), size: ny as usize });
                dims.push(Dim { name: "XDim".into(), size: nx as usize });
            }
        }
        let projection = g.text("Projection").unwrap_or_default();
        let grid = Grid {
            dims: dims.clone(),
            proj_code: GCTP.iter().position(|p| *p == projection).map_or(-1, |i| i as i32),
            origin: g.text("GridOrigin").unwrap_or_default().to_string(),
            proj_params: g.numbers("ProjParams"),
            zone: g.int("ZoneCode").unwrap_or(-1),
            sphere: g.int("SphereCode").unwrap_or(-1),
            upper_left: g.numbers("UpperLeftPointMtrs"),
            lower_right: g.numbers("LowerRightMtrs"),
        };
        let Some(fields) = data_fields else { return };
        for f in &fields.children {
            let Some(field) = f.text("DataFieldName") else { continue };
            let names = f.list("DimList");
            let Some(fdims) = field_dims(&names, &dims).filter(|d| !d.is_empty()) else { continue };
            let (mut x, mut y, mut other) = (None, None, None);
            for (i, d) in fdims.iter().enumerate() {
                match d.name.as_str() {
                    "XDim" => x = Some(i),
                    "YDim" => y = Some(i),
                    _ => other = Some(i),
                }
            }
            let path = format!("//HDFEOS/GRIDS/{name}/Data_Fields/{field}");
            self.fields.insert(path, Field { dims: fdims, x, y, other, kind: FieldKind::Grid(Box::new(grid.clone())) });
        }
    }

    fn add_swath(&mut self, s: &Node) {
        let Some(name) = s.text("SwathName") else { return };
        let dims = dims_of(s.child("Dimension"));
        // Data dimension -> (geolocation dimension, offset, increment).
        let mut maps: Vec<(String, String, i64, i64)> = Vec::new();
        if let Some(m) = s.child("DimensionMap") {
            for d in &m.children {
                let (Some(geo), Some(data)) = (d.text("GeoDimension"), d.text("DataDimension")) else { continue };
                maps.push((data.to_string(), geo.to_string(), d.int("Offset").unwrap_or(0), d.int("Increment").unwrap_or(1)));
            }
        }
        let geo_fields = s.child("GeoField");
        let (mut lon_dims, mut lat_dims) = (Vec::new(), Vec::new());
        if let Some(gf) = geo_fields {
            for f in &gf.children {
                let names = f.list("DimList");
                let Some(fdims) = field_dims(&names, &dims) else { continue };
                for n in &names {
                    // A geolocation dimension without a map maps to itself.
                    if !maps.iter().any(|(data, ..)| data == n) {
                        maps.push((n.clone(), n.clone(), 0, 1));
                    }
                }
                match f.text("GeoFieldName") {
                    Some("Longitude") => lon_dims = fdims,
                    Some("Latitude") => lat_dims = fdims,
                    _ => {}
                }
            }
        }
        let has_geolocation = lon_dims.len() == 2 && lon_dims.iter().map(|d| &d.name).eq(lat_dims.iter().map(|d| &d.name));
        let mut register = |group: &str, field_name_key: &str, node: Option<&Node>, require_dims: bool| {
            let Some(node) = node else { return };
            for f in &node.children {
                let Some(field) = f.text(field_name_key) else { continue };
                let names = f.list("DimList");
                if require_dims && names.is_empty() {
                    continue;
                }
                let Some(fdims) = field_dims(&names, &dims) else { continue };
                let (mut x, mut y, mut other, mut geolocation) = (None, None, None, None);
                if fdims.len() >= 2 && has_geolocation {
                    let (mut x_name, mut y_name) = (None, None);
                    for (i, d) in fdims.iter().enumerate() {
                        match maps.iter().find(|(data, ..)| *data == d.name) {
                            Some((_, geo, ..)) if *geo == lon_dims[0].name => {
                                y = Some(i);
                                y_name = Some(d.name.clone());
                            }
                            Some((_, geo, ..)) if *geo == lon_dims[1].name => {
                                x = Some(i);
                                x_name = Some(d.name.clone());
                            }
                            // Any other dimension holds the bands.
                            _ => other = Some(i),
                        }
                    }
                    if x.is_some() && y.is_some() {
                        let map = |n: &Option<String>| maps.iter().find(|(data, ..)| Some(data) == n.as_ref()).map(|m| (m.2, m.3));
                        let (line_offset, line_step) = map(&y_name).unwrap_or((0, 1));
                        let (pixel_offset, pixel_step) = map(&x_name).unwrap_or((0, 1));
                        geolocation = Some(SwathGeolocation {
                            longitude: format!("//HDFEOS/SWATHS/{name}/Geolocation_Fields/Longitude"),
                            latitude: format!("//HDFEOS/SWATHS/{name}/Geolocation_Fields/Latitude"),
                            line_offset,
                            line_step,
                            pixel_offset,
                            pixel_step,
                        });
                    } else {
                        // Not laid out on the geolocation arrays: GDAL's default layout.
                        (x, y, other) = (None, None, None);
                    }
                }
                let path = format!("//HDFEOS/SWATHS/{name}/{group}/{field}");
                self.fields.insert(path, Field { dims: fdims, x, y, other, kind: FieldKind::Swath(geolocation) });
            }
        };
        register("Geolocation_Fields", "GeoFieldName", geo_fields, false);
        register("Data_Fields", "DataFieldName", s.child("DataField"), true);
    }

    /// The field at a GDAL path, when its dimensions match the dataset's rank.
    pub fn field(&self, gdal_path: &str, rank: usize) -> Option<&Field> {
        self.fields.get(gdal_path).filter(|f| f.dims.len() == rank)
    }
}

impl Field {
    /// The subdataset description GDAL gives a swath field: `(nscans=1980)x(npixs=392)`, with
    /// the third dimension first or last as stored. `None` for grid fields, which GDAL describes
    /// by their sizes alone.
    pub fn swath_description(&self) -> Option<String> {
        let (FieldKind::Swath(_), Some(x), Some(y)) = (&self.kind, self.x, self.y) else { return None };
        let d = |i: usize| format!("({}={})", self.dims[i].name, self.dims[i].size);
        Some(match (self.dims.len(), self.other) {
            (2, _) => format!("{}x{}", d(y), d(x)),
            (3, Some(0)) => format!("{}x{}x{}", d(0), d(y), d(x)),
            (3, Some(o)) => format!("{}x{}x{}", d(y), d(x), d(o)),
            _ => return None,
        })
    }
}

/// GDAL's `CPLPackedDMSToDec`: packed degrees, minutes and seconds (`DDDMMMSSS.SS`) to degrees.
pub(crate) fn packed_dms_to_degrees(packed: f64) -> f64 {
    let sign = if packed < 0.0 { -1.0 } else { 1.0 };
    let mut seconds = packed.abs();
    let degrees = (seconds / 1_000_000.0).floor();
    seconds -= degrees * 1_000_000.0;
    let minutes = (seconds / 1000.0).floor();
    seconds -= minutes * 1000.0;
    sign * (degrees * 3600.0 + minutes * 60.0 + seconds) / 3600.0
}

impl Grid {
    /// The geotransform of a grid with its origin at the upper left (GDAL's
    /// `GridMetadata::GetGeoTransform`).
    pub fn geo_transform(&self) -> Option<[f64; 6]> {
        if self.proj_code < 0 || !(self.origin == "HE5_HDFE_GD_UL" || self.origin.is_empty()) {
            return None;
        }
        let ([ulx, uly], [lrx, lry]) =
            (<[f64; 2]>::try_from(self.upper_left.as_slice()).ok()?, <[f64; 2]>::try_from(self.lower_right.as_slice()).ok()?);
        let size = |n: &str| self.dims.iter().find(|d| d.name == n).map(|d| d.size).filter(|s| *s > 0);
        let (nx, ny) = (size("XDim")? as f64, size("YDim")? as f64);
        let f = |v: f64| if self.proj_code == 0 { packed_dms_to_degrees(v) } else { v };
        Some([f(ulx), (f(lrx) - f(ulx)) / nx, 0.0, f(uly), 0.0, (f(lry) - f(uly)) / ny])
    }

    /// The CRS, as GDAL's `importFromUSGS` builds it from the GCTP projection, parameters, zone
    /// and sphere code: an EPSG code for WGS 84 CRSs it identifies, a PROJ definition otherwise.
    pub fn crs(&self) -> Option<wbraster::CrsInfo> {
        gctp_crs(self.proj_code, self.zone, &self.proj_params, self.sphere)
    }
}

/// The ellipsoids of GCTP's sphere codes, as GDAL maps them to EPSG ellipsoids: semi-major axis
/// and inverse flattening (0 for a sphere). `None` for the codes GDAL skips.
fn gctp_ellipsoid(code: i64) -> Option<(f64, f64)> {
    // Clarke 1880 and Everest 1830 are defined in feet (Clarke's and Indian): converted as PROJ.
    let feet = |a: f64, b: f64, foot: f64| (a * foot, a / (a - b));
    Some(match code {
        0 => (6378206.4, 6378206.4 / (6378206.4 - 6356583.8)),
        1 => feet(20926202.0, 20854895.0, 0.3047972654),
        2 => (6377397.155, 299.1528128),
        4 | 26 => (6378388.0, 297.0),
        5 => (6378135.0, 298.26),
        6 => feet(20922931.8, 20853374.58, 0.30479951),
        7 => (6378145.0, 298.25),
        8 => (6378137.0, 298.257222101),
        9 => (6377563.396, 299.3249646),
        10 | 24 => (6377304.063, 300.8017),
        11 => (6377340.189, 299.3249646),
        12 => (6378137.0, 298.257223563),
        14 => (6378160.0, 298.25),
        15 => (6378245.0, 298.3),
        16 => (6378270.0, 297.0),
        19 => (6370997.0, 0.0),
        20 => (6377483.865, 299.1528128),
        21 => (6377298.556, 300.8017),
        22 => (6377301.243, 300.8017255),
        23 => (6377295.664, 300.8017),
        27 => (6378200.0, 298.3),
        28 => (6378160.0, 298.247),
        29 => (6378160.0, 298.247167427),
        _ => return None,
    })
}

/// A GCTP CRS (GDAL's `importFromUSGS` with angles in packed DMS) as terrio's CRS.
pub(crate) fn gctp_crs(proj: i32, zone: i64, params: &[f64], sphere: i64) -> Option<wbraster::CrsInfo> {
    let p = |i: usize| params.get(i).copied().unwrap_or(0.0);
    let a = |i: usize| packed_dms_to_degrees(p(i));
    let fe = format!("+x_0={} +y_0={}", p(6), p(7));
    let wgs84 = sphere == 12;
    let projection = match proj {
        0 => "+proj=longlat".to_string(),
        1 => {
            let (mut z, mut north) = (zone, true);
            if z == 0 {
                if p(2) != 0.0 {
                    z = p(2) as i64;
                } else if p(0) != 0.0 && p(1) != 0.0 {
                    let lon = a(0);
                    z = ((lon + 180.0) / 6.0 + 1.0) as i64;
                    north = lon >= 0.0;
                }
            }
            if !(-60..=60).contains(&z) || z == 0 {
                return None;
            }
            if z < 0 {
                z = -z;
                north = false;
            }
            if wgs84 {
                return Some(wbraster::CrsInfo::from_epsg(if north { 32600 } else { 32700 } + z as u32));
            }
            format!("+proj=utm +zone={z}{}", if north { "" } else { " +south" })
        }
        3 => format!("+proj=aea +lat_0={} +lon_0={} +lat_1={} +lat_2={} {fe}", a(5), a(4), a(2), a(3)),
        4 => format!("+proj=lcc +lat_0={} +lon_0={} +lat_1={} +lat_2={} {fe}", a(5), a(4), a(2), a(3)),
        5 => format!("+proj=merc +lat_ts={} +lon_0={} {fe}", a(5), a(4)),
        6 => {
            let lat = a(5);
            let pole = if lat < 0.0 { -90.0 } else { 90.0 };
            if (lat.abs() - 90.0).abs() > 1e-8 {
                format!("+proj=stere +lat_0={pole} +lat_ts={lat} +lon_0={} {fe}", a(4))
            } else {
                format!("+proj=stere +lat_0={pole} +lon_0={} +k=1 {fe}", a(4))
            }
        }
        7 => format!("+proj=poly +lat_0={} +lon_0={} {fe}", a(5), a(4)),
        8 => {
            let lat2 = if p(8) != 0.0 { a(3) } else { a(2) };
            format!("+proj=eqdc +lat_0={} +lon_0={} +lat_1={} +lat_2={lat2} {fe}", a(5), a(4), a(2))
        }
        9 => format!("+proj=tmerc +lat_0={} +lon_0={} +k={} {fe}", a(5), a(4), p(2)),
        10 => format!("+proj=stere +lat_0={} +lon_0={} +k=1 {fe}", a(5), a(4)),
        11 => format!("+proj=laea +lat_0={} +lon_0={} {fe}", a(5), a(4)),
        12 => format!("+proj=aeqd +lat_0={} +lon_0={} {fe}", a(5), a(4)),
        13 => format!("+proj=gnom +lat_0={} +lon_0={} {fe}", a(5), a(4)),
        14 => format!("+proj=ortho +lat_0={} +lon_0={} {fe}", a(5), a(4)),
        16 => format!("+proj=sinu +lon_0={} {fe}", a(4)),
        17 => format!("+proj=eqc +lat_ts={} +lat_0=0 +lon_0={} {fe}", a(5), a(4)),
        18 => format!("+proj=mill +lat_0={} +lon_0={} {fe}", a(5), a(4)),
        19 => format!("+proj=vandg +lon_0={} {fe}", a(4)),
        21 => format!("+proj=robin +lon_0={} {fe}", a(4)),
        25 => format!("+proj=moll +lon_0={} {fe}", a(4)),
        28 => format!("+proj=wag4 +lon_0=0 {fe}"),
        29 => format!("+proj=wag7 +lon_0=0 {fe}"),
        _ => return None,
    };
    let ellipsoid = if sphere < 0 {
        if p(0) > 0.0 {
            let rf = if p(1) > 1.0 {
                p(0) / (p(0) - p(1))
            } else if p(1) > 0.0 {
                1.0 / (1.0 - (1.0 - p(1)).sqrt())
            } else {
                0.0
            };
            (p(0), rf)
        } else if p(1) > 0.0 {
            gctp_ellipsoid(0)?
        } else {
            gctp_ellipsoid(19)?
        }
    } else {
        // Codes GDAL cannot map fall back to WGS 84, as GDAL does.
        gctp_ellipsoid(sphere).unwrap_or((6378137.0, 298.257223563))
    };
    if wgs84 && proj == 0 {
        return Some(wbraster::CrsInfo::from_epsg(4326));
    }
    let ellps = match ellipsoid {
        (r, 0.0) => format!("+R={r}"),
        _ if wgs84 => "+datum=WGS84".to_string(),
        (a, rf) => format!("+a={a} +rf={rf}"),
    };
    let units = if proj == 0 { "" } else { " +units=m" };
    let definition = format!("{projection} {ellps}{units} +no_defs");
    let srs = crate::crs::SpatialRef::parse(&definition).ok()?;
    Some(srs.to_crs_info())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SWATH: &str = r#"GROUP=SwathStructure
	GROUP=SWATH_1
		SwathName="MySwath"
		GROUP=Dimension
			OBJECT=Dimension_1
				DimensionName="Band"
				Size=2
			END_OBJECT=Dimension_1
			OBJECT=Dimension_2
				DimensionName="AlongTrack"
				Size=3
			END_OBJECT=Dimension_2
			OBJECT=Dimension_3
				DimensionName="CrossTrack"
				Size=4
			END_OBJECT=Dimension_3
		END_GROUP=Dimension
		GROUP=DimensionMap
		END_GROUP=DimensionMap
		GROUP=GeoField
			OBJECT=GeoField_1
				GeoFieldName="Longitude"
				DataType=H5T_NATIVE_FLOAT
				DimList=("AlongTrack","CrossTrack")
			END_OBJECT=GeoField_1
			OBJECT=GeoField_2
				GeoFieldName="Latitude"
				DataType=H5T_NATIVE_FLOAT
				DimList=("AlongTrack","CrossTrack")
			END_OBJECT=GeoField_2
		END_GROUP=GeoField
		GROUP=DataField
			OBJECT=DataField_1
				DataFieldName="MyDataField"
				DataType=H5T_NATIVE_UCHAR
				DimList=("Band","AlongTrack",
				"CrossTrack")
			END_OBJECT=DataField_1
		END_GROUP=DataField
	END_GROUP=SWATH_1
END_GROUP=SwathStructure
GROUP=GridStructure
END_GROUP=GridStructure
END
"#;

    #[test]
    fn parses_swaths() {
        let eos = Eos::parse(SWATH).unwrap();
        assert!(eos.swath);
        let f = eos.field("//HDFEOS/SWATHS/MySwath/Data_Fields/MyDataField", 3).unwrap();
        assert_eq!((f.x, f.y, f.other), (Some(2), Some(1), Some(0)));
        assert_eq!(f.swath_description().as_deref(), Some("(Band=2)x(AlongTrack=3)x(CrossTrack=4)"));
        let FieldKind::Swath(Some(g)) = &f.kind else { panic!("no geolocation") };
        assert_eq!(g.longitude, "//HDFEOS/SWATHS/MySwath/Geolocation_Fields/Longitude");
        assert_eq!((g.line_offset, g.line_step, g.pixel_offset, g.pixel_step), (0, 1, 0, 1));
        let lat = eos.field("//HDFEOS/SWATHS/MySwath/Geolocation_Fields/Latitude", 2).unwrap();
        assert_eq!((lat.x, lat.y), (Some(1), Some(0)));
    }

    #[test]
    fn parses_grids() {
        let text = "GROUP=GridStructure\n\tGROUP=GRID_1\n\t\tGridName=\"NpPolarGrid12km\"\n\t\tXDim=608\n\t\tYDim=896\n\t\t\
            UpperLeftPointMtrs=(-3850000.000000,5850000.000000)\n\t\tLowerRightMtrs=(3750000.000000,-5350000.000000)\n\t\t\
            Projection=HE5_GCTP_PS\n\t\tProjParams=(6378273,-0.006694,0,0,-45000000,70000000,0,0,0,0,0,0,0)\n\t\tSphereCode=-1\n\t\t\
            GridOrigin=HE5_HDFE_GD_UL\n\t\tGROUP=Dimension\n\t\tEND_GROUP=Dimension\n\t\tGROUP=DataField\n\t\t\tOBJECT=DataField_1\n\t\t\t\t\
            DataFieldName=\"SI_12km_NH_18V_ASC\"\n\t\t\t\tDimList=(\"YDim\",\"XDim\")\n\t\t\tEND_OBJECT=DataField_1\n\t\tEND_GROUP=DataField\n\t\
            END_GROUP=GRID_1\nEND_GROUP=GridStructure\nEND\n";
        let eos = Eos::parse(text).unwrap();
        let f = eos.field("//HDFEOS/GRIDS/NpPolarGrid12km/Data_Fields/SI_12km_NH_18V_ASC", 2).unwrap();
        assert_eq!((f.x, f.y), (Some(1), Some(0)));
        let FieldKind::Grid(g) = &f.kind else { panic!("not a grid") };
        assert_eq!(g.geo_transform(), Some([-3850000.0, 12500.0, 0.0, 5850000.0, 0.0, -12500.0]));
        let crs = crate::crs::SpatialRef::from_raster_crs(&g.crs().unwrap());
        let proj = crs.proj4.clone().unwrap_or_default();
        assert!(proj.contains("+proj=stere") && proj.contains("+lat_ts=70") && proj.contains("+R=6378273"), "{proj}");
    }

    #[test]
    fn converts_packed_dms() {
        assert_eq!(packed_dms_to_degrees(-70000000.0), -70.0);
        assert!((packed_dms_to_degrees(45030030.0) - (45.0 + 30.0 / 60.0 + 30.0 / 3600.0)).abs() < 1e-12);
        assert_eq!(packed_dms_to_degrees(-180000000.0), -180.0);
    }

    #[test]
    fn identifies_wgs84_crs() {
        assert_eq!(gctp_crs(0, 0, &[], 12).unwrap().epsg, Some(4326));
        assert_eq!(gctp_crs(1, 11, &[], 12).unwrap().epsg, Some(32611));
        assert_eq!(gctp_crs(1, -33, &[], 12).unwrap().epsg, Some(32733));
    }
}
