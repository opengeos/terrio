//! The LAS container: the public header block, variable-length records, and the point record
//! layouts of point data formats 0 to 10 (ASPRS LAS 1.0 to 1.4 R15), with extra bytes.

use super::values::{Dimension, ValueType, Values};
use crate::{Error, Result};
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom, Write};

/// The size of a VLR header.
pub(crate) const VLR_HEADER_LEN: usize = 54;
/// The size of an EVLR header.
pub(crate) const EVLR_HEADER_LEN: usize = 60;
/// The user id of the LAS specification's own records (extra bytes, classification lookup...).
pub(crate) const LASF_SPEC: &str = "LASF_Spec";
/// The user id of the CRS records.
pub(crate) const LASF_PROJECTION: &str = "LASF_Projection";
/// The user id and record id of the LASzip record.
pub(crate) const LASZIP_USER_ID: &str = "laszip encoded";
pub(crate) const LASZIP_RECORD_ID: u16 = 22204;
/// The record id of the waveform data packet record (an EVLR of `LASF_Spec`).
pub(crate) const WAVEFORM_RECORD_ID: u16 = 65535;
/// The global encoding bit of waveform data packets inside the file.
pub(crate) const WAVEFORM_INTERNAL_BIT: u16 = 2;
/// The global encoding bit of waveform data packets in an external `.wdp` file.
pub(crate) const WAVEFORM_EXTERNAL_BIT: u16 = 4;

/// The standard dimension names, in the order of the LAS 1.4 formats.
pub(crate) const STANDARD_DIMENSIONS: &[&str] = &[
    "x",
    "y",
    "z",
    "intensity",
    "return_number",
    "number_of_returns",
    "scan_direction_flag",
    "edge_of_flight_line",
    "classification",
    "synthetic",
    "key_point",
    "withheld",
    "overlap",
    "scanner_channel",
    "scan_angle_rank",
    "scan_angle",
    "user_data",
    "point_source_id",
    "gps_time",
    "red",
    "green",
    "blue",
    "nir",
    "wavepacket_index",
    "wavepacket_offset",
    "wavepacket_size",
    "return_point_wave_location",
    "x_t",
    "y_t",
    "z_t",
];

/// The type of a standard LAS dimension, by its name.
pub fn standard_value_type(name: &str) -> Option<ValueType> {
    if name == "scan_angle_rank" {
        return Some(ValueType::I8);
    }
    standard_fields(10).into_iter().find(|f| f.name == name).map(|f| f.value_type)
}

/// A variable-length record (VLR) or extended variable-length record (EVLR).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Vlr {
    /// The user id, such as `LASF_Projection`.
    pub user_id: String,
    /// The record id.
    pub record_id: u16,
    /// The description.
    pub description: String,
    /// The payload.
    #[serde(skip)]
    pub data: Vec<u8>,
}

impl Vlr {
    /// A record with a payload.
    pub fn new(user_id: &str, record_id: u16, description: &str, data: Vec<u8>) -> Self {
        Self { user_id: user_id.to_string(), record_id, description: description.to_string(), data }
    }

    /// Whether this record has the given user id and record id.
    pub fn is(&self, user_id: &str, record_id: u16) -> bool {
        self.user_id == user_id && self.record_id == record_id
    }

    /// Whether the record is the waveform data packet record (`LASF_Spec` 65535; some writers
    /// name the user `LAS_Spec`).
    pub fn is_waveform_data(&self) -> bool {
        self.record_id == WAVEFORM_RECORD_ID && (self.user_id == LASF_SPEC || self.user_id == "LAS_Spec")
    }

    /// Whether the record is the LASzip compression record.
    pub fn is_laszip(&self) -> bool {
        self.is(LASZIP_USER_ID, LASZIP_RECORD_ID)
    }

    pub(crate) fn write<W: Write>(&self, w: &mut W, extended: bool) -> Result<()> {
        w.write_all(&[0, 0])?;
        w.write_all(&fixed_str(&self.user_id, 16))?;
        w.write_all(&self.record_id.to_le_bytes())?;
        if extended {
            w.write_all(&(self.data.len() as u64).to_le_bytes())?;
        } else {
            let len = u16::try_from(self.data.len())
                .map_err(|_| Error::InvalidArgument(format!("VLR {}/{} is too large for a VLR", self.user_id, self.record_id)))?;
            w.write_all(&len.to_le_bytes())?;
        }
        w.write_all(&fixed_str(&self.description, 32))?;
        w.write_all(&self.data)?;
        Ok(())
    }

    /// Writes the record's 60-byte EVLR header, for a payload of `len` bytes written apart.
    pub(crate) fn write_evlr_header<W: Write>(&self, w: &mut W, len: u64) -> Result<()> {
        w.write_all(&[0, 0])?;
        w.write_all(&fixed_str(&self.user_id, 16))?;
        w.write_all(&self.record_id.to_le_bytes())?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(&fixed_str(&self.description, 32))?;
        Ok(())
    }

    /// The record's size in the file, header included.
    pub(crate) fn file_len(&self, extended: bool) -> usize {
        self.data.len() + if extended { EVLR_HEADER_LEN } else { VLR_HEADER_LEN }
    }
}

/// The public header block of a LAS file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LasHeader {
    /// The version: `(major, minor)`, such as `(1, 4)`.
    pub version: (u8, u8),
    /// The point data record format, 0 to 10.
    pub point_format: u8,
    /// The size of a point record in bytes, extra bytes included.
    pub point_record_length: u16,
    /// Whether the points are LASzip-compressed (a LAZ file).
    pub compressed: bool,
    /// The file source id.
    pub file_source_id: u16,
    /// The global encoding bits (GPS time type, WKT CRS...).
    pub global_encoding: u16,
    /// The project id (GUID), as a string.
    pub guid: String,
    /// The system identifier.
    pub system_identifier: String,
    /// The generating software.
    pub generating_software: String,
    /// The creation day of year.
    pub creation_day: u16,
    /// The creation year.
    pub creation_year: u16,
    /// The size of the header block.
    pub header_size: u16,
    /// The offset of the point records.
    pub offset_to_point_data: u32,
    /// The number of point records.
    pub point_count: u64,
    /// The number of points by return number (5 values, or 15 from LAS 1.4).
    pub points_by_return: Vec<u64>,
    /// The scale of the X, Y and Z record values.
    pub scale: [f64; 3],
    /// The offset of the X, Y and Z record values.
    pub offset: [f64; 3],
    /// The minimum X, Y and Z.
    pub min: [f64; 3],
    /// The maximum X, Y and Z.
    pub max: [f64; 3],
    /// The offset of the waveform data packet record (LAS 1.3 and later): the header of the EVLR
    /// that holds the waveforms of the points when they are in the file (global encoding bit 1).
    pub start_of_waveform_data: u64,
    /// The size of the waveform data packets in the file (the payload of their record), 0 when
    /// there are none. The record is listed among the EVLRs, without its payload, which
    /// [`super::PointCloudReader`] reads when it reads points that refer to it.
    pub waveform_data_size: u64,
    /// The offset of the first EVLR (LAS 1.4).
    pub start_of_first_evlr: u64,
    /// The variable-length records.
    pub vlrs: Vec<Vlr>,
    /// The extended variable-length records (LAS 1.4).
    pub evlrs: Vec<Vlr>,
}

impl Default for LasHeader {
    fn default() -> Self {
        Self {
            version: (1, 4),
            point_format: 6,
            point_record_length: 30,
            compressed: false,
            file_source_id: 0,
            global_encoding: 0,
            guid: format_guid(&[0; 16]),
            system_identifier: String::new(),
            generating_software: String::new(),
            creation_day: 0,
            creation_year: 0,
            header_size: 375,
            offset_to_point_data: 375,
            point_count: 0,
            points_by_return: vec![0; 15],
            scale: [0.01, 0.01, 0.01],
            offset: [0.0; 3],
            min: [0.0; 3],
            max: [0.0; 3],
            start_of_waveform_data: 0,
            waveform_data_size: 0,
            start_of_first_evlr: 0,
            vlrs: Vec::new(),
            evlrs: Vec::new(),
        }
    }
}

impl LasHeader {
    /// Reads the header block, the VLRs and the EVLRs. The source is left at the start of the
    /// point records.
    pub fn read<R: Read + Seek>(r: &mut R) -> Result<Self> {
        r.seek(SeekFrom::Start(0))?;
        let mut b = [0u8; 375];
        let n = read_up_to(r, &mut b[..227])?;
        if n < 227 || &b[0..4] != b"LASF" {
            return Err(Error::InvalidData("not a LAS file: no LASF signature".into()));
        }
        let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or_default());
        let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        let f64_at = |b: &[u8], o: usize| f64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        let header_size = u16_at(&b, 94);
        let version = (b[24], b[25]);
        if header_size < 227 {
            return Err(Error::InvalidData(format!("LAS header size {header_size} is smaller than 227 bytes")));
        }
        let extra = (header_size as usize).min(375) - 227;
        if extra > 0 && read_up_to(r, &mut b[227..227 + extra])? < extra {
            return Err(Error::InvalidData("truncated LAS header".into()));
        }
        let format_byte = b[104];
        let point_format = format_byte & 0x3F;
        if point_format > 10 {
            return Err(Error::Unsupported(format!("LAS point data format {point_format}")));
        }
        let mut h = LasHeader {
            version,
            point_format,
            point_record_length: u16_at(&b, 105),
            compressed: format_byte & 0x80 != 0,
            file_source_id: u16_at(&b, 4),
            global_encoding: u16_at(&b, 6),
            guid: format_guid(&b[8..24].try_into().unwrap_or([0; 16])),
            system_identifier: c_str(&b[26..58]),
            generating_software: c_str(&b[58..90]),
            creation_day: u16_at(&b, 90),
            creation_year: u16_at(&b, 92),
            header_size,
            offset_to_point_data: u32_at(&b, 96),
            point_count: u32_at(&b, 107) as u64,
            points_by_return: (0..5).map(|i| u32_at(&b, 111 + 4 * i) as u64).collect(),
            scale: [f64_at(&b, 131), f64_at(&b, 139), f64_at(&b, 147)],
            offset: [f64_at(&b, 155), f64_at(&b, 163), f64_at(&b, 171)],
            max: [f64_at(&b, 179), f64_at(&b, 195), f64_at(&b, 211)],
            min: [f64_at(&b, 187), f64_at(&b, 203), f64_at(&b, 219)],
            start_of_waveform_data: 0,
            waveform_data_size: 0,
            start_of_first_evlr: 0,
            vlrs: Vec::new(),
            evlrs: Vec::new(),
        };
        let mut evlr_count = 0;
        if header_size >= 235 && version >= (1, 3) {
            h.start_of_waveform_data = u64_at(&b, 227);
        }
        if header_size >= 375 && version >= (1, 4) {
            h.start_of_first_evlr = u64_at(&b, 235);
            evlr_count = u32_at(&b, 243);
            let count = u64_at(&b, 247);
            // Writers that fill only the legacy fields leave the 64-bit ones at zero.
            if count != 0 || h.point_count == 0 {
                h.point_count = count;
                h.points_by_return = (0..15).map(|i| u64_at(&b, 255 + 8 * i)).collect();
            }
        }
        // The VLRs.
        let vlr_count = u32_at(&b, 100);
        r.seek(SeekFrom::Start(header_size as u64))?;
        let mut pos = header_size as u64;
        for _ in 0..vlr_count {
            if pos + VLR_HEADER_LEN as u64 > h.offset_to_point_data as u64 {
                break; // a wrong VLR count: keep the records that fit
            }
            let mut vh = [0u8; VLR_HEADER_LEN];
            r.read_exact(&mut vh)?;
            let len = u16_at(&vh, 20) as usize;
            let len = len.min((h.offset_to_point_data as u64).saturating_sub(pos + VLR_HEADER_LEN as u64) as usize);
            let mut data = vec![0u8; len];
            r.read_exact(&mut data)?;
            h.vlrs.push(Vlr { user_id: c_str(&vh[2..18]), record_id: u16_at(&vh, 18), description: c_str(&vh[22..54]), data });
            pos += (VLR_HEADER_LEN + len) as u64;
        }
        // The EVLRs.
        if evlr_count > 0 && h.start_of_first_evlr > 0 {
            let mut at = h.start_of_first_evlr;
            for _ in 0..evlr_count {
                r.seek(SeekFrom::Start(at))?;
                let mut vh = [0u8; EVLR_HEADER_LEN];
                if read_up_to(r, &mut vh)? < EVLR_HEADER_LEN {
                    break;
                }
                let len = u64_at(&vh, 20);
                if len > (1 << 32) {
                    return Err(Error::InvalidData(format!("EVLR of {len} bytes")));
                }
                // The COPC hierarchy is read page by page when needed, and the waveform packets
                // when points that refer to them are read, not here.
                let user_id = c_str(&vh[2..18]);
                let record_id = u16_at(&vh, 18);
                let waveforms = record_id == WAVEFORM_RECORD_ID && (user_id == LASF_SPEC || user_id == "LAS_Spec");
                let skip = waveforms || (user_id == super::copc::COPC_USER_ID && record_id == 1000);
                if waveforms {
                    h.start_of_waveform_data = at;
                    h.waveform_data_size = len;
                }
                let mut data = vec![0u8; if skip { 0 } else { len as usize }];
                r.read_exact(&mut data)?;
                h.evlrs.push(Vlr { user_id, record_id, description: c_str(&vh[28..60]), data });
                at += EVLR_HEADER_LEN as u64 + len;
            }
        } else if version == (1, 3) && h.start_of_waveform_data > 0 {
            // LAS 1.3: the waveform data packet record, the only record after the points, in the
            // EVLR format.
            r.seek(SeekFrom::Start(h.start_of_waveform_data))?;
            let mut vh = [0u8; EVLR_HEADER_LEN];
            if read_up_to(r, &mut vh)? == EVLR_HEADER_LEN {
                h.waveform_data_size = u64_at(&vh, 20);
                h.evlrs.push(Vlr {
                    user_id: c_str(&vh[2..18]),
                    record_id: u16_at(&vh, 18),
                    description: c_str(&vh[28..60]),
                    data: Vec::new(),
                });
            }
        }
        if (h.point_record_length as usize) < standard_record_length(point_format) {
            return Err(Error::InvalidData(format!(
                "point record length {} is shorter than format {point_format}'s {} bytes",
                h.point_record_length,
                standard_record_length(point_format)
            )));
        }
        r.seek(SeekFrom::Start(h.offset_to_point_data as u64))?;
        Ok(h)
    }

    /// All records, VLRs then EVLRs.
    pub fn all_vlrs(&self) -> impl Iterator<Item = &Vlr> {
        self.vlrs.iter().chain(self.evlrs.iter())
    }

    /// The first record with this user id and record id, VLR or EVLR.
    pub fn find_vlr(&self, user_id: &str, record_id: u16) -> Option<&Vlr> {
        self.all_vlrs().find(|v| v.is(user_id, record_id))
    }

    /// The version as text, such as `1.4`.
    pub fn version_string(&self) -> String {
        format!("{}.{}", self.version.0, self.version.1)
    }

    /// Whether the GPS times are adjusted standard GPS time (global encoding bit 0) rather than
    /// GPS week time.
    pub fn adjusted_gps_time(&self) -> bool {
        self.global_encoding & 1 != 0
    }

    /// The extra-bytes descriptors of the extra bytes VLR (or EVLR).
    pub fn extra_bytes(&self) -> Vec<ExtraBytes> {
        self.all_vlrs()
            .filter(|v| v.is(LASF_SPEC, 4))
            .flat_map(|v| v.data.as_chunks::<192>().0.iter().map(|c| ExtraBytes::parse(c)).collect::<Vec<_>>())
            .collect()
    }

    /// Writes the header block for `version` (227, 235 or 375 bytes), the VLRs, and pads to
    /// `offset_to_point_data`.
    pub(crate) fn write<W: Write>(&self, w: &mut W) -> Result<()> {
        let size = header_size_for(self.version);
        let mut b = vec![0u8; size];
        b[0..4].copy_from_slice(b"LASF");
        b[4..6].copy_from_slice(&self.file_source_id.to_le_bytes());
        b[6..8].copy_from_slice(&self.global_encoding.to_le_bytes());
        b[8..24].copy_from_slice(&parse_guid(&self.guid));
        b[24] = self.version.0;
        b[25] = self.version.1;
        b[26..58].copy_from_slice(&fixed_str(&self.system_identifier, 32));
        b[58..90].copy_from_slice(&fixed_str(&self.generating_software, 32));
        b[90..92].copy_from_slice(&self.creation_day.to_le_bytes());
        b[92..94].copy_from_slice(&self.creation_year.to_le_bytes());
        b[94..96].copy_from_slice(&(size as u16).to_le_bytes());
        b[96..100].copy_from_slice(&self.offset_to_point_data.to_le_bytes());
        b[100..104].copy_from_slice(&(self.vlrs.len() as u32).to_le_bytes());
        // LASzip marks compressed point formats with bit 7.
        b[104] = self.point_format | if self.compressed { 0x80 } else { 0 };
        b[105..107].copy_from_slice(&self.point_record_length.to_le_bytes());
        // The legacy 32-bit counts: zero when they cannot hold the values, or for the formats
        // LAS 1.4 added.
        let legacy = self.point_format <= 5 && self.point_count <= u32::MAX as u64;
        let legacy_count = if legacy { self.point_count as u32 } else { 0 };
        b[107..111].copy_from_slice(&legacy_count.to_le_bytes());
        for i in 0..5 {
            let n = self.points_by_return.get(i).copied().unwrap_or(0);
            let n = if legacy && n <= u32::MAX as u64 { n as u32 } else { 0 };
            b[111 + 4 * i..115 + 4 * i].copy_from_slice(&n.to_le_bytes());
        }
        for i in 0..3 {
            b[131 + 8 * i..139 + 8 * i].copy_from_slice(&self.scale[i].to_le_bytes());
            b[155 + 8 * i..163 + 8 * i].copy_from_slice(&self.offset[i].to_le_bytes());
            b[179 + 16 * i..187 + 16 * i].copy_from_slice(&self.max[i].to_le_bytes());
            b[187 + 16 * i..195 + 16 * i].copy_from_slice(&self.min[i].to_le_bytes());
        }
        if size >= 235 {
            b[227..235].copy_from_slice(&self.start_of_waveform_data.to_le_bytes());
        }
        if size >= 375 {
            b[235..243].copy_from_slice(&self.start_of_first_evlr.to_le_bytes());
            b[243..247].copy_from_slice(&(self.evlrs.len() as u32).to_le_bytes());
            b[247..255].copy_from_slice(&self.point_count.to_le_bytes());
            for i in 0..15 {
                let n = self.points_by_return.get(i).copied().unwrap_or(0);
                b[255 + 8 * i..263 + 8 * i].copy_from_slice(&n.to_le_bytes());
            }
        }
        w.write_all(&b)?;
        let mut pos = size;
        for v in &self.vlrs {
            v.write(w, false)?;
            pos += v.file_len(false);
        }
        let pad = (self.offset_to_point_data as usize).saturating_sub(pos);
        w.write_all(&vec![0u8; pad])?;
        Ok(())
    }

    /// The offset of the point records when the header and VLRs are written for this version.
    pub(crate) fn data_offset(&self) -> usize {
        header_size_for(self.version) + self.vlrs.iter().map(|v| v.file_len(false)).sum::<usize>()
    }

    /// Writes the EVLRs.
    pub(crate) fn write_evlrs<W: Write>(&self, w: &mut W) -> Result<()> {
        for v in &self.evlrs {
            v.write(w, true)?;
        }
        Ok(())
    }
}

/// The header size of a LAS version.
pub(crate) fn header_size_for(version: (u8, u8)) -> usize {
    match version {
        (1, 0..=2) => 227,
        (1, 3) => 235,
        _ => 375,
    }
}

/// Reads as many bytes as available, up to the buffer's size.
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// A NUL-terminated string from a fixed-size field.
pub(crate) fn c_str(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

/// A string as a NUL-padded fixed-size field.
pub(crate) fn fixed_str(s: &str, len: usize) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.truncate(len);
    b.resize(len, 0);
    b
}

/// Formats a GUID as LAS stores it: a little-endian u32, two u16 and 8 bytes.
fn format_guid(b: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes(b[0..4].try_into().unwrap_or_default());
    let d2 = u16::from_le_bytes([b[4], b[5]]);
    let d3 = u16::from_le_bytes([b[6], b[7]]);
    format!(
        "{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn parse_guid(s: &str) -> [u8; 16] {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let mut raw = [0u8; 16];
    if hex.len() != 32 {
        return raw;
    }
    for (i, r) in raw.iter_mut().enumerate() {
        *r = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0);
    }
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&u32::from_be_bytes(raw[0..4].try_into().unwrap_or_default()).to_le_bytes());
    b[4..6].copy_from_slice(&u16::from_be_bytes([raw[4], raw[5]]).to_le_bytes());
    b[6..8].copy_from_slice(&u16::from_be_bytes([raw[6], raw[7]]).to_le_bytes());
    b[8..16].copy_from_slice(&raw[8..16]);
    b
}

/// An extra-bytes descriptor (LAS 1.4, `LASF_Spec` record 4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtraBytes {
    /// The data type code: 0 for undocumented bytes, 1 to 10 for scalars, 11 to 30 for the
    /// deprecated 2- and 3-element arrays.
    pub data_type: u8,
    /// The options bits: no data, min, max, scale and offset are set.
    pub options: u8,
    /// The name.
    pub name: String,
    /// The no-data values (one per element).
    pub no_data: [f64; 3],
    /// The scales (one per element).
    pub scale: [f64; 3],
    /// The offsets (one per element).
    pub offset: [f64; 3],
    /// The description.
    pub description: String,
}

impl ExtraBytes {
    fn parse(b: &[u8]) -> Self {
        let data_type = b[2];
        let options = b[3];
        let typ = ValueType::from_las_code(data_type).map(|(t, _)| t);
        let any = |o: usize| -> f64 {
            let raw: [u8; 8] = b[o..o + 8].try_into().unwrap_or_default();
            match typ {
                Some(ValueType::F32 | ValueType::F64) => f64::from_le_bytes(raw),
                Some(ValueType::I8 | ValueType::I16 | ValueType::I32 | ValueType::I64) => i64::from_le_bytes(raw) as f64,
                _ => u64::from_le_bytes(raw) as f64,
            }
        };
        let f = |o: usize| f64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        Self {
            data_type,
            options,
            name: c_str(&b[4..36]),
            no_data: [any(40), any(48), any(56)],
            scale: [f(112), f(120), f(128)],
            offset: [f(136), f(144), f(152)],
            description: c_str(&b[160..192]),
        }
    }

    /// The descriptor of a dimension, as stored.
    pub(crate) fn for_dimension(d: &Dimension) -> Self {
        let mut options = 0;
        let t = d.values.value_type();
        if d.no_data.is_some() {
            options |= 1;
        }
        if d.scale.is_some() {
            options |= 8;
        }
        if d.offset.is_some() {
            options |= 16;
        }
        Self {
            data_type: t.las_code(),
            options,
            name: d.name.clone(),
            no_data: [d.no_data.unwrap_or(0.0), 0.0, 0.0],
            scale: [d.scale.unwrap_or(0.0), 0.0, 0.0],
            offset: [d.offset.unwrap_or(0.0), 0.0, 0.0],
            description: d.description.clone(),
        }
    }

    /// The 192-byte record.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; 192];
        b[2] = self.data_type;
        b[3] = self.options;
        b[4..36].copy_from_slice(&fixed_str(&self.name, 32));
        let typ = ValueType::from_las_code(self.data_type).map(|(t, _)| t);
        let any = |x: f64| -> [u8; 8] {
            match typ {
                Some(ValueType::F32 | ValueType::F64) => x.to_le_bytes(),
                Some(ValueType::I8 | ValueType::I16 | ValueType::I32 | ValueType::I64) => (x as i64).to_le_bytes(),
                _ => (x as u64).to_le_bytes(),
            }
        };
        for k in 0..3 {
            b[40 + 8 * k..48 + 8 * k].copy_from_slice(&any(self.no_data[k]));
            b[112 + 8 * k..120 + 8 * k].copy_from_slice(&self.scale[k].to_le_bytes());
            b[136 + 8 * k..144 + 8 * k].copy_from_slice(&self.offset[k].to_le_bytes());
        }
        b[160..192].copy_from_slice(&fixed_str(&self.description, 32));
        b
    }

    /// The size in bytes of the values it describes.
    pub fn size(&self) -> usize {
        match ValueType::from_las_code(self.data_type) {
            Some((t, n)) => t.size() * n,
            None => self.options as usize,
        }
    }
}

/// The size of a point data format's standard fields.
pub(crate) fn standard_record_length(format: u8) -> usize {
    [20, 28, 26, 34, 57, 63, 30, 36, 38, 59, 67][format.min(10) as usize]
}

/// Where the optional parts of a format's records are: `(gps_time, rgb, nir, wave packet)`.
fn optional_offsets(format: u8) -> (Option<usize>, Option<usize>, Option<usize>, Option<usize>) {
    match format {
        0 => (None, None, None, None),
        1 => (Some(20), None, None, None),
        2 => (None, Some(20), None, None),
        3 => (Some(20), Some(28), None, None),
        4 => (Some(20), None, None, Some(28)),
        5 => (Some(20), Some(28), None, Some(34)),
        6 => (Some(22), None, None, None),
        7 => (Some(22), Some(30), None, None),
        8 => (Some(22), Some(30), Some(36), None),
        9 => (Some(22), None, None, Some(30)),
        _ => (Some(22), Some(30), Some(36), Some(38)),
    }
}

/// How one dimension is stored in a point record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Field {
    X,
    Y,
    Z,
    /// A little-endian value of a type at an offset.
    Plain(usize),
    /// Bits `shift..shift + width` of the byte at an offset.
    Bits(usize, u8, u8),
    /// Extra bytes: a value at an offset.
    Extra(usize),
}

/// A dimension of a point record format.
#[derive(Debug, Clone)]
pub(crate) struct FieldSpec {
    pub name: String,
    pub field: Field,
    pub value_type: ValueType,
    pub scale: Option<f64>,
    pub offset: Option<f64>,
    pub no_data: Option<f64>,
    pub description: String,
}

/// The layout of a point record: its dimensions and where they are.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub format: u8,
    pub record_len: usize,
    pub fields: Vec<FieldSpec>,
}

impl Layout {
    /// The layout of a header's records, extra bytes included.
    pub fn new(header: &LasHeader) -> Self {
        let format = header.point_format;
        let mut fields = standard_fields(format);
        let std_len = standard_record_length(format);
        let record_len = header.point_record_length as usize;
        let mut at = std_len;
        for eb in header.extra_bytes() {
            let size = eb.size();
            if at + size > record_len {
                break;
            }
            match ValueType::from_las_code(eb.data_type) {
                Some((t, count)) => {
                    for k in 0..count {
                        let name = if count == 1 { eb.name.clone() } else { format!("{}_{k}", eb.name) };
                        fields.push(FieldSpec {
                            name,
                            field: Field::Extra(at + k * t.size()),
                            value_type: t,
                            scale: (eb.options & 8 != 0).then_some(eb.scale[k]),
                            offset: (eb.options & 16 != 0).then_some(eb.offset[k]),
                            no_data: (eb.options & 1 != 0).then_some(eb.no_data[k]),
                            description: eb.description.clone(),
                        });
                    }
                }
                None => {
                    // Undocumented bytes: one byte dimension each.
                    for k in 0..size {
                        fields.push(extra_byte_field(&format!("{}_{k}", if eb.name.is_empty() { "extra" } else { &eb.name }), at + k));
                    }
                }
            }
            at += size;
        }
        // Bytes not described by any descriptor.
        for k in at..record_len {
            fields.push(extra_byte_field(&format!("extra_byte_{}", k - std_len), k));
        }
        Self { format, record_len, fields }
    }

    /// The layout of a format with these extra dimensions after the standard fields.
    pub fn with_extra(format: u8, extra: &[&Dimension]) -> Self {
        let mut fields = standard_fields(format);
        let mut at = standard_record_length(format);
        for d in extra {
            let t = d.values.value_type();
            fields.push(FieldSpec {
                name: d.name.clone(),
                field: Field::Extra(at),
                value_type: t,
                scale: d.scale,
                offset: d.offset,
                no_data: d.no_data,
                description: d.description.clone(),
            });
            at += t.size();
        }
        Self { format, record_len: at, fields }
    }
}

fn extra_byte_field(name: &str, at: usize) -> FieldSpec {
    FieldSpec {
        name: name.to_string(),
        field: Field::Extra(at),
        value_type: ValueType::U8,
        scale: None,
        offset: None,
        no_data: None,
        description: String::new(),
    }
}

/// The standard dimensions of a point data format.
pub(crate) fn standard_fields(format: u8) -> Vec<FieldSpec> {
    use Field::*;
    use ValueType as T;
    let mut f: Vec<(&str, Field, ValueType)> = vec![("x", X, T::F64), ("y", Y, T::F64), ("z", Z, T::F64), ("intensity", Plain(12), T::U16)];
    if format <= 5 {
        f.extend([
            ("return_number", Bits(14, 0, 3), T::U8),
            ("number_of_returns", Bits(14, 3, 3), T::U8),
            ("scan_direction_flag", Bits(14, 6, 1), T::U8),
            ("edge_of_flight_line", Bits(14, 7, 1), T::U8),
            ("classification", Bits(15, 0, 5), T::U8),
            ("synthetic", Bits(15, 5, 1), T::U8),
            ("key_point", Bits(15, 6, 1), T::U8),
            ("withheld", Bits(15, 7, 1), T::U8),
            ("scan_angle_rank", Plain(16), T::I8),
            ("user_data", Plain(17), T::U8),
            ("point_source_id", Plain(18), T::U16),
        ]);
    } else {
        f.extend([
            ("return_number", Bits(14, 0, 4), T::U8),
            ("number_of_returns", Bits(14, 4, 4), T::U8),
            ("synthetic", Bits(15, 0, 1), T::U8),
            ("key_point", Bits(15, 1, 1), T::U8),
            ("withheld", Bits(15, 2, 1), T::U8),
            ("overlap", Bits(15, 3, 1), T::U8),
            ("scanner_channel", Bits(15, 4, 2), T::U8),
            ("scan_direction_flag", Bits(15, 6, 1), T::U8),
            ("edge_of_flight_line", Bits(15, 7, 1), T::U8),
            ("classification", Plain(16), T::U8),
            ("user_data", Plain(17), T::U8),
            ("scan_angle", Plain(18), T::I16),
            ("point_source_id", Plain(20), T::U16),
        ]);
    }
    let (gps, rgb, nir, wave) = optional_offsets(format);
    if let Some(o) = gps {
        f.push(("gps_time", Plain(o), T::F64));
    }
    if let Some(o) = rgb {
        f.extend([("red", Plain(o), T::U16), ("green", Plain(o + 2), T::U16), ("blue", Plain(o + 4), T::U16)]);
    }
    if let Some(o) = nir {
        f.push(("nir", Plain(o), T::U16));
    }
    if let Some(o) = wave {
        f.extend([
            ("wavepacket_index", Plain(o), T::U8),
            ("wavepacket_offset", Plain(o + 1), T::U64),
            ("wavepacket_size", Plain(o + 9), T::U32),
            ("return_point_wave_location", Plain(o + 13), T::F32),
            ("x_t", Plain(o + 17), T::F32),
            ("y_t", Plain(o + 21), T::F32),
            ("z_t", Plain(o + 25), T::F32),
        ]);
    }
    f.into_iter()
        .map(|(name, field, value_type)| FieldSpec {
            name: name.to_string(),
            field,
            value_type,
            scale: None,
            offset: None,
            no_data: None,
            description: String::new(),
        })
        .collect()
}

/// The fields of a record a point filter looks at, decoded.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordCore {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub return_number: u8,
    pub number_of_returns: u8,
    pub classification: u8,
}

/// Decodes the coordinates, return numbers and class of a record.
pub(crate) fn record_core(rec: &[u8], format: u8, scale: &[f64; 3], offset: &[f64; 3]) -> RecordCore {
    let i = |o: usize| i32::from_le_bytes(rec[o..o + 4].try_into().unwrap_or_default()) as f64;
    let (rn, nr, class) = if format <= 5 { (rec[14] & 7, (rec[14] >> 3) & 7, rec[15] & 31) } else { (rec[14] & 15, rec[14] >> 4, rec[16]) };
    RecordCore {
        x: i(0) * scale[0] + offset[0],
        y: i(4) * scale[1] + offset[1],
        z: i(8) * scale[2] + offset[2],
        return_number: rn,
        number_of_returns: nr,
        classification: class,
    }
}

/// Column builders for the dimensions of a layout.
pub(crate) struct Columns {
    pub specs: Vec<FieldSpec>,
    pub values: Vec<Values>,
}

impl Columns {
    pub fn new(layout: &Layout, capacity: usize) -> Self {
        Self { specs: layout.fields.clone(), values: layout.fields.iter().map(|f| Values::with_capacity(f.value_type, capacity)).collect() }
    }

    /// The number of points.
    pub fn len(&self) -> usize {
        self.values.first().map_or(0, |v| v.len())
    }

    /// Appends the dimensions of a record.
    pub fn push(&mut self, rec: &[u8], scale: &[f64; 3], offset: &[f64; 3]) {
        for (spec, col) in self.specs.iter().zip(self.values.iter_mut()) {
            match (spec.field, col) {
                (Field::X, Values::F64(v)) => v.push(raw_i32(rec, 0) as f64 * scale[0] + offset[0]),
                (Field::Y, Values::F64(v)) => v.push(raw_i32(rec, 4) as f64 * scale[1] + offset[1]),
                (Field::Z, Values::F64(v)) => v.push(raw_i32(rec, 8) as f64 * scale[2] + offset[2]),
                (Field::Bits(o, shift, width), Values::U8(v)) => v.push((rec[o] >> shift) & ((1u16 << width) - 1) as u8),
                (Field::Plain(o) | Field::Extra(o), col) => col.push_le(&rec[o..]),
                _ => {}
            }
        }
    }

    pub fn into_dimensions(self) -> Vec<Dimension> {
        self.specs
            .into_iter()
            .zip(self.values)
            .map(|(s, values)| Dimension {
                name: s.name,
                values,
                scale: s.scale,
                offset: s.offset,
                no_data: s.no_data,
                description: s.description,
            })
            .collect()
    }
}

fn raw_i32(rec: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(rec[o..o + 4].try_into().unwrap_or_default())
}

/// Encodes points into records of a layout.
pub(crate) struct Encoder<'a> {
    layout: &'a Layout,
    /// For each field of the layout, the source dimension.
    sources: Vec<Source<'a>>,
    scale: [f64; 3],
    offset: [f64; 3],
}

enum Source<'a> {
    None,
    Dim(&'a Dimension),
    /// The legacy scan angle rank (degrees) from a LAS 1.4 scan angle (0.006 degree units).
    RankFromAngle(&'a Dimension),
    /// The scan angle from a rank.
    AngleFromRank(&'a Dimension),
}

impl<'a> Encoder<'a> {
    pub fn new(layout: &'a Layout, dims: &'a [Dimension], scale: [f64; 3], offset: [f64; 3]) -> Self {
        let find = |name: &str| dims.iter().find(|d| d.name == name);
        let sources = layout
            .fields
            .iter()
            .map(|f| match find(&f.name) {
                Some(d) => Source::Dim(d),
                None => match f.name.as_str() {
                    "scan_angle_rank" => find("scan_angle").map_or(Source::None, Source::RankFromAngle),
                    "scan_angle" => find("scan_angle_rank").map_or(Source::None, Source::AngleFromRank),
                    _ => Source::None,
                },
            })
            .collect();
        Self { layout, sources, scale, offset }
    }

    /// Encodes point `i` into `out` (one record, zeroed).
    pub fn encode(&self, i: usize, out: &mut [u8]) -> Result<()> {
        for (spec, src) in self.layout.fields.iter().zip(&self.sources) {
            let d = match src {
                Source::None => continue,
                Source::Dim(d) => *d,
                Source::RankFromAngle(d) => {
                    if let Field::Plain(o) = spec.field {
                        out[o] = (d.values.get_f64(i) * 0.006).round().clamp(-128.0, 127.0) as i8 as u8;
                    }
                    continue;
                }
                Source::AngleFromRank(d) => {
                    if let Field::Plain(o) = spec.field {
                        let v = (d.values.get_f64(i) / 0.006).round().clamp(-32768.0, 32767.0) as i16;
                        out[o..o + 2].copy_from_slice(&v.to_le_bytes());
                    }
                    continue;
                }
            };
            match spec.field {
                Field::X | Field::Y | Field::Z => {
                    let k = match spec.field {
                        Field::X => 0,
                        Field::Y => 1,
                        _ => 2,
                    };
                    let v = d.values.get_f64(i);
                    let raw = ((v - self.offset[k]) / self.scale[k]).round();
                    if !(i32::MIN as f64..=i32::MAX as f64).contains(&raw) {
                        return Err(Error::InvalidArgument(format!(
                            "{} = {v} does not fit in a LAS record with scale {} and offset {}",
                            spec.name, self.scale[k], self.offset[k]
                        )));
                    }
                    out[4 * k..4 * k + 4].copy_from_slice(&(raw as i32).to_le_bytes());
                }
                Field::Bits(o, shift, width) => {
                    let max = (1u16 << width) - 1;
                    let v = (d.values.get_f64(i).max(0.0) as u16).min(max) as u8;
                    out[o] |= v << shift;
                }
                Field::Plain(o) | Field::Extra(o) => {
                    if d.values.value_type() == spec.value_type {
                        d.values.write_le(i, &mut out[o..]);
                    } else {
                        let mut tmp = Values::with_capacity(spec.value_type, 1);
                        tmp.push_f64(d.values.get_f64(i));
                        tmp.write_le(0, &mut out[o..]);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_round_trips() {
        let s = "01020304-0506-0708-090a-0b0c0d0e0f10";
        let b = parse_guid(s);
        assert_eq!(format_guid(&b), s);
        assert_eq!(&b[0..4], &[4, 3, 2, 1]);
    }

    #[test]
    fn record_lengths_match_layouts() {
        for f in 0..=10u8 {
            let fields = standard_fields(f);
            let end = fields
                .iter()
                .map(|s| match s.field {
                    Field::X | Field::Y | Field::Z => 12,
                    Field::Bits(o, ..) => o + 1,
                    Field::Plain(o) | Field::Extra(o) => o + s.value_type.size(),
                })
                .max()
                .unwrap();
            assert_eq!(end, standard_record_length(f), "format {f}");
        }
    }
}
