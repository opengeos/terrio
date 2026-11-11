// SPDX-FileCopyrightText: Copyright (c) 2024 Whitebox Geospatial Inc.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Header layouts adapted from wbraster 0.2.1 src/formats/{dted,pcraster,esri_binary,grass_ascii}.rs.
// Only declarations are read here; the original reader still validates and decodes the file.

//! Declaration checks for readers without a separate metadata API. Reference: GDAL 3.13.3.

use crate::{DataType, Result, formats::RasterDriver};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// Checks the header before the fallback codec reads its file or allocates pixel buffers.
/// Unrecognized/truncated headers keep the original reader's diagnostics.
pub(crate) fn check(path: &Path, driver: RasterDriver) -> Result<()> {
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
        ascii_shape(BufReader::new(file).lines().map_while(std::result::Result::ok), driver)
    } else {
        let mut bytes = Vec::new();
        if file.take(256).read_to_end(&mut bytes).is_err() {
            return Ok(());
        }
        shape(&bytes, driver)
    };
    if let Some((cols, rows, data_type)) = dimensions {
        crate::declared::check(cols, rows, 1, data_type, false)?;
    }
    Ok(())
}

/// The in-memory ASCII codec reads Float64 samples regardless of the values in its document.
/// Keep that sample type while checking its header before the codec parses any pixel tokens.
pub(crate) fn check_ascii_bytes(bytes: &[u8]) -> Result<()> {
    let Ok(text) = std::str::from_utf8(bytes) else { return Ok(()) };
    let (mut cols, mut rows) = (None, None);
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+') {
            break;
        }
        // Use the codec's own key/comment rules, including '=' and unknown header fields.
        if let Some((key, value)) = wbraster::io_utils::parse_key_value(line) {
            match key.as_str() {
                "ncols" => cols = value.parse().ok(),
                "nrows" => rows = value.parse().ok(),
                _ => {}
            }
        }
    }
    if let Some((cols, rows)) = cols.zip(rows) {
        crate::declared::check(cols, rows, 1, DataType::F64, false)?;
    }
    Ok(())
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
                if let Some(v) = super::common::parse_f64(value)
                    && (value.contains(['.', ',']) || v < i32::MIN as f64 || v > i32::MAX as f64)
                    && v.is_finite()
                    && v != 0.0
                    && (v.abs() < f32::MIN_POSITIVE as f64 || v.abs() > f32::MAX as f64)
                {
                    data_type = DataType::F64;
                }
            }
            _ => {}
        }
    }
    Some((cols?, rows?, data_type))
}
