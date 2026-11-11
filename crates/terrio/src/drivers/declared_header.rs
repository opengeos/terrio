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
    if crate::config::get("TERRIO_MAX_DECLARED_BYTES").is_none() && crate::config::get("TERRIO_MAX_BANDS").is_none() {
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
        let lines = header_lines(BufReader::new(file), driver);
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
    }
    Ok(())
}

/// Match the fallback ASCII format detection without its unbounded line buffers. Only active
/// under declaration limits; other names and PCRaster signatures keep the original detector.
pub(crate) fn detect_ascii(path: &str) -> Result<Option<wbraster::RasterFormat>> {
    if crate::config::get("TERRIO_MAX_DECLARED_BYTES").is_none() && crate::config::get("TERRIO_MAX_BANDS").is_none() {
        return Ok(None);
    }
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
    let mut esri = false;
    let mut grass = false;
    for line in header_lines(BufReader::new(file), RasterDriver::GRASSASCIIGrid).take(32) {
        if let Some((key, _)) = wbraster::io_utils::parse_key_value(&line) {
            esri |= super::aaigrid::HEADER_KEYS.contains(&key.as_str());
        }
        if let Some((key, _)) = line.split_once(':') {
            grass |=
                matches!(key.trim().to_ascii_lowercase().as_str(), "north" | "south" | "east" | "west" | "rows" | "cols" | "null" | "type");
        }
    }
    Ok(Some(if grass && !esri { wbraster::RasterFormat::GrassAscii } else { wbraster::RasterFormat::EsriAscii }))
}

/// The in-memory ASCII codec reads Float64 samples regardless of the values in its document.
/// Keep that sample type while checking its header before the codec parses any pixel tokens.
pub(crate) fn check_ascii_bytes(bytes: &[u8]) -> Result<()> {
    let Ok(text) = std::str::from_utf8(bytes) else { return Ok(()) };
    if let Some((cols, rows, data_type)) = fallback_ascii_shape(text.lines()) {
        crate::declared::check(cols, rows, 1, data_type, false)?;
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

/// Keep the metadata prefix of a line, not a possibly enormous unwrapped pixel row.
/// Long metadata lines are consumed without retaining their tails. GRASS dimensions and type
/// are summarized while streaming so even long numeric/type fields keep their actual meaning.
fn header_lines(mut reader: impl BufRead, driver: RasterDriver) -> impl Iterator<Item = String> {
    let mut data = false;
    std::iter::from_fn(move || {
        if data {
            return None;
        }
        const PREFIX: u64 = 64 << 10;
        // Leading header padding must not consume the bounded prefix before its key is seen.
        loop {
            let part = reader.fill_buf().ok()?;
            if part.is_empty() {
                return None;
            }
            let n = part.iter().take_while(|&&b| matches!(b, b' ' | b'\t' | b'\r')).count();
            let more = n == part.len();
            reader.consume(n);
            if !more {
                break;
            }
        }
        let mut bytes = Vec::new();
        reader.by_ref().take(PREFIX).read_until(b'\n', &mut bytes).ok()?;
        if bytes.is_empty() {
            return None;
        }
        let mut line = String::from_utf8_lossy(&bytes).into_owned();
        if bytes.len() < PREFIX as usize || bytes.ends_with(b"\n") {
            return Some(line);
        }
        let trimmed = line.trim_start();
        // Numeric cells (or the standard ASCII null tokens) start the data, not another header.
        if trimmed.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+' || c == '*')
            || trimmed.split_ascii_whitespace().next().is_some_and(|v| v.eq_ignore_ascii_case("nan") || v.eq_ignore_ascii_case("inf"))
        {
            data = true;
            return Some(line);
        }
        let pair = if driver == RasterDriver::AAIGrid { trimmed.split_once(char::is_whitespace) } else { trimmed.split_once(':') };
        let (key, value) =
            pair.map_or_else(|| (trimmed.split_ascii_whitespace().next().unwrap_or(""), ""), |(k, v)| (k.trim(), v.trim_start()));
        let key = key.to_ascii_lowercase();
        let mut colon_seen = trimmed.contains(':');
        let mut awaiting_colon = driver == RasterDriver::GRASSASCIIGrid && !colon_seen;
        let numeric = matches!(key.as_str(), "cols" | "rows" | "ncols" | "nrows");
        let typed = driver == RasterDriver::GRASSASCIIGrid && key == "type";
        let first_only = driver == RasterDriver::AAIGrid;
        let (mut number, mut started, mut ended, mut invalid) = (0usize, false, false, false);
        let (mut double, mut float) = (false, false);
        let mut tail = [0u8; 6];
        let mut feed = |part: &[u8]| {
            colon_seen |= part.contains(&b':');
            if !numeric && !typed {
                return;
            }
            for &b in part {
                if awaiting_colon {
                    if b == b':' {
                        awaiting_colon = false;
                    }
                    continue;
                }
                if numeric && !(first_only && ended) {
                    if b.is_ascii_digit() && !ended {
                        started = true;
                        match number.checked_mul(10).and_then(|v| v.checked_add(usize::from(b - b'0'))) {
                            Some(v) => number = v,
                            None => invalid = true,
                        }
                    } else if b.is_ascii_whitespace() {
                        ended |= started;
                    } else if b != b'+' || started || ended {
                        invalid = true;
                    }
                }
                if typed {
                    tail.rotate_left(1);
                    tail[5] = b.to_ascii_lowercase();
                    double |= &tail == b"double";
                    float |= &tail[1..] == b"float";
                }
            }
        };
        feed(value.as_bytes());
        loop {
            let part = reader.fill_buf().ok()?;
            if part.is_empty() {
                break;
            }
            let newline = part.iter().position(|&b| b == b'\n');
            let n = newline.map_or(part.len(), |at| at + 1);
            feed(&part[..n]);
            reader.consume(n);
            if newline.is_some() {
                break;
            }
        }
        if numeric {
            line = if started && !invalid {
                format!("{key}{} {number}", if first_only { "" } else { ":" })
            } else {
                format!("{key}{} invalid", if first_only { "" } else { ":" })
            };
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
        } else if !first_only && colon_seen && !trimmed.contains(':') {
            // An unknown, very long metadata key is still a header line, not pixel data.
            line = "ignored: header".to_string();
        } else if first_only && key == "nodata_value" && !value.contains(char::is_whitespace) {
            // An unusually long nodata token is left to the native ASCII parser, which repeats
            // the declaration check with its exact type before allocating any pixel buffer.
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
            let mut fields = line.split_ascii_whitespace();
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
            "cols" | "ncols" => {
                if driver != RasterDriver::AAIGrid || cols.is_none() {
                    cols = Some(value.parse().ok()?);
                }
            }
            "rows" | "nrows" => {
                if driver != RasterDriver::AAIGrid || rows.is_none() {
                    rows = Some(value.parse().ok()?);
                }
            }
            "type" => {
                let value = value.to_ascii_lowercase();
                data_type = if value.contains("double") {
                    DataType::F64
                } else if value.contains("float") {
                    DataType::F32
                } else {
                    DataType::I32
                };
            }
            "nodata_value" if !nodata_seen => {
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
