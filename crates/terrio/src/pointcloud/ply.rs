// Written to match the behavior of:
//   PDAL 2.10.2:
//     io/PlyReader.cpp, io/PlyWriter.cpp
// No code is translated from these sources.

//! PLY (Polygon File Format, Stanford): the vertices of ASCII and binary (little- and big-endian)
//! files, read and written as PDAL's `readers.ply` and `writers.ply` do.
//!
//! The `vertex` element's properties are the dimensions: `x`, `y` and `z` as float64, the others
//! of their PLY type. Properties named like PDAL's dimensions, in any case (`red`, `Intensity`,
//! `returnnumber`, as PDAL writes them), take terrio's names (`red`, `intensity`,
//! `return_number`); others keep theirs. Other elements (faces) are skipped.

use super::crs::LasCrs;
use super::las::LasHeader;
use super::read::{FormatReader, Source, apply_options, made_up_header, stride_for};
use super::values::{Dimension, ValueType, Values};
use super::{PlyEncoding, PointCloud, PointFilter, PointReadOptions, PointWriteOptions};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom, Write};

/// A PLY property's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlyType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl PlyType {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "char" | "int8" => PlyType::I8,
            "uchar" | "uint8" => PlyType::U8,
            "short" | "int16" => PlyType::I16,
            "ushort" | "uint16" => PlyType::U16,
            "int" | "int32" => PlyType::I32,
            "uint" | "uint32" => PlyType::U32,
            "float" | "float32" => PlyType::F32,
            "double" | "float64" => PlyType::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            PlyType::I8 | PlyType::U8 => 1,
            PlyType::I16 | PlyType::U16 => 2,
            PlyType::I32 | PlyType::U32 | PlyType::F32 => 4,
            PlyType::F64 => 8,
        }
    }

    fn value_type(self) -> ValueType {
        match self {
            PlyType::I8 => ValueType::I8,
            PlyType::U8 => ValueType::U8,
            PlyType::I16 => ValueType::I16,
            PlyType::U16 => ValueType::U16,
            PlyType::I32 => ValueType::I32,
            PlyType::U32 => ValueType::U32,
            PlyType::F32 => ValueType::F32,
            PlyType::F64 => ValueType::F64,
        }
    }

    /// The PLY type of a column, as PDAL names sized types; 64-bit integers are written as
    /// float64 (PLY has no 64-bit integers).
    fn for_value_type(t: ValueType) -> Self {
        match t {
            ValueType::I8 => PlyType::I8,
            ValueType::U8 => PlyType::U8,
            ValueType::I16 => PlyType::I16,
            ValueType::U16 => PlyType::U16,
            ValueType::I32 => PlyType::I32,
            ValueType::U32 => PlyType::U32,
            ValueType::F32 => PlyType::F32,
            ValueType::F64 | ValueType::I64 | ValueType::U64 => PlyType::F64,
        }
    }

    fn sized_name(self) -> &'static str {
        match self {
            PlyType::I8 => "int8",
            PlyType::U8 => "uint8",
            PlyType::I16 => "int16",
            PlyType::U16 => "uint16",
            PlyType::I32 => "int32",
            PlyType::U32 => "uint32",
            PlyType::F32 => "float32",
            PlyType::F64 => "float64",
        }
    }

    /// Decodes a binary value.
    fn decode(self, b: &[u8], big: bool) -> f64 {
        macro_rules! num {
            ($t:ty, $n:expr) => {{
                let a: [u8; $n] = b[..$n].try_into().unwrap_or_default();
                (if big { <$t>::from_be_bytes(a) } else { <$t>::from_le_bytes(a) }) as f64
            }};
        }
        match self {
            PlyType::I8 => b[0] as i8 as f64,
            PlyType::U8 => b[0] as f64,
            PlyType::I16 => num!(i16, 2),
            PlyType::U16 => num!(u16, 2),
            PlyType::I32 => num!(i32, 4),
            PlyType::U32 => num!(u32, 4),
            PlyType::F32 => num!(f32, 4),
            PlyType::F64 => num!(f64, 8),
        }
    }
}

/// A property of an element: a value, or a list (with the types of its count and items).
#[derive(Debug, Clone)]
enum Property {
    Scalar(String, PlyType),
    List(PlyType, PlyType),
}

#[derive(Debug, Clone)]
struct Element {
    name: String,
    count: u64,
    properties: Vec<Property>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Ascii,
    Le,
    Be,
}

/// Buffered reading of values from a source.
struct ByteReader {
    buf: Vec<u8>,
    at: usize,
    end: usize,
}

impl ByteReader {
    fn new() -> Self {
        Self { buf: vec![0; 1 << 20], at: 0, end: 0 }
    }

    fn fill(&mut self, src: &mut Source) -> Result<bool> {
        if self.at < self.end {
            return Ok(true);
        }
        self.at = 0;
        self.end = src.read(&mut self.buf)?;
        Ok(self.end > 0)
    }

    fn read_exact(&mut self, src: &mut Source, out: &mut [u8]) -> Result<()> {
        let mut k = 0;
        while k < out.len() {
            if !self.fill(src)? {
                return Err(Error::InvalidData("the PLY file ends early".into()));
            }
            let n = (self.end - self.at).min(out.len() - k);
            out[k..k + n].copy_from_slice(&self.buf[self.at..self.at + n]);
            self.at += n;
            k += n;
        }
        Ok(())
    }

    /// The next whitespace-separated word.
    fn word(&mut self, src: &mut Source, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        loop {
            if !self.fill(src)? {
                if out.is_empty() {
                    return Err(Error::InvalidData("the PLY file ends early".into()));
                }
                return Ok(());
            }
            let c = self.buf[self.at];
            if c.is_ascii_whitespace() {
                self.at += 1;
                if !out.is_empty() {
                    return Ok(());
                }
            } else {
                out.push(c);
                self.at += 1;
            }
        }
    }

    fn number(&mut self, src: &mut Source, word: &mut Vec<u8>) -> Result<f64> {
        self.word(src, word)?;
        let s = std::str::from_utf8(word).unwrap_or_default();
        s.parse::<f64>().map_err(|_| Error::InvalidData(format!("PLY: '{s}' is not a number")))
    }
}

/// A PLY file.
pub(crate) struct PlyReader {
    source: Source,
    encoding: Encoding,
    elements: Vec<Element>,
    /// The position of the first element's data.
    data_pos: u64,
    vertex: usize,
    header: LasHeader,
    crs: LasCrs,
    dims: Vec<(String, ValueType)>,
    cursor: Option<PlyCursor>,
    _local: Option<crate::vfs::LocalPath>,
}

struct PlyCursor {
    reader: ByteReader,
    done: u64,
    stride: u64,
    filter: PointFilter,
    chunk: usize,
}

impl PlyReader {
    pub fn new(mut source: Source, local: Option<crate::vfs::LocalPath>) -> Result<Self> {
        source.seek(SeekFrom::Start(0))?;
        // The header: lines of text up to `end_header`.
        let mut text = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if source.read(&mut byte)? == 0 {
                return Err(Error::InvalidData("not a PLY file: no end_header".into()));
            }
            text.push(byte[0]);
            if byte[0] == b'\n' && (text.ends_with(b"end_header\n") || text.ends_with(b"end_header\r\n")) {
                break;
            }
            if text.len() > 1 << 20 {
                return Err(Error::InvalidData("not a PLY file: no end_header in the first megabyte".into()));
            }
        }
        let data_pos = text.len() as u64;
        let text = String::from_utf8_lossy(&text);
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        if lines.next() != Some("ply") {
            return Err(Error::InvalidData("not a PLY file: no 'ply' magic".into()));
        }
        let mut encoding = None;
        let mut elements: Vec<Element> = Vec::new();
        for line in lines {
            let w: Vec<&str> = line.split_whitespace().collect();
            match w[0] {
                "format" => {
                    encoding = Some(match w.get(1).copied() {
                        Some("ascii") => Encoding::Ascii,
                        Some("binary_little_endian") => Encoding::Le,
                        Some("binary_big_endian") => Encoding::Be,
                        f => return Err(Error::InvalidData(format!("unknown PLY format {f:?}"))),
                    });
                    if w.get(2) != Some(&"1.0") {
                        return Err(Error::Unsupported(format!("PLY version {:?}", w.get(2))));
                    }
                }
                "comment" | "obj_info" => {}
                "element" => {
                    let count = w
                        .get(2)
                        .and_then(|c| c.parse::<u64>().ok())
                        .ok_or_else(|| Error::InvalidData(format!("PLY: bad line '{line}'")))?;
                    elements.push(Element { name: w.get(1).unwrap_or(&"").to_string(), count, properties: Vec::new() });
                }
                "property" => {
                    let e = elements.last_mut().ok_or_else(|| Error::InvalidData("PLY: a property before any element".into()))?;
                    let bad = || Error::InvalidData(format!("PLY: bad line '{line}'"));
                    if w.get(1) == Some(&"list") {
                        if e.name == "vertex" {
                            return Err(Error::Unsupported("list properties of the vertex element".into()));
                        }
                        let ct = w.get(2).and_then(|t| PlyType::parse(t)).ok_or_else(bad)?;
                        let it = w.get(3).and_then(|t| PlyType::parse(t)).ok_or_else(bad)?;
                        e.properties.push(Property::List(ct, it));
                    } else {
                        let t = w.get(1).and_then(|t| PlyType::parse(t)).ok_or_else(bad)?;
                        let name = w.get(2).ok_or_else(bad)?;
                        e.properties.push(Property::Scalar(name.to_string(), t));
                    }
                }
                "end_header" => break,
                other => return Err(Error::InvalidData(format!("PLY: unknown keyword '{other}'"))),
            }
        }
        let encoding = encoding.ok_or_else(|| Error::InvalidData("PLY: no format line".into()))?;
        let vertex = elements
            .iter()
            .position(|e| e.name == "vertex")
            .ok_or_else(|| Error::InvalidData("can't read a PLY file without a 'vertex' element".into()))?;
        let mut dims: Vec<(String, ValueType)> = Vec::new();
        for (name, t) in ["x", "y", "z"].map(|n| (n.to_string(), ValueType::F64)) {
            dims.push((name, t));
        }
        for p in &elements[vertex].properties {
            if let Property::Scalar(name, t) = p {
                let n = super::names::from_any(name);
                if !dims.iter().any(|(d, _)| *d == n) {
                    dims.push((n, t.value_type()));
                }
            }
        }
        let crs = LasCrs::default();
        let mut r =
            Self { source, encoding, elements, data_pos, vertex, header: LasHeader::default(), crs, dims, cursor: None, _local: local };
        // The bounds, as PDAL's `pdal info` computes them: from the points.
        let mut bounds: Option<[f64; 6]> = None;
        r.start(&PointReadOptions::default(), 1 << 20)?;
        while let Some(chunk) = r.next_chunk()? {
            let c = PointCloud::new(chunk, Default::default());
            if let Some(b) = c.bounds() {
                bounds = Some(match bounds {
                    None => b,
                    Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2]), a[3].max(b[3]), a[4].max(b[4]), a[5].max(b[5])],
                });
            }
        }
        let count = r.elements[r.vertex].count;
        r.header = made_up_header(&r.dims, count, bounds, &Default::default(), None);
        Ok(r)
    }

    /// Skips the elements before the vertices.
    fn skip_to_vertices(&mut self, reader: &mut ByteReader) -> Result<()> {
        let mut word = Vec::new();
        let before: Vec<Element> = self.elements.iter().take(self.vertex).cloned().collect();
        for e in before {
            for _ in 0..e.count {
                for p in &e.properties {
                    match (self.encoding, p) {
                        (Encoding::Ascii, Property::Scalar(..)) => reader.word(&mut self.source, &mut word)?,
                        (Encoding::Ascii, Property::List(..)) => {
                            let n = reader.number(&mut self.source, &mut word)? as u64;
                            for _ in 0..n {
                                reader.word(&mut self.source, &mut word)?;
                            }
                        }
                        (_, Property::Scalar(_, t)) => reader.read_exact(&mut self.source, &mut [0u8; 8][..t.size()])?,
                        (enc, Property::List(ct, it)) => {
                            let mut b = [0u8; 8];
                            reader.read_exact(&mut self.source, &mut b[..ct.size()])?;
                            let n = ct.decode(&b, enc == Encoding::Be) as u64;
                            // Skip the items through a small buffer: the count is the file's.
                            let mut left = n.saturating_mul(it.size() as u64);
                            let mut skip = [0u8; 4096];
                            while left > 0 {
                                let k = left.min(skip.len() as u64) as usize;
                                reader.read_exact(&mut self.source, &mut skip[..k])?;
                                left -= k as u64;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl FormatReader for PlyReader {
    fn header(&self) -> &LasHeader {
        &self.header
    }

    fn crs(&self) -> &LasCrs {
        &self.crs
    }

    fn dimensions(&self) -> Vec<(String, ValueType)> {
        self.dims.clone()
    }

    fn format_name(&self) -> &'static str {
        "PLY"
    }

    fn start(&mut self, options: &PointReadOptions, chunk: usize) -> Result<()> {
        self.source.seek(SeekFrom::Start(self.data_pos))?;
        let mut reader = ByteReader::new();
        self.skip_to_vertices(&mut reader)?;
        let n = self.elements[self.vertex].count;
        self.cursor = Some(PlyCursor { reader, done: 0, stride: stride_for(n, options.max_points), filter: options.filter.clone(), chunk });
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<Option<Vec<Dimension>>> {
        let Some(mut c) = self.cursor.take() else { return Ok(None) };
        let n = self.elements[self.vertex].count;
        if c.done >= n {
            return Ok(None);
        }
        let props: Vec<(usize, PlyType)> = self.elements[self.vertex]
            .properties
            .iter()
            .filter_map(|p| match p {
                Property::Scalar(name, t) => {
                    let n = super::names::from_any(name);
                    self.dims.iter().position(|(d, _)| *d == n).map(|k| (k, *t))
                }
                Property::List(..) => None,
            })
            .collect();
        let k = ((n - c.done) as usize).min(c.chunk);
        let mut cols: Vec<Values> = self.dims.iter().map(|(_, t)| Values::with_capacity(*t, k)).collect();
        let mut word = Vec::new();
        let mut b = [0u8; 8];
        let big = self.encoding == Encoding::Be;
        for _ in 0..k {
            for (col, t) in &props {
                let v = if self.encoding == Encoding::Ascii {
                    c.reader.number(&mut self.source, &mut word)?
                } else {
                    c.reader.read_exact(&mut self.source, &mut b[..t.size()])?;
                    t.decode(&b, big)
                };
                cols[*col].push_f64(v);
            }
        }
        // Dimensions the vertices have no property for (z, sometimes) are zeros.
        for (col, (_, t)) in cols.iter_mut().zip(&self.dims) {
            if col.len() < k {
                *col = Values::zeros(*t, k);
            }
        }
        let dims: Vec<Dimension> = self.dims.iter().zip(cols).map(|((name, _), v)| Dimension::new(name, v)).collect();
        let first = c.done;
        c.done += k as u64;
        let out = apply_options(dims, &c.filter, c.stride, first);
        self.cursor = Some(c);
        Ok(Some(out))
    }
}

/// The columns written, with the PLY names and types.
fn columns(cloud: &PointCloud, options: &PointWriteOptions) -> Result<Vec<(String, PlyType, usize)>> {
    let pick: Vec<usize> = if options.dimensions.is_empty() {
        (0..cloud.dimensions.len()).collect()
    } else {
        options
            .dimensions
            .iter()
            .map(|n| {
                cloud.dimensions.iter().position(|d| d.name == *n).ok_or_else(|| Error::InvalidArgument(format!("no dimension '{n}'")))
            })
            .collect::<Result<_>>()?
    };
    Ok(pick
        .into_iter()
        .map(|k| {
            let d = &cloud.dimensions[k];
            let scaled = d.scale.is_some() || d.offset.is_some();
            let t = if scaled { PlyType::F64 } else { PlyType::for_value_type(d.values.value_type()) };
            (super::names::ply_name(&d.name), t, k)
        })
        .collect())
}

/// Writes PLY vertices as they come; the count is patched in the header at the end.
pub(crate) struct PlyStreamWriter<W: Write + Seek> {
    w: W,
    cols: Vec<(String, PlyType, usize)>,
    names: Vec<String>,
    encoding: PlyEncoding,
    precision: Option<usize>,
    count_pos: u64,
    count: u64,
}

/// The room left for the vertex count in the header, padded with spaces.
const COUNT_WIDTH: usize = 20;

impl<W: Write + Seek> PlyStreamWriter<W> {
    pub fn new(mut w: W, template: &PointCloud, options: &PointWriteOptions) -> Result<Self> {
        let cols = columns(template, options)?;
        let names = cols.iter().map(|(_, _, k)| template.dimensions[*k].name.clone()).collect();
        let format = match options.ply_encoding {
            PlyEncoding::Ascii => "ascii",
            PlyEncoding::BinaryLittleEndian => "binary_little_endian",
            PlyEncoding::BinaryBigEndian => "binary_big_endian",
        };
        write!(w, "ply\nformat {format} 1.0\ncomment Generated by terrio {}\nelement vertex ", crate::VERSION)?;
        let count_pos = w.stream_position()?;
        writeln!(w, "{:<COUNT_WIDTH$}", 0)?;
        for (name, t, _) in &cols {
            writeln!(w, "property {} {name}", t.sized_name())?;
        }
        writeln!(w, "end_header")?;
        Ok(Self { w, cols, names, encoding: options.ply_encoding, precision: options.ply_precision, count_pos, count: 0 })
    }

    pub fn write_cloud(&mut self, cloud: &PointCloud) -> Result<()> {
        let dims: Vec<&Dimension> = self
            .names
            .iter()
            .map(|n| cloud.dim(n).ok_or_else(|| Error::InvalidArgument(format!("the points have no '{n}'"))))
            .collect::<Result<_>>()?;
        let mut line = String::new();
        let mut bytes = Vec::new();
        for i in 0..cloud.len() {
            line.clear();
            for (j, ((_, t, _), d)) in self.cols.iter().zip(&dims).enumerate() {
                let v = if d.scale.is_some() || d.offset.is_some() { d.get(i) } else { d.values.get_f64(i) };
                match self.encoding {
                    PlyEncoding::Ascii => {
                        use std::fmt::Write as _;
                        if j > 0 {
                            line.push(' ');
                        }
                        let _ = match (t, self.precision) {
                            (PlyType::F32, Some(p)) => write!(line, "{:.p$}", v as f32),
                            (PlyType::F64, Some(p)) => write!(line, "{v:.p$}"),
                            (PlyType::F32, None) => write!(line, "{}", v as f32),
                            (PlyType::F64, None) => write!(line, "{v}"),
                            _ => write!(line, "{}", v as i64),
                        };
                    }
                    enc => {
                        let big = enc == PlyEncoding::BinaryBigEndian;
                        macro_rules! put {
                            ($x:expr) => {
                                if big { bytes.extend_from_slice(&$x.to_be_bytes()) } else { bytes.extend_from_slice(&$x.to_le_bytes()) }
                            };
                        }
                        match t {
                            PlyType::I8 => put!(v as i8),
                            PlyType::U8 => put!(v as u8),
                            PlyType::I16 => put!(v as i16),
                            PlyType::U16 => put!(v as u16),
                            PlyType::I32 => put!(v as i32),
                            PlyType::U32 => put!(v as u32),
                            PlyType::F32 => put!(v as f32),
                            PlyType::F64 => put!(v),
                        }
                    }
                }
            }
            if self.encoding == PlyEncoding::Ascii {
                line.push('\n');
                self.w.write_all(line.as_bytes())?;
            }
        }
        self.w.write_all(&bytes)?;
        self.count += cloud.len() as u64;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        let end = self.w.stream_position()?;
        self.w.seek(SeekFrom::Start(self.count_pos))?;
        write!(self.w, "{:<COUNT_WIDTH$}", self.count)?;
        self.w.seek(SeekFrom::Start(end))?;
        self.w.flush()?;
        Ok(())
    }
}

/// Writes a point cloud as a PLY file.
pub(crate) fn write_ply<W: Write + Seek>(cloud: &PointCloud, w: &mut W, options: &PointWriteOptions) -> Result<()> {
    let mut pw = PlyStreamWriter::new(w, cloud, options)?;
    pw.write_cloud(cloud)?;
    pw.finish()
}
