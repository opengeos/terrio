// SPDX-FileCopyrightText: 2026 Qiusheng Wu
// SPDX-FileCopyrightText: 2026 T-Kartor
// SPDX-License-Identifier: MIT
//! Bounded NITF/RPF metadata inspection; GDAL 3.13.3 is the conformance reference.
use super::{RpfFrame, invalid};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

const MAX_HEADER: usize = 1024 * 1024;
const MAX_LOCATION: usize = 64 * 1024;

fn read_at(reader: &mut (impl Read + Seek), file_size: u64, offset: u64, size: usize) -> Result<Vec<u8>> {
    if offset.checked_add(size as u64).is_none_or(|end| end > file_size) {
        return Err(invalid("metadata record lies outside the file"));
    }
    reader.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn decimal(bytes: &[u8]) -> Result<usize> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid("invalid NITF decimal field"));
    }
    bytes.iter().try_fold(0usize, |n, c| {
        n.checked_mul(10).and_then(|n| n.checked_add(usize::from(c - b'0'))).ok_or_else(|| invalid("NITF decimal overflow"))
    })
}
fn take<'a>(bytes: &'a [u8], cursor: &mut usize, count: usize) -> Result<&'a [u8]> {
    let end = cursor.checked_add(count).ok_or_else(|| invalid("NITF header overflow"))?;
    let part = bytes.get(*cursor..end).ok_or_else(|| invalid("truncated NITF header"))?;
    *cursor = end;
    Ok(part)
}
fn u16_at(bytes: &[u8], offset: usize, little: bool) -> Result<u16> {
    let b: [u8; 2] =
        bytes.get(offset..offset + 2).ok_or_else(|| invalid("truncated RPF integer"))?.try_into().map_err(|_| invalid("RPF integer"))?;
    Ok(if little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
}
fn u32_at(bytes: &[u8], offset: usize, little: bool) -> Result<u32> {
    let b: [u8; 4] =
        bytes.get(offset..offset + 4).ok_or_else(|| invalid("truncated RPF integer"))?.try_into().map_err(|_| invalid("RPF integer"))?;
    Ok(if little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
}
fn rpf_header(header: &[u8], segment_start: usize) -> Result<&[u8]> {
    let mut cursor = segment_start;
    // Image, graphic, label/reserved, text, data-extension and reserved-extension
    // segment tables. The header contains only descriptors, not segment data.
    for record_size in [16, 10, 7, 9, 13, 11] {
        let count = decimal(take(header, &mut cursor, 3)?)?;
        let size = count.checked_mul(record_size).ok_or_else(|| invalid("segment table overflow"))?;
        take(header, &mut cursor, size)?;
    }
    let mut found = None;
    for _ in 0..2 {
        let size = decimal(take(header, &mut cursor, 5)?)?;
        if size == 0 {
            continue;
        }
        let tre = take(header, &mut cursor, size)?;
        if tre.get(..3) != Some(b"000") {
            return Err(Error::Unsupported("RPF header TRE overflow chains are not supported".into()));
        }
        let mut pos = 3;
        while pos < tre.len() {
            let name = take(tre, &mut pos, 6)?;
            let size = decimal(take(tre, &mut pos, 5)?)?;
            let payload = take(tre, &mut pos, size)?;
            if name == b"RPFHDR" {
                if found.is_some() || payload.len() != 48 {
                    return Err(invalid("duplicate or nonstandard RPFHDR"));
                }
                found = Some(payload);
            }
        }
    }
    found.ok_or_else(|| invalid("no RPFHDR in the NITF file header"))
}

pub(super) fn name_parts(relative_name: &str) -> Result<(u8, String)> {
    if relative_name.is_empty()
        || relative_name.len() > 4096
        || relative_name.starts_with('/')
        || relative_name.contains(['\\', '\0', ':'])
        || relative_name.split('/').any(|part| part.is_empty() || part.ends_with(['.', ' ']))
    {
        return Err(Error::InvalidArgument("RPF frame names must be safe relative paths".into()));
    }
    let basename = relative_name.rsplit('/').next().ok_or_else(|| invalid("missing frame name"))?;
    if !basename.is_ascii()
        || basename.len() != 12
        || basename.as_bytes()[8] != b'.'
        || !basename[..8].bytes().all(|c| c.is_ascii_alphanumeric())
        || !basename[9..11].bytes().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(invalid("CADRG frame basenames must have eight characters and a three-character extension"));
    }
    let zone = match basename.as_bytes()[11].to_ascii_uppercase() {
        c @ b'1'..=b'9' => c - b'0',
        c @ b'A'..=b'H' => c - b'A' + 10,
        b'J' => 18,
        _ => return Err(invalid("unknown CADRG zone")),
    };
    Ok((zone, basename[9..11].to_ascii_uppercase()))
}

pub(super) fn read(relative_name: &str, reader: &mut (impl Read + Seek)) -> Result<RpfFrame> {
    let (zone, series) = name_parts(relative_name)?;
    let size = reader.seek(SeekFrom::End(0))?;
    let prefix = read_at(reader, size, 0, 360)?;
    let version = &prefix[..9];
    if version != b"NITF02.00" && version != b"NITF02.10" {
        return Err(Error::Unsupported("RPF frame inspection requires NITF 2.0 or 2.1".into()));
    }
    let extra = if version == b"NITF02.00" && &prefix[280..286] == b"999998" { 40 } else { 0 };
    let header_length = if extra == 0 { decimal(&prefix[354..360])? } else { decimal(&read_at(reader, size, 394, 6)?)? };
    if !(363 + extra..=MAX_HEADER).contains(&header_length) {
        return Err(invalid("NITF header length exceeds its metadata budget"));
    }
    let mut header = prefix;
    header.extend(read_at(reader, size, 360, header_length - 360)?);
    if decimal(&header[360 + extra..363 + extra])? != 1 {
        return Err(invalid("RPF frame requires exactly one NITF image segment"));
    }
    let rpf = rpf_header(&header, 360 + extra)?;
    let standard = std::str::from_utf8(&rpf[16..31]).map_err(|_| invalid("invalid RPF standard identifier"))?.trim();
    if !matches!(standard, "MIL-C-89038" | "MIL-PRF-89038") {
        return Err(Error::Unsupported("RPF TOC creation requires CADRG frames (MIL-C-89038/MIL-PRF-89038)".into()));
    }
    let little = match rpf[0] {
        0 => false,
        1 => true,
        _ => return Err(invalid("invalid RPF byte-order flag")),
    };
    if u16_at(rpf, 1, little)? != 48 {
        return Err(invalid("RPFHDR length does not match its payload"));
    }
    let location = u64::from(u32_at(rpf, 44, little)?);
    let section_header = read_at(reader, size, location, 14)?;
    let section_length = usize::from(u16_at(&section_header, 0, little)?);
    let table_offset = u32_at(&section_header, 2, little)? as usize;
    let count = usize::from(u16_at(&section_header, 6, little)?);
    let record_length = usize::from(u16_at(&section_header, 8, little)?);
    if section_length > MAX_LOCATION
        || table_offset < 14
        || record_length < 10
        || count.checked_mul(record_length).and_then(|n| n.checked_add(table_offset)).is_none_or(|n| n > section_length)
    {
        return Err(invalid("invalid or oversized RPF location table"));
    }
    let table = read_at(reader, size, location + table_offset as u64, count * record_length)?;
    let mut coverage = None;
    let mut description = None;
    for record in table.chunks_exact(record_length) {
        let id = u16_at(record, 0, little)?;
        let length = u32_at(record, 2, little)? as usize;
        let offset = u64::from(u32_at(record, 6, little)?);
        if offset.checked_add(length as u64).is_none_or(|end| end > size) {
            return Err(invalid("RPF component lies outside the file"));
        }
        let target = match id {
            130 => &mut coverage,
            136 => &mut description,
            _ => continue,
        };
        if target.replace((offset, length)).is_some() {
            return Err(invalid("duplicate required RPF component"));
        }
    }
    let (offset, length) = coverage.ok_or_else(|| invalid("missing RPF coverage"))?;
    if length != 96 {
        return Err(invalid("nonstandard RPF coverage length"));
    }
    let bytes = read_at(reader, size, offset, length)?;
    let mut values = [0.; 12];
    for (value, bytes) in values.iter_mut().zip(bytes.as_chunks::<8>().0) {
        let b = *bytes;
        *value = if little { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) };
    }
    if values.iter().any(|v| !v.is_finite())
        || values[8..].iter().any(|v| *v <= 0.)
        || values[..8].as_chunks::<2>().0.iter().any(|c| c[0].abs() > 90. || c[1].abs() > 360.)
    {
        return Err(invalid("invalid RPF coverage coordinates or resolution"));
    }
    let (offset, length) = description.ok_or_else(|| invalid("missing RPF image description"))?;
    if length < 20 {
        return Err(invalid("truncated RPF image description"));
    }
    let description = read_at(reader, size, offset, 20)?;
    if u32_at(&description, 12, little)?.checked_mul(u32::from(u16_at(&description, 8, little)?)) != Some(1536)
        || u32_at(&description, 16, little)?.checked_mul(u32::from(u16_at(&description, 10, little)?)) != Some(1536)
    {
        return Err(invalid("RPF input must be a 1536 by 1536 frame"));
    }
    let classes = b"URCST";
    let rpf_class = classes.iter().position(|value| *value == rpf[39]).ok_or_else(|| invalid("unknown RPF frame classification"))?;
    let nitf_class = classes.iter().position(|value| *value == header[119]).ok_or_else(|| invalid("unknown NITF file classification"))?;
    let classification = char::from(classes[rpf_class.max(nitf_class)]);
    Ok(RpfFrame { name: relative_name.into(), zone, series, classification, coverage: values })
}

#[cfg(test)]
pub(super) fn test_frame(coverage: [f64; 12]) -> Vec<u8> {
    let mut header = vec![b' '; 360];
    header[..9].copy_from_slice(b"NITF02.00");
    header[119] = b'U';
    header.extend_from_slice(b"0010000000000000000000000000000000");
    header.extend_from_slice(b"00062000RPFHDR00048");
    let mut rpf = vec![0];
    rpf.extend_from_slice(&48u16.to_be_bytes());
    rpf.extend_from_slice(b"00000000.ON2");
    rpf.push(0);
    rpf.extend_from_slice(b"MIL-C-89038    19941006U    ");
    let location = header.len() + 48 + 5;
    rpf.extend_from_slice(&(location as u32).to_be_bytes());
    assert_eq!(rpf.len(), 48);
    header.extend(rpf);
    header.extend_from_slice(b"00000");
    assert_eq!(header.len(), location);
    header[354..360].copy_from_slice(format!("{location:06}").as_bytes());
    let mut bytes = header;
    bytes.extend_from_slice(&34u16.to_be_bytes());
    bytes.extend_from_slice(&14u32.to_be_bytes());
    bytes.extend_from_slice(&2u16.to_be_bytes());
    bytes.extend_from_slice(&10u16.to_be_bytes());
    bytes.extend_from_slice(&116u32.to_be_bytes());
    for (id, size, offset) in [(130u16, 96u32, location + 34), (136, 20, location + 34 + 96)] {
        bytes.extend_from_slice(&id.to_be_bytes());
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(&(offset as u32).to_be_bytes());
    }
    for value in coverage {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&[0, 1, 0, 36, 0, 1, 0, 1, 0, 6, 0, 6]);
    bytes.extend_from_slice(&256u32.to_be_bytes());
    bytes.extend_from_slice(&256u32.to_be_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fixture() -> Vec<u8> {
        super::test_frame([15.36, 0., 0., 0., 15.36, 15.36, 0., 15.36, 100., 100., 0.01, 0.01])
    }

    #[test]
    fn bounded_frame_metadata_retains_zone_series_and_coverage() {
        let frame = read("sub/00000000.ON2", &mut Cursor::new(fixture())).unwrap();
        assert_eq!((frame.zone, frame.classification, frame.series.as_str()), (2, 'U', "ON"));
        assert_eq!(frame.coverage[10..], [0.01, 0.01]);
        assert_eq!(frame.inferred_scale(), Some(1_000_000));
    }

    #[test]
    fn frame_classification_keeps_the_stricter_nitf_or_rpf_label() {
        for (nitf, rpf, expected) in [(b'S', b'U', 'S'), (b'U', b'S', 'S'), (b'C', b'R', 'C')] {
            let mut bytes = fixture();
            bytes[119] = nitf;
            let start = bytes.windows(6).position(|v| v == b"RPFHDR").unwrap() + 11;
            bytes[start + 39] = rpf;
            assert_eq!(read("00000000.ON2", &mut Cursor::new(bytes)).unwrap().classification, expected);
        }
    }

    #[test]
    fn non_cadrg_standard_is_rejected() {
        let mut bytes = fixture();
        let at = bytes.windows(15).position(|v| v == b"MIL-C-89038    ").unwrap();
        bytes[at..at + 15].copy_from_slice(b"MIL-C-89041    ");
        assert!(matches!(read("00000000.ON2", &mut Cursor::new(bytes)), Err(Error::Unsupported(_))));
    }

    #[test]
    fn truncated_metadata_and_unsafe_paths_fail_without_panics() {
        let bytes = fixture();
        for length in [0, 359, 360, bytes.len() - 1] {
            assert!(read("00000000.ON2", &mut Cursor::new(&bytes[..length])).is_err());
        }
        for name in ["../00000000.ON2", "/00000000.ON2", "x//00000000.ON2", "00000000.ON0"] {
            assert!(read(name, &mut Cursor::new(&bytes)).is_err());
        }
    }

    #[test]
    fn windows_normalized_components_fail_before_metadata_reads() {
        for name in [".. /00000000.ON2", "zone./00000000.ON2", "zone /00000000.ON2", "./00000000.ON2", "../00000000.ON2"] {
            assert!(matches!(read(name, &mut Cursor::new(Vec::<u8>::new())), Err(Error::InvalidArgument(_))), "{name}");
        }
    }

    #[test]
    fn declared_component_outside_file_is_rejected() {
        let mut bytes = fixture();
        let location = decimal(&bytes[354..360]).unwrap();
        bytes[location + 20..location + 24].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(read("00000000.ON2", &mut Cursor::new(bytes)).is_err());
    }
    #[test]
    #[ignore = "requires prepared RPF fixtures; see docs/testing.md"]
    fn real_frame_metadata_reads_stay_below_one_kib_when_fixtures_are_available() {
        let root = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA to prepared RPF fixtures"));
        struct Counted {
            file: std::fs::File,
            bytes: usize,
        }
        impl Read for Counted {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let n = self.file.read(buffer)?;
                self.bytes += n;
                Ok(n)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
                self.file.seek(from)
            }
        }
        let mut paths = super::super::prepared_flat_frames(&root);
        paths.push(root.join("rpftoc_polar/RPF/ZONE9/00027010.ON9"));
        for path in paths {
            assert!(path.is_file(), "missing RPF fixture {}", path.display());
            let name = path.strip_prefix(&root).unwrap().to_str().unwrap().replace(std::path::MAIN_SEPARATOR, "/");
            let mut input = Counted { file: std::fs::File::open(path).unwrap(), bytes: 0 };
            let frame = read(&name, &mut input).unwrap();
            assert_eq!(frame.classification, 'U');
            assert!(frame.inferred_scale().is_some());
            assert!(input.bytes < 1024, "{} metadata bytes for {name}", input.bytes);
        }
    }
    #[test]
    fn declared_header_without_segment_count_is_rejected_without_panicking() {
        for length in [360, 361, 362] {
            let mut bytes = fixture();
            bytes[354..360].copy_from_slice(format!("{length:06}").as_bytes());
            assert!(read("00000000.ON2", &mut Cursor::new(bytes)).is_err());
        }
    }
}
