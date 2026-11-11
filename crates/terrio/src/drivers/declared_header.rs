// SPDX-FileCopyrightText: Copyright (c) 2024 Whitebox Geospatial Inc.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Header layouts adapted from wbraster 0.2.1 src/formats/{dted,pcraster,esri_binary,grass_ascii}.rs.
// ASCII format detection adapted from that version's src/formats/mod.rs.
// Only declarations are read here; the original reader still validates and decodes the file.

//! Declaration checks for readers without a separate metadata API. Reference: GDAL 3.13.3.

use crate::{DataType, Result, formats::RasterDriver};
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;

/// Checks the header before the fallback codec reads its file or allocates pixel buffers.
/// Unrecognized/truncated headers keep the original reader's diagnostics.
pub(crate) fn check(path: &Path, driver: RasterDriver) -> Result<()> {
    check_type(path, driver, None)
}

/// The fallback ASCII codec always reads Float64 samples, unlike the native ASCII reader.
pub(crate) fn check_fallback(path: &Path, driver: RasterDriver) -> Result<()> {
    check_type(path, driver, (driver == RasterDriver::AAIGrid).then_some(DataType::F64))
}

fn check_type(path: &Path, driver: RasterDriver, sample_type: Option<DataType>) -> Result<()> {
    if !matches!(driver, RasterDriver::AAIGrid | RasterDriver::GRASSASCIIGrid)
        && crate::config::get("TERRIO_MAX_DECLARED_BYTES").is_none()
        && crate::config::get("TERRIO_MAX_BANDS").is_none()
    {
        return Ok(());
    }
    // Validate the options even if this header cannot supply a shape.
    crate::declared::check_bytes(0, 0, 0, 0, false)?;
    let header_path = if driver == RasterDriver::AIG {
        if path.is_dir() { path.join("hdr.adf") } else { path.with_file_name("hdr.adf") }
    } else {
        path.to_path_buf()
    };
    let Ok(file) = std::fs::File::open(header_path) else { return Ok(()) };
    let dimensions = if matches!(driver, RasterDriver::AAIGrid | RasterDriver::GRASSASCIIGrid) {
        let lines = header_lines(BufReader::new(file), driver, sample_type.is_some());
        if sample_type.is_some() { fallback_ascii_shape(lines) } else { ascii_shape(lines, driver) }
    } else {
        let mut bytes = Vec::new();
        if file.take(256).read_to_end(&mut bytes).is_err() {
            return Ok(());
        }
        shape(&bytes, driver)
    };
    if let Some((cols, rows, data_type)) = dimensions {
        crate::declared::check(cols, rows, 1, sample_type.unwrap_or(data_type), false)?;
        if matches!(driver, RasterDriver::AAIGrid | RasterDriver::GRASSASCIIGrid) {
            // The GRASS fallback builds a Float64 working vector even for integer output.
            let working_type = if driver == RasterDriver::GRASSASCIIGrid { DataType::F64 } else { sample_type.unwrap_or(data_type) };
            check_ascii_allocation(cols, rows, working_type)?;
        }
    }
    Ok(())
}

/// Match the fallback's first 32 raw lines with bounded memory, including unknown fields.
/// Detection must not reuse the declaration parser's canonicalized numeric/type fields.
pub(crate) fn detect_ascii(path: &str) -> Result<Option<wbraster::RasterFormat>> {
    let p = Path::new(path);
    if !p.is_file() || !matches!(crate::formats::extension(path).as_deref(), Some("asc" | "txt")) {
        return Ok(None);
    }
    let mut file = std::fs::File::open(p)?;
    let mut signature = Vec::new();
    file.by_ref().take(32).read_to_end(&mut signature)?;
    if signature.starts_with(b"RUU CROSS SYSTEM MAP FORMAT") {
        return Ok(Some(wbraster::RasterFormat::Pcraster));
    }
    file.rewind()?;
    detect_ascii_reader(BufReader::new(file)).map(Some)
}

fn detect_ascii_reader(mut reader: impl BufRead) -> Result<wbraster::RasterFormat> {
    let mut grass = false;
    for _ in 0..32 {
        let Some(line) = detection_line(&mut reader)? else { break };
        if let Some((key, _)) = wbraster::io_utils::parse_key_value(&line)
            && matches!(
                key.as_str(),
                "ncols" | "nrows" | "xllcorner" | "yllcorner" | "xllcenter" | "yllcenter" | "cellsize" | "nodata_value"
            )
        {
            // An Esri key decides the result regardless of the following lines.
            return Ok(wbraster::RasterFormat::EsriAscii);
        }
        if let Some((key, _)) = line.split_once(':') {
            grass |=
                matches!(key.trim().to_ascii_lowercase().as_str(), "north" | "south" | "east" | "west" | "rows" | "cols" | "null" | "type");
        }
    }
    Ok(if grass { wbraster::RasterFormat::GrassAscii } else { wbraster::RasterFormat::EsriAscii })
}

/// Retain only a short significant prefix, compressing whitespace runs without changing
/// whether they contain an ASCII key separator or the character preceding a hash comment.
/// Consuming a long unknown line can require I/O, but never a line-sized allocation.
fn detection_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let (mut ascii_space, mut last_space) = (None, None);
    let mut present = false;
    while let Some(c) = read_char(reader)? {
        present = true;
        if c == '\n' {
            break;
        }
        if line.len() >= 128 {
            // The retained non-whitespace key prefix is already longer than every known key.
            continue;
        }
        if c.is_whitespace() {
            if !line.is_empty() {
                if c.is_ascii_whitespace() {
                    ascii_space = Some(c);
                }
                last_space = Some(c);
            }
        } else {
            if let Some(space) = ascii_space.take() {
                line.push(space);
            }
            if let Some(space) = last_space.take() {
                line.push(space);
            }
            line.push(c);
        }
    }
    Ok(present.then_some(line))
}

/// Read one UTF-8 character without buffering an arbitrarily padded line.
fn read_char(reader: &mut impl BufRead) -> std::io::Result<Option<char>> {
    let Some(&first) = reader.fill_buf()?.first() else { return Ok(None) };
    reader.consume(1);
    if first.is_ascii() {
        return Ok(Some(char::from(first)));
    }
    let width = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Err(std::io::ErrorKind::InvalidData.into()),
    };
    let mut bytes = [0; 4];
    bytes[0] = first;
    reader.read_exact(&mut bytes[1..width])?;
    let text = std::str::from_utf8(&bytes[..width]).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    Ok(text.chars().next())
}

/// Intrinsic allocation bounds apply even when configurable declaration limits are unset.
pub(crate) fn check_ascii_allocation(cols: usize, rows: usize, data_type: DataType) -> Result<()> {
    let bytes = cols.checked_mul(rows).and_then(|n| n.checked_mul(data_type.size_bytes())).filter(|&n| n <= isize::MAX as usize);
    crate::dataset::fallible::check_usable(bytes, || format!("an ASCII raster of {cols} x {rows} cells is too large to allocate"))
        .map(|_| ())
        .map_err(crate::Error::ResourceLimit)
}

/// The in-memory ASCII codec reads Float64 samples regardless of the values in its document.
/// Keep that sample type while checking its header before the codec parses any pixel tokens.
pub(crate) fn check_ascii_bytes(bytes: &[u8]) -> Result<()> {
    crate::declared::check_bytes(0, 0, 0, 0, false)?;
    // Invalid UTF-8 later in the pixel body cannot hide an oversized valid header.
    let lines = header_lines(std::io::Cursor::new(bytes), RasterDriver::AAIGrid, true);
    if let Some((cols, rows, data_type)) = fallback_ascii_shape(lines) {
        crate::declared::check(cols, rows, 1, data_type, false)?;
        check_ascii_allocation(cols, rows, data_type)?;
    }
    Ok(())
}

/// The fallback and byte ASCII codecs share key/comment rules and always produce Float64.
fn fallback_ascii_shape<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> Option<(usize, usize, DataType)> {
    let (mut cols, mut rows) = (None, None);
    for line in lines {
        let line = line.as_ref().trim();
        if line.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+') {
            break;
        }
        if let Some((key, value)) = wbraster::io_utils::parse_key_value(line) {
            match key.as_str() {
                "ncols" => cols = value.parse().ok(),
                "nrows" => rows = value.parse().ok(),
                _ => {}
            }
        }
    }
    Some((cols?, rows?, DataType::F64))
}

/// A streaming unsigned field, with the fallback Esri reader's inline comment rules.
#[derive(Default)]
struct AsciiNumber {
    number: usize,
    digits: bool,
    plus: bool,
    ended: bool,
    invalid: bool,
    comment: bool,
    slash: bool,
    previous: Option<char>,
}

impl AsciiNumber {
    fn feed(&mut self, c: char, native: bool, esri: bool) {
        if self.comment || (native && self.ended) {
            return;
        }
        if esri {
            if c == '/' && self.slash {
                self.comment = true;
                return;
            }
            if self.slash {
                self.invalid = true;
            }
            self.slash = c == '/';
            if self.slash {
                return;
            }
            if c == '#' && self.previous.is_none_or(|v| matches!(v, ' ' | '\t')) {
                self.comment = true;
                return;
            }
            self.previous = Some(c);
        }
        if c.is_whitespace() {
            self.ended |= self.digits || self.plus;
        } else if esri && c == '=' && !self.digits && !self.plus && !self.ended {
            // Separators can extend past the retained prefix.
        } else if c == '+' && !self.plus && !self.digits && !self.ended {
            self.plus = true;
        } else if c.is_ascii_digit() && !self.ended {
            self.digits = true;
            match self.number.checked_mul(10).and_then(|n| n.checked_add(c as usize - '0' as usize)) {
                Some(n) => self.number = n,
                None => self.invalid = true,
            }
        } else {
            self.invalid = true;
        }
    }

    fn value(&self) -> Option<usize> {
        (self.digits && !self.invalid && (!self.slash || self.comment)).then_some(self.number)
    }
}

/// Keep a bounded metadata prefix and stream oversized numeric/type fields. Native Esri,
/// fallback Esri and GRASS have different header boundaries; preserve each reader's rule.
fn header_lines(mut reader: impl BufRead, driver: RasterDriver, fallback: bool) -> impl Iterator<Item = String> {
    let mut data = false;
    std::iter::from_fn(move || {
        if data {
            return None;
        }
        const PREFIX: usize = 64 << 10;
        let first = loop {
            let c = read_char(&mut reader).ok()??;
            if c == '\n' || !c.is_whitespace() {
                break c;
            }
        };
        let mut line = String::new();
        line.push(first);
        while line.len() < PREFIX && !line.ends_with('\n') {
            let Some(c) = read_char(&mut reader).ok()? else { break };
            line.push(c);
        }
        if line.len() < PREFIX || line.ends_with('\n') {
            return Some(line);
        }
        let grass = driver == RasterDriver::GRASSASCIIGrid;
        let trimmed = line.trim_start();
        let native = !grass && !fallback;
        let numeric_data = trimmed.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+');
        let native_data =
            !super::aaigrid::HEADER_KEYS.contains(&trimmed.split_whitespace().next().unwrap_or("").to_ascii_lowercase().as_str());
        if !grass && (if native { native_data } else { numeric_data }) {
            data = true;
            return Some(line);
        }
        let split = |c: char| if native { c.is_whitespace() } else { c == '=' || c.is_ascii_whitespace() };
        let (key, value, mut awaiting_colon) = if grass {
            if let Some((k, v)) = trimmed.split_once(':') {
                (k.trim(), v, false)
            } else {
                let (k, v) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
                (k, v, true)
            }
        } else {
            let (k, v) = trimmed.split_once(split).unwrap_or((trimmed, ""));
            (k.trim(), v, false)
        };
        let key = key.to_ascii_lowercase();
        let numeric = if grass { matches!(key.as_str(), "cols" | "rows") } else { matches!(key.as_str(), "ncols" | "nrows") };
        let typed = grass && key == "type";
        let mut number = AsciiNumber::default();
        let (mut colon_seen, mut key_valid) = (grass && !awaiting_colon, true);
        let (mut double, mut float) = (false, false);
        let mut tail = [0u8; 6];
        let mut feed = |c: char| {
            colon_seen |= c == ':';
            if awaiting_colon {
                if c == ':' {
                    awaiting_colon = false;
                } else if !c.is_whitespace() {
                    key_valid = false;
                }
                return;
            }
            if numeric {
                number.feed(c, native, fallback);
            }
            if typed {
                tail.rotate_left(1);
                tail[5] = if c.is_ascii() { c.to_ascii_lowercase() as u8 } else { 0 };
                double |= &tail == b"double";
                float |= &tail[1..] == b"float";
            }
        };
        for c in value.chars() {
            feed(c);
        }
        while let Some(c) = read_char(&mut reader).ok()? {
            feed(c);
            if c == '\n' {
                break;
            }
        }
        if grass && (!colon_seen || !key_valid) {
            return Some(if colon_seen { "ignored: header".to_string() } else { "pixel data".to_string() });
        }
        if numeric {
            // A comment-only fallback line has no pair, and must not erase an earlier key.
            if fallback && !number.digits && !number.plus && !number.invalid && (!number.slash || number.comment) {
                line = "ignored header".to_string();
            } else {
                let value = number.value().map_or_else(|| "invalid".to_string(), |n| n.to_string());
                line = format!("{key}{} {value}", if grass { ":" } else { "" });
            }
        } else if typed {
            line = format!(
                "type: {}",
                if double {
                    "double"
                } else if float {
                    "float"
                } else {
                    "int"
                }
            );
        } else if grass {
            line = "ignored: header".to_string();
        } else if native && key == "nodata_value" && !value.trim().contains(char::is_whitespace) {
            // The native decoder repeats the exact type and allocation checks before pixels.
            line = "nodata_value null".to_string();
        }
        Some(line)
    })
}

fn shape(bytes: &[u8], driver: RasterDriver) -> Option<(usize, usize, DataType)> {
    match driver {
        RasterDriver::DTED if bytes.starts_with(b"UHL1") => {
            let number = |range: std::ops::Range<usize>| std::str::from_utf8(bytes.get(range)?).ok()?.trim().parse().ok();
            Some((number(47..51)?, number(51..55)?, DataType::I16))
        }
        RasterDriver::AIG if bytes.starts_with(&[0, 0, 0x27, 0x0a, 0xff, 0xff, 0xfb, 0xf8]) => {
            let number = |range: std::ops::Range<usize>| usize::try_from(i32::from_be_bytes(bytes.get(range)?.try_into().ok()?)).ok();
            Some((number(16..20)?, number(20..24)?, DataType::F32))
        }
        RasterDriver::PCRaster if bytes.starts_with(b"RUU CROSS SYSTEM MAP FORMAT") => {
            let little = match bytes.get(46..50)? {
                [1, 0, 0, 0] => true,
                [0, 0, 0, 1] => false,
                _ => return None,
            };
            let raw: [u8; 2] = bytes.get(66..68)?.try_into().ok()?;
            let representation = if little { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) };
            let data_type = match representation {
                0 => DataType::U8,
                0x26 => DataType::I32,
                0x5a => DataType::F32,
                0xdb => DataType::F64,
                _ => return None,
            };
            let number = |range: std::ops::Range<usize>| {
                let raw = bytes.get(range)?.try_into().ok()?;
                usize::try_from(if little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) }).ok()
            };
            Some((number(104..108)?, number(100..104)?, data_type))
        }
        _ => None,
    }
}

fn ascii_shape<S: AsRef<str>>(lines: impl Iterator<Item = S>, driver: RasterDriver) -> Option<(usize, usize, DataType)> {
    let (mut cols, mut rows) = (None, None);
    let mut data_type = if driver == RasterDriver::AAIGrid { DataType::I32 } else { DataType::F64 };
    let mut nodata_seen = false;
    for line in lines {
        let line = line.as_ref().trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = if driver == RasterDriver::AAIGrid {
            let mut fields = line.split_whitespace();
            let key = fields.next()?;
            if !super::aaigrid::HEADER_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                break;
            }
            (key, fields.next()?)
        } else {
            let Some(pair) = line.split_once(':') else { break };
            (pair.0.trim(), pair.1.trim())
        };
        let key = key.to_ascii_lowercase();
        if driver == RasterDriver::AAIGrid && !super::aaigrid::HEADER_KEYS.contains(&key.as_str()) {
            break;
        }
        match key.as_str() {
            "cols" if driver == RasterDriver::GRASSASCIIGrid => {
                cols = Some(value.parse().ok()?);
            }
            "ncols" if driver == RasterDriver::AAIGrid => {
                if driver != RasterDriver::AAIGrid || cols.is_none() {
                    cols = Some(value.parse().ok()?);
                }
            }
            "rows" if driver == RasterDriver::GRASSASCIIGrid => {
                rows = Some(value.parse().ok()?);
            }
            "nrows" if driver == RasterDriver::AAIGrid => {
                if driver != RasterDriver::AAIGrid || rows.is_none() {
                    rows = Some(value.parse().ok()?);
                }
            }
            "type" if driver == RasterDriver::GRASSASCIIGrid => {
                let value = value.to_ascii_lowercase();
                data_type = if value.contains("double") {
                    DataType::F64
                } else if value.contains("float") {
                    DataType::F32
                } else {
                    DataType::I32
                };
            }
            "nodata_value" if driver == RasterDriver::AAIGrid && !nodata_seen => {
                nodata_seen = true;
                if let Some(v) = super::common::parse_f64(value) {
                    data_type = super::aaigrid::nodata_type(value, v);
                }
            }
            _ => {}
        }
    }
    Some((cols?, rows?, data_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: GDAL 3.13.3. Invalid pixel encoding cannot bypass a valid ASCII header.
    #[test]
    fn byte_header_limit_precedes_invalid_utf8_pixels() {
        let mut bytes = b"ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2\n3 ".to_vec();
        bytes.push(0xff);
        crate::config::with_options(&[("TERRIO_MAX_DECLARED_BYTES", "31")], || {
            assert!(matches!(check_ascii_bytes(&bytes), Err(crate::Error::ResourceLimit(_))));
        });
        crate::config::with_options(&[("TERRIO_MAX_DECLARED_BYTES", "32")], || {
            check_ascii_bytes(&bytes).unwrap();
            assert!(wbraster::formats::esri_ascii::parse(std::io::Cursor::new(&bytes), "pixels.asc").is_err());
        });
    }

    #[test]
    fn byte_headers_enforce_intrinsic_allocation_bounds_without_options() {
        crate::config::push_scope([("TERRIO_MAX_DECLARED_BYTES".into(), None), ("TERRIO_MAX_BANDS".into(), None)].into());
        let text = format!("ncols {}\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n0\n", usize::MAX);
        let result = check_ascii_bytes(text.as_bytes());
        crate::config::pop_scope();
        assert!(matches!(result, Err(crate::Error::ResourceLimit(_))), "{result:?}");
    }

    #[test]
    fn fallback_headers_keep_unknown_null_lines_and_inline_comments() {
        for token in ["nan", "inf", "*"] {
            let text =
                format!("ncols 2\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n{token} {}\nncols 3\n1 2 3\n", "ignored".repeat(12000));
            let reference = wbraster::formats::esri_ascii::parse(std::io::Cursor::new(text.as_bytes()), "grid.asc").unwrap();
            assert_eq!((reference.cols, reference.rows), (3, 1));
            crate::config::with_options(&[("TERRIO_MAX_DECLARED_BYTES", "23")], || {
                assert!(matches!(check_ascii_bytes(text.as_bytes()), Err(crate::Error::ResourceLimit(_))), "{token}");
            });
        }
        for suffix in ["", "# ignored", "// ignored"] {
            let text = format!("ncols 3\nncols {}{suffix}\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2 3\n", " ".repeat(128 << 10));
            let reference = wbraster::formats::esri_ascii::parse(std::io::Cursor::new(text.as_bytes()), "grid.asc").unwrap();
            assert_eq!(reference.cols, 3);
            crate::config::with_options(&[("TERRIO_MAX_DECLARED_BYTES", "23")], || {
                assert!(matches!(check_ascii_bytes(text.as_bytes()), Err(crate::Error::ResourceLimit(_))), "{suffix}");
            });
        }
        let text = format!(
            "ncols {}{}3// trailing comment\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2 3\n",
            "\u{2003}".repeat(40000),
            "0".repeat(70000)
        );
        let reference = wbraster::formats::esri_ascii::parse(std::io::Cursor::new(text.as_bytes()), "grid.asc").unwrap();
        assert_eq!((reference.cols, reference.rows), (3, 1));
        crate::config::with_options(&[("TERRIO_MAX_DECLARED_BYTES", "23")], || {
            assert!(matches!(check_ascii_bytes(text.as_bytes()), Err(crate::Error::ResourceLimit(_))));
        });
    }

    #[test]
    fn grass_shape_ignores_esri_only_dimension_keys() {
        let shape = ascii_shape(["rows: 1", "cols: 3", "type: double", "ncols: 0"].into_iter(), RasterDriver::GRASSASCIIGrid);
        assert_eq!(shape, Some((3, 1, DataType::F64)));
    }

    #[test]
    fn bounded_detection_matches_the_fallback_on_long_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grid.txt");
        let esri = "ncols 3\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2 3\n";
        let grass = "north: 1\nsouth: 0\neast: 3\nwest: 0\nrows: 1\ncols: 3\ntype: double\n1 2 3\n";
        let mut cases: Vec<String> = [
            format!("rows {}\n", "0".repeat(128 << 10)),
            format!("type {}float\n", " ".repeat(128 << 10)),
            format!("rows {}: ignored\n", "0".repeat(128 << 10)),
        ]
        .into_iter()
        .map(|prefix| format!("{prefix}{}{esri}", "unknown header\n".repeat(32)))
        .collect();
        cases.push(format!("ncols:{}\n{grass}", "0".repeat(128 << 10)));
        cases.push("DSAA\n3 1\n0 3\n0 1\n1 3\n1 2 3\n".to_string());
        cases.push("arbitrary non-grid text\n".to_string());
        for text in cases {
            std::fs::write(&path, text).unwrap();
            let reference = wbraster::RasterFormat::detect(path.to_str().unwrap()).unwrap();
            for limit in [None, Some("24".to_string())] {
                crate::config::push_scope([("TERRIO_MAX_DECLARED_BYTES".into(), limit), ("TERRIO_MAX_BANDS".into(), None)].into());
                let actual = detect_ascii(path.to_str().unwrap()).unwrap();
                crate::config::pop_scope();
                assert_eq!(actual, Some(reference));
            }
        }
    }
    /// Detection stops after its first Esri header; measure logical and buffered I/O.
    #[test]
    fn esri_detection_stops_after_its_first_header_line() {
        struct Counted<'a> {
            remaining: &'a [u8],
            bytes: usize,
        }
        impl Read for Counted<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.remaining.read(buf)?;
                self.bytes += n;
                Ok(n)
            }
        }
        impl BufRead for Counted<'_> {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                Ok(self.remaining)
            }
            fn consume(&mut self, n: usize) {
                self.remaining = &self.remaining[n..];
                self.bytes += n;
            }
        }
        let text = format!("ncols 3\nnrows 1\n{}", "1 ".repeat(1 << 20));
        let mut reader = Counted { remaining: text.as_bytes(), bytes: 0 };
        assert_eq!(detect_ascii_reader(&mut reader).unwrap(), wbraster::RasterFormat::EsriAscii);
        assert_eq!(reader.bytes, "ncols 3\n".len());
        let mut reader = Counted { remaining: text.as_bytes(), bytes: 0 };
        let buffered = BufReader::with_capacity(8192, &mut reader);
        assert_eq!(detect_ascii_reader(buffered).unwrap(), wbraster::RasterFormat::EsriAscii);
        assert_eq!(reader.bytes, 8192, "the buffer reads ahead, without fetching the remaining pixel rows");
    }
}
