// SPDX-FileCopyrightText: Copyright (c) 2026, T-Kartor
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
// Derived from GDAL 3.13.3 frmts/nitf/rpftocwriter.cpp (MIT).
//! NITF 2.0 / RPF TOC serialization; GDAL 3.13.3 rpftocwriter.cpp layout (MIT).
use super::{RpfFrame, RpfTocOptions, frame, grid::Grid, invalid};
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_FRAMES: usize = 100_000;
const LOCATION: u32 = 683;

fn u16be(out: &mut Vec<u8>, n: u16) {
    out.extend(n.to_be_bytes());
}
fn u32be(out: &mut Vec<u8>, n: u32) {
    out.extend(n.to_be_bytes());
}
fn text(out: &mut Vec<u8>, value: &str, width: usize) {
    out.extend(value.as_bytes());
    out.resize(out.len() + width - value.len(), b' ');
}
fn class_rank(c: char) -> Result<usize> {
    "URCST".find(c).ok_or_else(|| Error::InvalidArgument("RPF classification must be U, R, C, S or T".into()))
}
fn georef(lon: f64, lat: f64) -> Result<String> {
    const LETTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    if !(-360. ..=360.).contains(&lon) || !(-90. ..=90.).contains(&lat) {
        return Err(invalid("invalid GEOREF coordinates"));
    }
    let x = (lon + 180.).rem_euclid(360.);
    // At the north pole use the final representable minute/degree below 90.
    let y = (lat + 90.).min(180. - 1e-5);
    let xi = ((x + 1e-5) as usize).min(359);
    let yi = ((y + 1e-5) as usize).min(179);
    Ok(format!(
        "{}{}{}{}{:02}",
        LETTERS[xi / 15] as char,
        LETTERS[yi / 15] as char,
        LETTERS[xi % 15] as char,
        LETTERS[yi % 15] as char,
        (x * 60. + 1e-5) as usize % 60
    ))
}

struct Indexed<'a> {
    frame: &'a RpfFrame,
    at: [u32; 2],
}

pub(super) fn encode(frames: &[RpfFrame], options: &RpfTocOptions) -> Result<Vec<u8>> {
    let index_classification = class_rank(options.classification)?;
    for (value, limit, name) in [
        (&options.producer_id, 5, "producer ID"),
        (&options.producer_name, 27, "producer name"),
        (&options.country_code, 2, "country code"),
    ] {
        if value.len() > limit || !value.is_ascii() || value.bytes().any(|v| v.is_ascii_control()) {
            return Err(Error::InvalidArgument(format!("RPF {name} must contain at most {limit} printable ASCII bytes")));
        }
    }
    if !options.country_code.is_empty()
        && (options.country_code.len() != 2 || !options.country_code.bytes().all(|v| v.is_ascii_uppercase()))
    {
        return Err(Error::InvalidArgument("RPF country code must be empty or two uppercase letters".into()));
    }
    if frames.is_empty() || frames.len() > MAX_FRAMES {
        return Err(Error::InvalidArgument(format!("RPF TOC requires 1..{MAX_FRAMES} frames")));
    }
    let mut names = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut groups: BTreeMap<(std::cmp::Reverse<u32>, u8), Vec<Indexed<'_>>> = BTreeMap::new();
    let mut highest = 0;
    let mut path_bytes = 0;
    for f in frames {
        let (zone, series) = frame::name_parts(&f.name)?;
        if zone != f.zone || series != f.series || f.name.bytes().any(|v| v.is_ascii_control()) {
            return Err(invalid("frame name does not match its zone/series metadata"));
        }
        if !names.insert(f.name.to_ascii_uppercase()) {
            return Err(invalid("duplicate frame path"));
        }
        highest = highest.max(class_rank(f.classification)?);
        let scale = options
            .scale
            .or_else(|| f.inferred_scale())
            .ok_or_else(|| Error::InvalidArgument(format!("cannot infer scale for {}; supply scale", f.name)))?;
        let at = f.grid_position(scale)?;
        let directory = f.name.rsplit_once('/').map_or("", |(dir, _)| dir);
        if directories.insert(directory) {
            path_bytes += directory.len() + 5;
        }
        if frames.len() * 33 + path_bytes + groups.len() * 132 + 1024 > MAX_BYTES {
            return Err(invalid("TOC metadata exceeds 16 MiB"));
        }
        groups.entry((std::cmp::Reverse(scale), zone)).or_default().push(Indexed { frame: f, at });
    }
    if index_classification < highest {
        return Err(Error::InvalidArgument("RPF classification must be at least the highest frame classification".into()));
    }
    if groups.len() > usize::from(u16::MAX) || directories.len() > usize::from(u16::MAX) {
        return Err(invalid("too many TOC groups or paths"));
    }
    let mut paths = Vec::new();
    let mut offsets = BTreeMap::new();
    for directory in directories {
        let value = if directory.is_empty() { "./".into() } else { format!("./{directory}/") };
        offsets.insert(directory, (frames.len() * 33 + paths.len()) as u32);
        u16be(&mut paths, value.len() as u16);
        paths.extend(value.as_bytes());
    }
    let mut boundaries = Vec::new();
    let mut records = Vec::new();
    for (group, ((std::cmp::Reverse(scale), zone), mut entries)) in groups.into_iter().enumerate() {
        entries.sort_by(|a, b| (a.at[1], a.at[0], &a.frame.name).cmp(&(b.at[1], b.at[0], &b.frame.name)));
        if entries.windows(2).any(|w| w[0].at == w[1].at) {
            return Err(invalid("multiple frames occupy the same scale/zone grid cell"));
        }
        let min = [entries.iter().map(|f| f.at[0]).min().unwrap(), entries.iter().map(|f| f.at[1]).min().unwrap()];
        let max = [entries.iter().map(|f| f.at[0]).max().unwrap(), entries.iter().map(|f| f.at[1]).max().unwrap()];
        if (0..2).any(|i| max[i] - min[i] > u32::from(u16::MAX)) {
            return Err(invalid("TOC frame offsets exceed 16-bit fields"));
        }
        boundaries.extend(b"CADRG55:1 ");
        let label = if scale % 1_000_000 == 0 {
            format!("1:{}M", scale / 1_000_000)
        } else if scale % 1000 == 0 {
            format!("1:{}K", scale / 1000)
        } else {
            format!("1:{scale}")
        };
        text(&mut boundaries, &label, 12);
        boundaries.push(if zone <= 9 {
            b'0' + zone
        } else if zone < 18 {
            b'A' + zone - 10
        } else {
            b'J'
        });
        text(&mut boundaries, &options.producer_id, 5);
        let grid = Grid::new(zone, scale)?;
        for v in grid.coverage(min, max) {
            boundaries.extend(v.to_be_bytes());
        }
        u32be(&mut boundaries, max[1] - min[1] + 1);
        u32be(&mut boundaries, max[0] - min[0] + 1);
        for item in entries {
            let f = item.frame;
            u16be(&mut records, group as u16);
            u16be(&mut records, (item.at[1] - min[1]) as u16);
            u16be(&mut records, (item.at[0] - min[0]) as u16);
            let (directory, basename) = f.name.rsplit_once('/').unwrap_or(("", &f.name));
            u32be(&mut records, offsets[directory]);
            records.extend(basename.as_bytes());
            let [min_lat, min_lon] = grid.geographic_min(f);
            records.extend(georef(min_lon, min_lat)?.as_bytes());
            records.push(f.classification as u8);
            text(&mut records, &options.country_code, 2);
            records.extend(b"  ");
        }
    }
    let mut boundary_header = Vec::new();
    u32be(&mut boundary_header, 0);
    u16be(&mut boundary_header, (boundaries.len() / 132) as u16);
    u16be(&mut boundary_header, 132);
    let mut index_header = vec![b"URCST"[highest]];
    u32be(&mut index_header, 0);
    u32be(&mut index_header, frames.len() as u32);
    u16be(&mut index_header, offsets.len() as u16);
    u16be(&mut index_header, 33);
    records.extend(paths);
    let parts = [boundary_header, boundaries, index_header, records];
    let aggregate = parts.iter().map(Vec::len).sum::<usize>();
    if aggregate + LOCATION as usize + 54 > MAX_BYTES {
        return Err(invalid("TOC metadata exceeds 16 MiB"));
    }
    let mut content = Vec::new();
    u16be(&mut content, 54);
    u32be(&mut content, 14);
    u16be(&mut content, 4);
    u16be(&mut content, 10);
    u32be(&mut content, aggregate as u32);
    let mut offset = LOCATION + 54;
    for (id, part) in (148..=151).zip(&parts) {
        u16be(&mut content, id);
        u32be(&mut content, part.len() as u32);
        u32be(&mut content, offset);
        offset += part.len() as u32;
    }
    for part in parts {
        content.extend(part);
    }
    let mut des = Vec::new();
    des.extend(b"DE");
    text(&mut des, "Registered Extensions", 25);
    des.extend(b"01");
    des.push(options.classification as u8);
    des.extend([b' '; 166]);
    des.extend(b"UDID  0010000");
    if content.len() > 99_999 {
        return Err(Error::Unsupported("RPF TOC exceeds the five-digit TRE payload limit (99999 bytes)".into()));
    }
    let des_size = 11 + content.len();
    let mut header = Vec::new();
    header.extend(b"NITF02.0003    ");
    text(&mut header, &options.producer_id, 10);
    header.extend(b"11111111ZJAN26");
    text(&mut header, "       A.TOC", 80);
    header.push(options.classification as u8);
    header.extend([b' '; 166]);
    header.extend(b"00000000000");
    text(&mut header, &options.producer_name, 27);
    header.extend([b' '; 18]);
    header.extend(format!("{:012}0004630000000000000010209{des_size:09}00000062000RPFHDR00048", 463 + des.len() + des_size).as_bytes());
    header.push(0);
    u16be(&mut header, 48);
    header.extend(b"       A.TOC");
    header.push(0);
    header.extend(b"MIL-C-89038    19941006");
    header.push(options.classification as u8);
    header.extend([b' '; 4]);
    u32be(&mut header, LOCATION);
    header.extend(b"00000");
    debug_assert_eq!(header.len(), 463);
    debug_assert_eq!(des.len(), 209);
    header.extend(des);
    header.extend(format!("RPFDES{:05}", content.len()).as_bytes());
    header.extend(content);
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> RpfFrame {
        let coverage = Grid::new(2, 1_000_000).unwrap().coverage([21, 3], [21, 3]);
        RpfFrame { name: "ZONE2/000C9010.ON2".into(), zone: 2, series: "ON".into(), classification: 'U', coverage }
    }
    #[test]
    fn nitf_lengths_offsets_and_invalid_metadata() {
        let frame = example();
        let bytes = encode(std::slice::from_ref(&frame), &Default::default()).unwrap();
        assert_eq!(&bytes[..9], b"NITF02.00");
        assert_eq!(std::str::from_utf8(&bytes[342..354]).unwrap().parse::<usize>().unwrap(), bytes.len());
        assert_eq!(&bytes[354..360], b"000463");
        assert_eq!(&bytes[672..678], b"RPFDES");
        assert_eq!(u16::from_be_bytes(bytes[683..685].try_into().unwrap()), 54);
        assert!(encode(&[], &Default::default()).is_err());
        assert!(encode(&[frame.clone(), frame.clone()], &Default::default()).is_err());
        let mut wrong = frame.clone();
        wrong.zone = 3;
        assert!(encode(&[wrong], &Default::default()).is_err());
        let mut wrong = frame.clone();
        wrong.series = "TP".into();
        assert!(encode(&[wrong], &Default::default()).is_err());
        let options = RpfTocOptions { scale: Some(500_000), ..Default::default() };
        assert!(encode(std::slice::from_ref(&frame), &options).is_err());
        let options = RpfTocOptions { producer_id: "TOO LONG".into(), ..Default::default() };
        assert!(encode(std::slice::from_ref(&frame), &options).is_err());
        let options = RpfTocOptions { classification: 'X', ..Default::default() };
        assert!(encode(&[frame], &options).is_err());
    }
    #[test]
    fn index_classification_cannot_downgrade_frame_classification() {
        let mut frame = example();
        frame.classification = 'S';
        for classification in ['U', 'R', 'C'] {
            let options = RpfTocOptions { classification, ..Default::default() };
            assert!(
                matches!(encode(std::slice::from_ref(&frame),&options),Err(Error::InvalidArgument(message)) if message.contains("highest frame"))
            );
        }
        for classification in ['S', 'T'] {
            let options = RpfTocOptions { classification, ..Default::default() };
            let bytes = encode(std::slice::from_ref(&frame), &options).unwrap();
            assert_eq!(bytes[119], classification as u8);
        }
    }
    #[test]
    fn wrapped_and_unwrapped_antimeridian_corners_encode_identically() {
        let grid = Grid::new(2, 1_000_000).unwrap();
        for column in [0, 131] {
            let mut frame = example();
            frame.coverage = grid.coverage([column, 3], [column, 3]);
            let expected = encode(std::slice::from_ref(&frame), &Default::default()).unwrap();
            if column == 0 {
                frame.coverage[1] -= 4e-14;
                frame.coverage[3] -= 4e-14;
            }
            for i in [1, 3, 5, 7] {
                frame.coverage[i] = (frame.coverage[i] + 180.).rem_euclid(360.) - 180.;
            }
            assert_eq!(frame.grid_position(1_000_000).unwrap(), [column, 3]);
            assert_eq!(encode(&[frame], &Default::default()).unwrap(), expected);
        }
    }

    #[test]
    fn pole_center_georef_uses_the_full_geographic_extent() {
        for (zone, suffix, expected) in [(9, '9', b"AMAP00"), (18, 'J', b"AAAA00")] {
            let grid = Grid::new(zone, 1_000_000).unwrap();
            let frame = RpfFrame {
                name: format!("00000000.ON{suffix}"),
                zone,
                series: "ON".into(),
                classification: 'U',
                coverage: grid.coverage([5, 5], [5, 5]),
            };
            let bytes = encode(&[frame], &Default::default()).unwrap();
            let section = LOCATION as usize;
            let location_record = section + 14 + 3 * 10;
            let index = u32::from_be_bytes(bytes[location_record + 6..location_record + 10].try_into().unwrap()) as usize;
            assert_eq!(&bytes[index + 22..index + 28], expected);
        }
    }

    #[test]
    fn oversized_tre_is_rejected_without_expanding_its_five_digit_field() {
        let grid = Grid::new(1, 500_000).unwrap();
        let frames = (0..4000)
            .map(|i| {
                let at = [i % 300, i / 300];
                RpfFrame {
                    name: format!("{i:08X}.TP1"),
                    zone: 1,
                    series: "TP".into(),
                    classification: 'U',
                    coverage: grid.coverage(at, at),
                }
            })
            .collect::<Vec<_>>();
        assert!(matches!(encode(&frames, &Default::default()), Err(Error::Unsupported(message)) if message.contains("five-digit TRE")));
    }

    #[test]
    #[ignore = "requires prepared RPF fixtures; see docs/testing.md"]
    fn real_frames_encode_when_available() {
        let root = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA to prepared RPF fixtures"));
        let paths = super::super::prepared_flat_frames(&root);
        let input = root.join("rpftoc_landsat");
        let mut frames = Vec::new();
        for path in paths {
            let name = path.strip_prefix(&input).unwrap().to_str().unwrap().replace(std::path::MAIN_SEPARATOR, "/");
            frames.push(super::super::read_frame(&name, &mut std::fs::File::open(path).unwrap()).unwrap());
        }
        let bytes = encode(&frames, &Default::default()).unwrap();
        assert!(bytes.len() < 2048);
        if let Some(output) = std::env::var_os("TERRIO_RPFTOC_TEST_OUTPUT") {
            std::fs::write(output, bytes).unwrap();
        }
    }
}
