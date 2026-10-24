//! The data representations of GRIB2 (section 5) decoded as GDAL's GRIB driver (its copy of NCEP's
//! g2clib) decodes them, in single precision with g2clib's arithmetic, so that the values are
//! GDAL's to the bit: simple packing (5.0), complex packing with and without spatial differencing
//! (5.2, 5.3), IEEE floating point (5.4), JPEG 2000 (5.40, with terrio's OpenJPEG port), PNG
//! (5.41) and CCSDS (5.42, with gribberish's pure-Rust decoder).

use crate::{Error, Result};

/// A value of the bit stream: `nbits` bits (at most 32) from bit `pos`, most significant first,
/// read as g2clib's `gbit` reads them into a 32-bit int (bits past the end read as zeros).
fn bits(data: &[u8], pos: usize, nbits: u32) -> i32 {
    let mut v: u64 = 0;
    let (mut p, mut left) = (pos, nbits);
    while left > 0 {
        let byte = data.get(p / 8).copied().unwrap_or(0);
        let off = (p % 8) as u32;
        let take = (8 - off).min(left);
        let chunk = (u64::from(byte) >> (8 - off - take)) & ((1u64 << take) - 1);
        v = (v << take) | chunk;
        p += take as usize;
        left -= take;
    }
    v as u32 as i32
}

/// g2clib's `gbits`: `n` values of `nbits` bits from bit `pos`.
fn gbits(data: &[u8], pos: usize, nbits: u32, n: usize) -> Vec<i32> {
    if nbits == 0 {
        return vec![0; n];
    }
    (0..n).map(|i| bits(data, pos + i * nbits as usize, nbits)).collect()
}

/// g2clib's `int_power`.
fn int_power(x: f64, y: i32) -> f64 {
    let (mut x, mut y) = if y < 0 { (1.0 / x, -i64::from(y)) } else { (x, i64::from(y)) };
    let mut value = 1.0;
    while y != 0 {
        if y & 1 != 0 {
            value *= x;
        }
        x *= x;
        y >>= 1;
    }
    value
}

/// g2clib's `DoubleToFloatClamp`.
fn clamp_f32(v: f64) -> f32 {
    if v >= f64::from(f32::MAX) {
        f32::MAX
    } else if v <= -f64::from(f32::MAX) {
        -f32::MAX
    } else {
        v as f32
    }
}

/// g2clib's `rdieee`: an IEEE single, infinities and NaNs read as +/-1e37.
fn rdieee(bits: u32) -> f32 {
    if (bits >> 23) & 0xff == 255 { if bits >> 31 == 1 { -1e37 } else { 1e37 } } else { f32::from_bits(bits) }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b.get(i).copied().unwrap_or(0), b.get(i + 1).copied().unwrap_or(0)])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([0, 1, 2, 3].map(|k| b.get(i + k).copied().unwrap_or(0)))
}

/// A sign-and-magnitude 16-bit value (scale factors).
fn s16_at(b: &[u8], i: usize) -> i32 {
    let v = u16_at(b, i);
    let m = i32::from(v & 0x7fff);
    if v & 0x8000 != 0 { -m } else { m }
}

/// The common values of the packing templates (octets 12 to 21 of section 5): the reference value
/// (as g2clib reads it), the binary and decimal scale factors, the bit count and the type of the
/// original values (0 float, 1 integer).
#[derive(Debug, Clone, Copy)]
pub(super) struct Packing {
    pub(super) reference: f32,
    pub(super) binary_scale: i32,
    pub(super) decimal_scale: i32,
    pub(super) nbits: u32,
    pub(super) field_type: u8,
}

impl Packing {
    pub(super) fn of(sec5: &[u8]) -> Self {
        Self {
            reference: rdieee(u32_at(sec5, 11)),
            binary_scale: s16_at(sec5, 15),
            decimal_scale: s16_at(sec5, 17),
            nbits: u32::from(sec5.get(19).copied().unwrap_or(0)),
            field_type: sec5.get(20).copied().unwrap_or(0),
        }
    }

    /// `bscale` and `dscale` as g2clib's simple, complex and JPEG 2000 unpackers compute them.
    fn scales(&self) -> (f32, f32) {
        (clamp_f32(int_power(2.0, self.binary_scale)), clamp_f32(int_power(10.0, -self.decimal_scale)))
    }

    /// `((X * bscale) + ref) * dscale`, in single precision.
    fn scale(&self, ints: &[i32]) -> Vec<f32> {
        let (bscale, dscale) = self.scales();
        ints.iter().map(|&x| ((x as f32 * bscale) + self.reference) * dscale).collect()
    }

    /// `refD + bdscale * X`, as g2clib's PNG and CCSDS unpackers compute values.
    fn scale_png(&self, ints: &[i32]) -> Vec<f32> {
        let bscale = int_power(2.0, self.binary_scale) as f32;
        let dscale = int_power(10.0, -self.decimal_scale) as f32;
        let bdscale = bscale * dscale;
        let ref_d = self.reference * dscale;
        ints.iter().map(|&x| ref_d + bdscale * x as f32).collect()
    }

    fn ref_d_png(&self) -> f32 {
        self.reference * int_power(10.0, -self.decimal_scale) as f32
    }
}

/// The values a field's data section packs: `ndpts` of them (the points the bitmap keeps).
/// `sec5` is the whole section 5, `data` section 7 without its 5-octet header.
pub(super) fn unpack(sec5: &[u8], data: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    let template = u16_at(sec5, 9);
    let p = Packing::of(sec5);
    match template {
        0 => Ok(if p.nbits != 0 {
            p.scale(&gbits(data, 0, p.nbits, ndpts))
        } else {
            let (_, dscale) = p.scales();
            vec![p.reference * dscale; ndpts]
        }),
        2 | 3 => complex(sec5, data, ndpts, template),
        4 => match sec5.get(11).copied().unwrap_or(0) {
            1 => {
                if data.len() / 4 < ndpts {
                    return Err(Error::InvalidData("GRIB2 IEEE data section too short".into()));
                }
                Ok(data.as_chunks::<4>().0.iter().take(ndpts).map(|c| f32::from_be_bytes(*c)).collect())
            }
            2 => {
                if data.len() / 8 < ndpts {
                    return Err(Error::InvalidData("GRIB2 IEEE data section too short".into()));
                }
                Ok(data.as_chunks::<8>().0.iter().take(ndpts).map(|c| clamp_f32(f64::from_be_bytes(*c))).collect())
            }
            n => Err(Error::Unsupported(format!("GRIB2 IEEE packing of precision {n}"))),
        },
        40 | 40000 => jpeg2000(&p, data, ndpts),
        41 | 40010 => png(&p, data, ndpts),
        42 => ccsds(sec5, &p, data, ndpts),
        t => Err(Error::Unsupported(format!("GRIB2 data representation template 5.{t}"))),
    }
}

/// g2clib's `comunpack`: complex packing (5.2) and complex packing with spatial differencing (5.3).
fn complex(sec5: &[u8], data: &[u8], ndpts: usize, template: u16) -> Result<Vec<f32>> {
    let bad = || Error::InvalidData("invalid GRIB2 complex packing".into());
    let p = Packing::of(sec5);
    let (bscale, dscale) = p.scales();
    let ref_ = p.reference;
    let nbitsgref = p.nbits;
    let itype = p.field_type;
    let missing_mgmt = sec5.get(22).copied().unwrap_or(0);
    let miss1 = u32_at(sec5, 23);
    let miss2 = u32_at(sec5, 27);
    let ngroups = u32_at(sec5, 31) as i64;
    let width_ref = i64::from(sec5.get(35).copied().unwrap_or(0));
    let nbitsgwidth = u32::from(sec5.get(36).copied().unwrap_or(0));
    let len_ref = u32_at(sec5, 37) as i32 as i64;
    let len_incr = i64::from(sec5.get(41).copied().unwrap_or(0));
    let last_len = u32_at(sec5, 42) as i32 as i64;
    let nbitsglen = u32::from(sec5.get(46).copied().unwrap_or(0));
    let order = sec5.get(47).copied().unwrap_or(0);
    let nbitsd = if template == 3 { u32::from(sec5.get(48).copied().unwrap_or(0)) * 8 } else { 0 };
    let n = ndpts as i64;
    if ngroups == 0 {
        return Ok(vec![ref_; ndpts]);
    }
    if ngroups < 0 || ngroups - 10 > n / 2 {
        return Err(bad());
    }
    if len_ref < 0 || last_len < 0 || last_len > n {
        return Err(bad());
    }
    if nbitsglen == 0 && ((ngroups > 1 && len_ref != (n - last_len) / (ngroups - 1)) || len_ref * (ngroups - 1) + last_len != n) {
        return Err(bad());
    }
    let (rmiss1, rmiss2) = if itype == 0 { (rdieee(miss1), rdieee(miss2)) } else { (miss1 as i32 as f32, miss2 as i32 as f32) };
    let mut pos = 0usize;
    let (mut ival1, mut ival2, mut minsd) = (0i32, 0i32, 0i32);
    if template == 3 && nbitsd != 0 {
        ival1 = bits(data, pos, nbitsd);
        pos += nbitsd as usize;
        if order == 2 {
            ival2 = bits(data, pos, nbitsd);
            pos += nbitsd as usize;
        }
        let sign = bits(data, pos, 1);
        pos += 1;
        minsd = bits(data, pos, nbitsd - 1);
        pos += nbitsd as usize - 1;
        if sign == 1 && minsd != i32::MIN {
            minsd = -minsd;
        }
    }
    let ng = ngroups as usize;
    let gref = if nbitsgref != 0 { gbits(data, pos, nbitsgref, ng) } else { vec![0; ng] };
    if nbitsgref != 0 {
        pos += (nbitsgref as usize * ng).div_ceil(8) * 8;
    }
    let mut gwidth: Vec<i64> =
        if nbitsgwidth != 0 { gbits(data, pos, nbitsgwidth, ng).into_iter().map(i64::from).collect() } else { vec![0; ng] };
    if nbitsgwidth != 0 {
        pos += (nbitsgwidth as usize * ng).div_ceil(8) * 8;
    }
    for w in &mut gwidth {
        *w += width_ref;
    }
    let mut glen: Vec<i64> =
        if nbitsglen != 0 { gbits(data, pos, nbitsglen, ng).into_iter().map(i64::from).collect() } else { vec![0; ng] };
    if nbitsglen != 0 {
        pos += (nbitsglen as usize * ng).div_ceil(8) * 8;
    }
    for l in glen.iter_mut().take(ng - 1) {
        *l = *l * len_incr + len_ref;
    }
    glen[ng - 1] = last_len;
    let (mut tot_bits, mut tot_len) = (0i64, 0i64);
    for j in 0..ng {
        if gwidth[j] < 0 || glen[j] < 0 || gwidth[j] > 32 {
            return Err(bad());
        }
        tot_bits += gwidth[j] * glen[j];
        tot_len += glen[j];
    }
    if tot_len != n || tot_bits as f64 / 8.0 > (data.len() + 5) as f64 {
        return Err(bad());
    }
    let mut ifld = vec![0i32; ndpts];
    // 0: a value, 1: primary missing, 2: secondary missing.
    let mut miss = vec![0u8; if missing_mgmt == 1 || missing_mgmt == 2 { ndpts } else { 0 }];
    let mut non = 0usize;
    let mut k = 0usize;
    if missing_mgmt == 0 {
        for j in 0..ng {
            let (w, l) = (gwidth[j] as u32, glen[j] as usize);
            if w != 0 {
                for i in 0..l {
                    ifld[k + i] = bits(data, pos + i * w as usize, w).wrapping_add(gref[j]);
                }
            } else {
                ifld[k..k + l].fill(gref[j]);
            }
            k += l;
            pos += w as usize * l;
        }
    } else if missing_mgmt == 1 || missing_mgmt == 2 {
        for j in 0..ng {
            let (w, l) = (gwidth[j] as u32, glen[j] as usize);
            if w != 0 {
                let msng1 = int_power(2.0, w as i32) as i64 - 1;
                let msng2 = msng1 - 1;
                for i in 0..l {
                    let v = i64::from(bits(data, pos + i * w as usize, w));
                    if v == msng1 {
                        miss[k + i] = 1;
                    } else if missing_mgmt == 2 && v == msng2 {
                        miss[k + i] = 2;
                    } else {
                        ifld[non] = (v as i32).wrapping_add(gref[j]);
                        non += 1;
                    }
                }
                pos += w as usize * l;
            } else {
                let msng1 = int_power(2.0, nbitsgref as i32) as i64 - 1;
                let msng2 = msng1 - 1;
                let g = i64::from(gref[j]);
                if g == msng1 {
                    miss[k..k + l].fill(1);
                } else if missing_mgmt == 2 && g == msng2 {
                    miss[k..k + l].fill(2);
                } else {
                    ifld[non..non + l].fill(gref[j]);
                    non += l;
                }
            }
            k += l;
        }
    }
    if template == 3 {
        let count = if missing_mgmt == 0 { ndpts } else { non };
        if order == 1 && count > 0 {
            ifld[0] = ival1;
            for i in 1..count {
                let v = i64::from(ifld[i]) + i64::from(minsd) + i64::from(ifld[i - 1]);
                ifld[i] = i32::try_from(v).map_err(|_| bad())?;
            }
        } else if order == 2 && count > 0 {
            ifld[0] = ival1;
            if count > 1 {
                ifld[1] = ival2;
            }
            for i in 2..count {
                let tmp = f64::from(ifld[i]) + f64::from(minsd) + 2.0 * f64::from(ifld[i - 1]) - f64::from(ifld[i - 2]);
                if tmp > f64::from(i32::MAX) || tmp < f64::from(i32::MIN) {
                    return Err(bad());
                }
                ifld[i] = tmp as i32;
            }
        }
    }
    let scale = |x: i32| ((x as f32 * bscale) + ref_) * dscale;
    if missing_mgmt == 1 || missing_mgmt == 2 {
        let mut non = 0;
        Ok(miss
            .iter()
            .map(|m| match m {
                0 => {
                    let v = scale(ifld[non]);
                    non += 1;
                    v
                }
                1 => rmiss1,
                _ => rmiss2,
            })
            .collect())
    } else {
        Ok(ifld.iter().map(|&x| scale(x)).collect())
    }
}

/// JPEG 2000 packing (5.40), decoded with terrio's OpenJPEG port.
#[cfg(feature = "jp2")]
fn jpeg2000(p: &Packing, data: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    if p.nbits == 0 {
        let (_, dscale) = p.scales();
        return Ok(vec![p.reference * dscale; ndpts]);
    }
    let ints = crate::drivers::jp2::decode_ints(data)?;
    if ints.len() != ndpts {
        return Err(Error::InvalidData(format!("GRIB2 JPEG 2000 data of {} values, not {ndpts}", ints.len())));
    }
    Ok(p.scale(&ints))
}

#[cfg(not(feature = "jp2"))]
fn jpeg2000(_: &Packing, _: &[u8], _: usize) -> Result<Vec<f32>> {
    Err(Error::disabled("jp2"))
}

/// PNG packing (5.41): a grey, RGB or RGBA image whose samples are the packed values.
fn png(p: &Packing, data: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    if p.nbits == 0 || ndpts == 0 {
        return Ok(vec![p.ref_d_png(); ndpts]);
    }
    let mut d = png::Decoder::new(std::io::Cursor::new(data));
    d.set_transformations(png::Transformations::IDENTITY);
    let mut reader = d.read_info().map_err(|e| Error::InvalidData(format!("GRIB2 PNG data: {e}")))?;
    // The size first: the image is allocated from its header.
    let (w, h) = (reader.info().width as usize, reader.info().height as usize);
    if w.checked_mul(h) != Some(ndpts) {
        return Err(Error::InvalidData("GRIB2 PNG data of the wrong size".into()));
    }
    // Multi-channel 16-bit samples do not fit in a GRIB packed integer (at most 32 bits).
    let depth = match (reader.info().color_type, reader.info().bit_depth) {
        (png::ColorType::Rgb, png::BitDepth::Eight) => 24,
        (png::ColorType::Rgba, png::BitDepth::Eight) => 32,
        (png::ColorType::Grayscale, depth) => depth as u32,
        _ => return Err(Error::Unsupported("GRIB2 PNG color type or bit depth".into())),
    };
    if depth != p.nbits {
        return Err(Error::InvalidData(format!("GRIB2 PNG data of {depth} bits per value, not {}", p.nbits)));
    }
    let size = reader.output_buffer_size().ok_or_else(|| Error::InvalidData("GRIB2 PNG buffer too large".into()))?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| Error::InvalidData(format!("GRIB2 PNG data: {e}")))?;
    if info.width as usize * info.height as usize != ndpts {
        return Err(Error::InvalidData("GRIB2 PNG data of the wrong size".into()));
    }
    if info.line_size == 0 {
        return Err(Error::InvalidData("GRIB2 PNG data has an empty row".into()));
    }
    // As g2clib copies the rows: whole bytes per value (none below 8 bits).
    let bytes = depth as usize / 8;
    let mut packed = Vec::with_capacity(ndpts * bytes);
    for row in buf.chunks(info.line_size).take(info.height as usize) {
        packed.extend_from_slice(&row[..(info.width as usize * bytes).min(row.len())]);
    }
    let ints = if bytes == 0 { vec![0; ndpts] } else { gbits(&packed, 0, p.nbits, ndpts) };
    Ok(p.scale_png(&ints))
}

/// CCSDS packing (5.42), decoded by gribberish, whose values are turned back into the packed
/// integers to scale them as g2clib does.
fn ccsds(sec5: &[u8], p: &Packing, data: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    use bitvec::view::BitView;
    use gribberish::templates::data_representation::{CCSDSDataRepresentationTemplate, DataRepresentationTemplate};
    if p.nbits == 0 {
        return Ok(vec![p.ref_d_png(); ndpts]);
    }
    let template = CCSDSDataRepresentationTemplate::new(sec5.to_vec());
    let values =
        template.unpack(data.view_bits::<bitvec::order::Msb0>()).map_err(|e| Error::InvalidData(format!("GRIB2 CCSDS data: {e}")))?;
    if values.len() < ndpts {
        return Err(Error::InvalidData(format!("GRIB2 CCSDS data of {} values, not {ndpts}", values.len())));
    }
    // gribberish's scaling: (X * 2^E + R) * 10^-D, in double precision.
    let bsf = 2f64.powi(p.binary_scale);
    let dsf = 10f64.powi(-p.decimal_scale);
    let rv = f64::from(p.reference);
    let forward = |x: f64| (x * bsf + rv) * dsf;
    let ints: Vec<i32> = values[..ndpts]
        .iter()
        .map(|&v| {
            let guess = ((v / dsf - rv) / bsf).round();
            let x = [0.0, -1.0, 1.0, -2.0, 2.0].iter().map(|d| guess + d).find(|x| forward(*x) == v).unwrap_or(guess);
            x as i64 as i32
        })
        .collect();
    Ok(p.scale_png(&ints))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bits_as_g2clib() {
        let data = [0b1010_1100, 0b0101_0011];
        assert_eq!(bits(&data, 0, 4), 0b1010);
        assert_eq!(bits(&data, 4, 6), 0b11_0001);
        assert_eq!(gbits(&data, 0, 3, 5), vec![0b101, 0b011, 0b000, 0b101, 0b001]);
        assert_eq!(bits(&data, 12, 8), 0b0011_0000);
        assert_eq!(int_power(10.0, -2), 1.0 / 10.0 * (1.0 / 10.0));
    }

    #[test]
    fn unpacks_simple_packing() {
        // Reference 1.5, E = 1, D = 1, 4 bits: ((X * 2) + 1.5) * 0.1.
        let mut sec5 = vec![0u8; 21];
        sec5[11..15].copy_from_slice(&1.5f32.to_bits().to_be_bytes());
        sec5[16] = 1;
        sec5[18] = 1;
        sec5[19] = 4;
        let v = unpack(&sec5, &[0x12, 0x30], 3).unwrap();
        let e = |x: f32| ((x * 2.0) + 1.5) * 0.1f64 as f32;
        assert_eq!(v, vec![e(1.0), e(2.0), e(3.0)]);
        // No bits: the reference value, scaled.
        sec5[19] = 0;
        assert_eq!(unpack(&sec5, &[], 2).unwrap(), vec![1.5 * 0.1f64 as f32; 2]);
    }

    #[test]
    fn validates_png_dimensions_and_sample_layout() {
        let image = |color, depth, bytes: &[u8]| {
            let mut out = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut out, 2, 1);
                encoder.set_color(color);
                encoder.set_depth(depth);
                encoder.write_header().unwrap().write_image_data(bytes).unwrap();
            }
            out
        };
        let mut p = Packing { reference: 0.0, binary_scale: 0, decimal_scale: 0, nbits: 16, field_type: 0 };
        let gray = image(png::ColorType::Grayscale, png::BitDepth::Sixteen, &[0, 1, 1, 0]);
        assert_eq!(png(&p, &gray, 2).unwrap(), vec![1.0, 256.0]);
        assert!(matches!(png(&p, &gray, 1), Err(Error::InvalidData(s)) if s.contains("wrong size")));
        assert!(png(&p, &gray, 0).unwrap().is_empty());
        p.nbits = 24;
        let rgb = image(png::ColorType::Rgb, png::BitDepth::Eight, &[0, 0, 1, 0, 1, 0]);
        assert_eq!(png(&p, &rgb, 2).unwrap(), vec![1.0, 256.0]);
        let rgb16 = image(png::ColorType::Rgb, png::BitDepth::Sixteen, &[0; 12]);
        assert!(matches!(png(&p, &rgb16, 2), Err(Error::Unsupported(_))));
        p.nbits = 32;
        let rgba16 = image(png::ColorType::Rgba, png::BitDepth::Sixteen, &[0; 16]);
        assert!(matches!(png(&p, &rgba16, 2), Err(Error::Unsupported(_))));
    }
}
