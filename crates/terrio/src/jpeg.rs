// SPDX-FileCopyrightText: Copyright (C) 1991-1998, Thomas G. Lane.
// SPDX-FileCopyrightText: Copyright (C) 2015, 2020, 2022, 2026, D. R. Commander.
// SPDX-FileCopyrightText: Copyright (C) 1991-1996, Thomas G. Lane.
// SPDX-FileCopyrightText: Copyright 2009 Pierre Ossman <ossman@cendio.se> for Cendio AB
// SPDX-FileCopyrightText: Copyright (C) 2010, 2015-2016, 2022, 2024-2026, D. R. Commander.
// SPDX-FileCopyrightText: Copyright (C) 2015, Google, Inc.
// SPDX-FileCopyrightText: Copyright (C) 2019-2020, Arm Limited.
// SPDX-FileCopyrightText: Copyright (C) 1991-1997, Thomas G. Lane.
// SPDX-FileCopyrightText: Copyright (C) 2009, 2011-2012, 2014-2015, 2022, 2024-2025, D. R. Commander.
// SPDX-FileCopyrightText: Copyright (C) 2013, Linaro Limited.
// SPDX-FileCopyrightText: Copyright (C) 1994-1998, Thomas G. Lane.
// SPDX-FileCopyrightText: Copyright (C) 1999, Ken Murchison.
// SPDX-FileCopyrightText: Copyright (C) 2016, 2022, 2024-2026, D. R. Commander.
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT AND IJG
//
// Derived from (translated to Rust and modified):
//   libjpeg-turbo 3.2.1:
//     src/jidctint.c
//     src/jdsample.c
//     src/jdcolor.c
//     src/jdapimin.c

//! A baseline JPEG decoder that reproduces libjpeg's output, as GDAL decodes JPEG with
//! libjpeg(-turbo): the same integer IDCT (`jidctint.c`, the default "islow" method), "fancy"
//! chroma upsampling (`jdsample.c`), and YCbCr to RGB conversion (`jdcolor.c`), bit for bit.
//!
//! It reads sequential Huffman-coded, 8-bit JPEG (baseline and extended), as libtiff writes in
//! TIFF and GDAL in GeoPackage and MBTiles tiles; progressive and arithmetic-coded streams are
//! left to other decoders.

/// A decoded image: `components` samples per pixel, interleaved, row after row.
#[derive(Debug)]
pub(crate) struct Image {
    pub width: usize,
    pub height: usize,
    pub components: usize,
    pub data: Vec<u8>,
}

/// How the components of a three-component image are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Color {
    /// As coded (libjpeg's `JCS_UNKNOWN` output, as libtiff asks for RGB TIFFs).
    Raw,
    /// Converted from YCbCr to RGB.
    Rgb,
    /// RGB, from YCbCr when the stream's markers and component identifiers say so, as libjpeg
    /// decides (`jdapimin.c`: a JFIF marker, an Adobe marker's transform, identifiers 1, 2, 3).
    Auto,
}

type Result<T> = std::result::Result<T, String>;

/// A frame component.
#[derive(Debug, Clone)]
struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    /// Blocks across and down in the component's plane (whole MCUs).
    bw: usize,
    bh: usize,
    /// Samples across and down in the image (libjpeg's `downsampled_width`/`height`).
    w: usize,
    hgt: usize,
    /// Dequantized coefficients are not kept: blocks are transformed as they are decoded into
    /// this plane, `bw * 8` samples wide.
    plane: Vec<u8>,
}

/// A Huffman table, decoded with code length limits as in the JPEG specification (F.2.2.3).
#[derive(Debug, Clone)]
struct Huffman {
    /// The largest code of each length (index 1..=16), -1 when none.
    maxcode: [i32; 18],
    /// The index in `values` of each length's first code, minus that code.
    valptr: [i32; 17],
    values: Vec<u8>,
    /// Lookup of codes up to 8 bits: (length, value), length 0 when longer.
    fast: Vec<(u8, u8)>,
}

impl Huffman {
    fn new(counts: &[u8; 16], values: Vec<u8>) -> Result<Self> {
        let mut maxcode = [-1i32; 18];
        let mut valptr = [0i32; 17];
        let mut fast = vec![(0u8, 0u8); 256];
        let (mut code, mut k) = (0i32, 0usize);
        for len in 1..=16 {
            let n = counts[len - 1] as usize;
            valptr[len] = k as i32 - code;
            for _ in 0..n {
                if len <= 8 {
                    let shift = 8 - len;
                    let start = (code as usize) << shift;
                    for e in &mut fast[start..start + (1 << shift)] {
                        *e = (len as u8, *values.get(k).ok_or("Huffman table too short")?);
                    }
                }
                code += 1;
                k += 1;
            }
            if n > 0 {
                maxcode[len] = code - 1;
            }
            if code > 1 << len {
                return Err("invalid Huffman table".into());
            }
            code <<= 1;
        }
        maxcode[17] = i32::MAX;
        if k > values.len() {
            return Err("Huffman table too short".into());
        }
        Ok(Self { maxcode, valptr, values, fast })
    }
}

/// Reads entropy-coded bits, skipping stuffed zero bytes and stopping at markers.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    count: u32,
    /// A marker was reached: zero bits follow, as libjpeg supplies.
    marker: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos, acc: 0, count: 0, marker: false }
    }

    fn fill(&mut self) {
        while self.count <= 56 {
            let mut byte = 0u8;
            if !self.marker {
                match self.data.get(self.pos) {
                    Some(0xff) => match self.data.get(self.pos + 1) {
                        Some(0x00) => {
                            byte = 0xff;
                            self.pos += 2;
                        }
                        _ => self.marker = true,
                    },
                    Some(&b) => {
                        byte = b;
                        self.pos += 1;
                    }
                    None => self.marker = true,
                }
            }
            self.acc |= (byte as u64) << (56 - self.count);
            self.count += 8;
        }
    }

    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.fill();
        }
        (self.acc >> (64 - n)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.acc <<= n;
        self.count -= n;
    }

    fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    fn decode(&mut self, t: &Huffman) -> Result<u8> {
        let (len, value) = t.fast[self.peek(8) as usize];
        if len > 0 {
            self.skip(len as u32);
            return Ok(value);
        }
        let code = self.peek(16) as i32;
        for len in 9..=16 {
            let c = code >> (16 - len);
            if c <= t.maxcode[len] {
                self.skip(len as u32);
                return t.values.get((t.valptr[len] + c) as usize).copied().ok_or_else(|| "invalid Huffman code".into());
            }
        }
        Err("invalid Huffman code".into())
    }

    /// A coefficient of `s` bits, sign-extended (F.2.2.1).
    fn receive_extend(&mut self, s: u8) -> i32 {
        if s == 0 {
            return 0;
        }
        let v = self.bits(s as u32) as i32;
        if v < 1 << (s - 1) { v - (1 << s) + 1 } else { v }
    }

    /// Moves past a restart marker, discarding the remaining bits.
    fn restart(&mut self) {
        self.acc = 0;
        self.count = 0;
        self.marker = false;
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xff && (0xd0..=0xd7).contains(&self.data[self.pos + 1]) {
                self.pos += 2;
                return;
            }
            self.pos += 1;
        }
    }
}

/// The zigzag order: the natural index of each coefficient.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56,
    57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Reads baseline frame dimensions without allocating component planes.
/// Unlike a color decoder, this also accepts raw two-component frames.
pub(crate) fn frame_dimensions(data: &[u8]) -> Result<(usize, usize, usize)> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return Err("not a JPEG stream".into());
    }
    let mut pos = 2;
    while pos < data.len() {
        while pos < data.len() && data[pos] != 0xff {
            pos += 1;
        }
        while pos < data.len() && data[pos] == 0xff {
            pos += 1;
        }
        let Some(&marker) = data.get(pos) else { break };
        pos += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            continue;
        }
        let len = u16::from_be_bytes([*data.get(pos).ok_or("truncated JPEG")?, *data.get(pos + 1).ok_or("truncated JPEG")?]) as usize;
        if len < 2 {
            return Err("invalid JPEG segment length".into());
        }
        let segment = data.get(pos + 2..pos + len).ok_or("truncated JPEG segment")?;
        pos += len;
        if matches!(marker, 0xc0 | 0xc1) {
            if segment.len() < 6 {
                return Err("truncated JPEG frame header".into());
            }
            let height = u16::from_be_bytes([segment[1], segment[2]]) as usize;
            let width = u16::from_be_bytes([segment[3], segment[4]]) as usize;
            return Ok((width, height, segment[5] as usize));
        }
    }
    Err("no baseline JPEG frame header".into())
}

/// Decodes a JPEG stream.
pub(crate) fn decode(data: &[u8], color: Color) -> Result<Image> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return Err("not a JPEG stream".into());
    }
    let mut pos = 2;
    let mut qt = [[0u16; 64]; 4];
    let mut dc: [Option<Huffman>; 4] = Default::default();
    let mut ac: [Option<Huffman>; 4] = Default::default();
    let mut restart_interval = 0usize;
    let mut frame: Option<(usize, usize, Vec<Component>)> = None;
    let (mut jfif, mut adobe) = (false, None::<u8>);
    loop {
        // The next marker.
        while pos < data.len() && data[pos] != 0xff {
            pos += 1;
        }
        while pos < data.len() && data[pos] == 0xff {
            pos += 1;
        }
        let Some(&marker) = data.get(pos) else { break };
        pos += 1;
        if marker == 0xd9 {
            break;
        }
        if (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            continue;
        }
        let len = u16::from_be_bytes([*data.get(pos).ok_or("truncated JPEG")?, *data.get(pos + 1).ok_or("truncated JPEG")?]) as usize;
        let seg = data.get(pos + 2..pos + len).ok_or("truncated JPEG segment")?;
        pos += len;
        match marker {
            0xdb => {
                let mut s = seg;
                while !s.is_empty() {
                    let (pq, tq) = ((s[0] >> 4) as usize, (s[0] & 15) as usize);
                    let size = if pq == 0 { 64 } else { 128 };
                    let body = s.get(1..1 + size).ok_or("truncated quantization table")?;
                    let t = qt.get_mut(tq).ok_or("invalid quantization table")?;
                    for (k, z) in ZIGZAG.iter().enumerate() {
                        t[*z] = if pq == 0 { body[k] as u16 } else { u16::from_be_bytes([body[2 * k], body[2 * k + 1]]) };
                    }
                    s = &s[1 + size..];
                }
            }
            0xc4 => {
                let mut s = seg;
                while s.len() >= 17 {
                    let (class, th) = (s[0] >> 4, (s[0] & 15) as usize);
                    let counts: [u8; 16] = s[1..17].try_into().unwrap_or_default();
                    let n: usize = counts.iter().map(|c| *c as usize).sum();
                    let values = s.get(17..17 + n).ok_or("truncated Huffman table")?.to_vec();
                    let t = Huffman::new(&counts, values)?;
                    let slot = if class == 0 { dc.get_mut(th) } else { ac.get_mut(th) };
                    *slot.ok_or("invalid Huffman table")? = Some(t);
                    s = &s[17 + n..];
                }
            }
            0xdd if seg.len() >= 2 => restart_interval = u16::from_be_bytes([seg[0], seg[1]]) as usize,
            0xe0 if seg.starts_with(b"JFIF\0") => jfif = true,
            0xee if seg.starts_with(b"Adobe") && seg.len() >= 12 => adobe = Some(seg[11]),
            0xc0 | 0xc1 => {
                if frame.is_some() {
                    return Err("multiple JPEG frame headers".into());
                }
                if seg.len() < 6 {
                    return Err("truncated frame header".into());
                }
                if seg[0] != 8 {
                    return Err(format!("{}-bit JPEG is not supported", seg[0]));
                }
                let height = u16::from_be_bytes([seg[1], seg[2]]) as usize;
                let width = u16::from_be_bytes([seg[3], seg[4]]) as usize;
                let n = seg[5] as usize;
                let mut comps = Vec::with_capacity(n);
                for i in 0..n {
                    let c = seg.get(6 + 3 * i..9 + 3 * i).ok_or("truncated frame header")?;
                    let (h, v) = ((c[1] >> 4) as usize, (c[1] & 15) as usize);
                    if !(1..=4).contains(&h) || !(1..=4).contains(&v) || c[2] > 3 {
                        return Err("invalid JPEG component".into());
                    }
                    comps.push(Component { id: c[0], h, v, tq: c[2] as usize, bw: 0, bh: 0, w: 0, hgt: 0, plane: Vec::new() });
                }
                if width == 0 || height == 0 || comps.is_empty() {
                    return Err("empty JPEG image".into());
                }
                if width.saturating_mul(height).saturating_mul(n) > 1 << 31 {
                    return Err(format!("JPEG image too large ({width} x {height} x {n})"));
                }
                let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
                let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
                let (mcux, mcuy) = (width.div_ceil(8 * hmax), height.div_ceil(8 * vmax));
                for c in &mut comps {
                    c.bw = mcux * c.h;
                    c.bh = mcuy * c.v;
                    c.w = (width * c.h).div_ceil(hmax);
                    c.hgt = (height * c.v).div_ceil(vmax);
                    c.plane = vec![0; c.bw * 8 * c.bh * 8];
                }
                frame = Some((width, height, comps));
            }
            0xc2 | 0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                return Err("progressive, lossless and arithmetic-coded JPEG are not supported".into());
            }
            0xda => {
                let (width, height, comps) = frame.as_mut().ok_or("JPEG scan before its frame")?;
                let n = *seg.first().ok_or("truncated scan header")? as usize;
                if seg.len() < 1 + 2 * n {
                    return Err("truncated scan header".into());
                }
                let mut scan = Vec::with_capacity(n);
                for i in 0..n {
                    let (cs, t) = (seg[1 + 2 * i], seg[2 + 2 * i]);
                    let ci = comps.iter().position(|c| c.id == cs).ok_or("JPEG scan of an unknown component")?;
                    scan.push((ci, (t >> 4) as usize, (t & 15) as usize));
                }
                pos = decode_scan(data, pos, comps, &scan, (*width, *height), &qt, &dc, &ac, restart_interval)?;
            }
            _ => {}
        }
    }
    let (width, height, comps) = frame.ok_or("no JPEG frame")?;
    let color = match color {
        Color::Auto => {
            let ids: Vec<u8> = comps.iter().map(|c| c.id).collect();
            let ycc = jfif || adobe.map_or(ids != b"RGB", |t| t != 0);
            if ycc { Color::Rgb } else { Color::Raw }
        }
        c => c,
    };
    Ok(output(width, height, &comps, color))
}

/// Decodes a JPEG file as libjpeg does by default, into `(width, height, components, samples)`:
/// with this decoder, or `jpeg-decoder` for the streams it does not read (progressive ones).
pub(crate) fn decode_file(b: &[u8]) -> std::result::Result<Image, String> {
    decode(b, Color::Auto).or_else(|_| {
        let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(b));
        let data = d.decode().map_err(|e| e.to_string())?;
        let info = d.info().ok_or("no JPEG header")?;
        let (width, height) = (info.width as usize, info.height as usize);
        let components = (data.len() / (width * height).max(1)).clamp(1, 4);
        Ok(Image { width, height, components, data })
    })
}

/// Decodes the entropy-coded segment of a scan starting at `pos`; returns where it ends.
#[allow(clippy::too_many_arguments)]
fn decode_scan(
    data: &[u8],
    pos: usize,
    comps: &mut [Component],
    scan: &[(usize, usize, usize)],
    (width, height): (usize, usize),
    qt: &[[u16; 64]; 4],
    dc: &[Option<Huffman>; 4],
    ac: &[Option<Huffman>; 4],
    restart_interval: usize,
) -> Result<usize> {
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
    let mut bits = Bits::new(data, pos);
    let mut pred = vec![0i32; comps.len()];
    let tables: Vec<(&Huffman, &Huffman)> = scan
        .iter()
        .map(|&(_, td, ta)| Ok((dc[td].as_ref().ok_or("missing DC table")?, ac[ta].as_ref().ok_or("missing AC table")?)))
        .collect::<Result<_>>()?;
    let mut block = [0i32; 64];
    let mut decode_block = |bits: &mut Bits, c: &mut Component, (dct, act): (&Huffman, &Huffman), pred: &mut i32, bx: usize, by: usize| {
        block.fill(0);
        let s = bits.decode(dct)?;
        if s > 11 {
            return Err("invalid JPEG DC coefficient".to_string());
        }
        *pred = pred.wrapping_add(bits.receive_extend(s));
        block[0] = *pred;
        let mut k = 1;
        while k < 64 {
            let rs = bits.decode(act)?;
            let (r, s) = ((rs >> 4) as usize, rs & 15);
            if s == 0 {
                if r != 15 {
                    break;
                }
                k += 16;
                continue;
            }
            k += r;
            if k > 63 {
                return Err("invalid JPEG coefficient".to_string());
            }
            block[ZIGZAG[k]] = bits.receive_extend(s);
            k += 1;
        }
        if bx < c.bw && by < c.bh {
            let stride = c.bw * 8;
            let at = by * 8 * stride + bx * 8;
            idct_islow(&block, &qt[c.tq], &mut c.plane[at..], stride);
        }
        Ok(())
    };
    let single = scan.len() == 1;
    let (units_x, units_y) = if single {
        let c = &comps[scan[0].0];
        // A non-interleaved scan codes the blocks the component's samples cover.
        ((width * c.h).div_ceil(hmax).div_ceil(8), (height * c.v).div_ceil(vmax).div_ceil(8))
    } else {
        (width.div_ceil(8 * hmax), height.div_ceil(8 * vmax))
    };
    let mut done = 0usize;
    for uy in 0..units_y {
        for ux in 0..units_x {
            if restart_interval > 0 && done > 0 && done.is_multiple_of(restart_interval) {
                bits.restart();
                pred.fill(0);
            }
            for (si, &(ci, _, _)) in scan.iter().enumerate() {
                let c = &mut comps[ci];
                if single {
                    decode_block(&mut bits, c, tables[si], &mut pred[ci], ux, uy)?;
                } else {
                    for v in 0..c.v {
                        for h in 0..c.h {
                            decode_block(&mut bits, c, tables[si], &mut pred[ci], ux * c.h + h, uy * c.v + v)?;
                        }
                    }
                }
            }
            done += 1;
        }
    }
    // The end of the segment: the next marker.
    let mut end = bits.pos;
    while end + 1 < data.len() && !(data[end] == 0xff && data[end + 1] != 0 && !(0xd0..=0xd7).contains(&data[end + 1])) {
        end += 1;
    }
    Ok(end)
}

/// libjpeg's slow-but-accurate integer IDCT (`jpeg_idct_islow`), with its output range limiting.
fn idct_islow(coef: &[i32; 64], q: &[u16; 64], out: &mut [u8], stride: usize) {
    const CONST_BITS: i32 = 13;
    const PASS1_BITS: i32 = 2;
    const F_0_298631336: i64 = 2446;
    const F_0_390180644: i64 = 3196;
    const F_0_541196100: i64 = 4433;
    const F_0_765366865: i64 = 6270;
    const F_0_899976223: i64 = 7373;
    const F_1_175875602: i64 = 9633;
    const F_1_501321110: i64 = 12299;
    const F_1_847759065: i64 = 15137;
    const F_1_961570560: i64 = 16069;
    const F_2_053119869: i64 = 16819;
    const F_2_562915447: i64 = 20995;
    const F_3_072711026: i64 = 25172;
    let descale = |x: i64, n: i32| (x + (1i64 << (n - 1))) >> n;
    // The even and odd parts of a one-dimensional IDCT of `x` (0 to 7).
    let butterfly = |x: [i64; 8]| -> [i64; 8] {
        let (z2, z3) = (x[2], x[6]);
        let z1 = (z2 + z3) * F_0_541196100;
        let tmp2 = z1 + z3 * -F_1_847759065;
        let tmp3 = z1 + z2 * F_0_765366865;
        let tmp0 = (x[0] + x[4]) << CONST_BITS;
        let tmp1 = (x[0] - x[4]) << CONST_BITS;
        let (tmp10, tmp13, tmp11, tmp12) = (tmp0 + tmp3, tmp0 - tmp3, tmp1 + tmp2, tmp1 - tmp2);
        let (mut t0, mut t1, mut t2, mut t3) = (x[7], x[5], x[3], x[1]);
        let (z1, z2, z3, z4) = (t0 + t3, t1 + t2, t0 + t2, t1 + t3);
        let z5 = (z3 + z4) * F_1_175875602;
        t0 *= F_0_298631336;
        t1 *= F_2_053119869;
        t2 *= F_3_072711026;
        t3 *= F_1_501321110;
        let (z1, z2) = (z1 * -F_0_899976223, z2 * -F_2_562915447);
        let (z3, z4) = (z3 * -F_1_961570560 + z5, z4 * -F_0_390180644 + z5);
        t0 += z1 + z3;
        t1 += z2 + z4;
        t2 += z2 + z3;
        t3 += z1 + z4;
        [tmp10 + t3, tmp11 + t2, tmp12 + t1, tmp13 + t0, tmp13 - t0, tmp12 - t1, tmp11 - t2, tmp10 - t3]
    };
    let mut ws = [0i64; 64];
    for col in 0..8 {
        let x: [i64; 8] = std::array::from_fn(|r| coef[r * 8 + col] as i64 * q[r * 8 + col] as i64);
        let y = butterfly(x);
        for r in 0..8 {
            ws[r * 8 + col] = descale(y[r], CONST_BITS - PASS1_BITS) as i32 as i64;
        }
    }
    for row in 0..8 {
        let x: [i64; 8] = std::array::from_fn(|c| ws[row * 8 + c]);
        let y = butterfly(x);
        for c in 0..8 {
            out[row * stride + c] = range_limit(descale(y[c], CONST_BITS + PASS1_BITS + 3) as i32);
        }
    }
}

/// libjpeg's post-IDCT range limiting: the sample centred on 128 and clamped, its value taken
/// modulo 1024 first, as the lookup table is indexed.
fn range_limit(v: i32) -> u8 {
    match v & 1023 {
        i @ 0..=127 => (i + 128) as u8,
        128..=511 => 255,
        512..=895 => 0,
        i => (i - 896) as u8,
    }
}

/// Upsamples the components to the image size (libjpeg's fancy upsampling) and converts colours.
fn output(width: usize, height: usize, comps: &[Component], color: Color) -> Image {
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
    let planes: Vec<Vec<u8>> = comps.iter().map(|c| upsample(c, hmax / c.h.max(1), vmax / c.v.max(1), width, height)).collect();
    let n = comps.len();
    let mut data = vec![0u8; width * height * n];
    for (k, p) in planes.iter().enumerate() {
        for (i, v) in p.iter().enumerate() {
            data[i * n + k] = *v;
        }
    }
    if n == 3 && color == Color::Rgb {
        ycc_to_rgb(&mut data);
    }
    Image { width, height, components: n, data }
}

/// A component's samples at the image's size, `width` x `height`: copied, or upsampled by
/// `hs` x `vs` as libjpeg does by default (`jdsample.c`: triangle filters for 2x1, 2x2 and 1x2,
/// replication otherwise), its edge rows repeated beyond the image.
fn upsample(c: &Component, hs: usize, vs: usize, width: usize, height: usize) -> Vec<u8> {
    let stride = c.bw * 8;
    let (w, h) = (c.w, c.hgt);
    // A row of the component, its index clamped to the rows it holds.
    let row = |r: isize| -> &[u8] {
        let r = r.clamp(0, h as isize - 1) as usize;
        &c.plane[r * stride..r * stride + w]
    };
    let mut out = vec![0u8; width * height];
    let mut put_row = |r: usize, vals: &[u8]| {
        if r < height {
            let n = width.min(vals.len());
            out[r * width..r * width + n].copy_from_slice(&vals[..n]);
        }
    };
    let fancy = w > 2;
    match (hs, vs) {
        (1, 1) => (0..h).for_each(|r| put_row(r, row(r as isize))),
        (2, 1) if fancy => {
            for r in 0..h {
                let i = row(r as isize);
                let mut o = Vec::with_capacity(2 * w);
                o.push(i[0]);
                o.push(((i[0] as u32 * 3 + i[1] as u32 + 2) >> 2) as u8);
                for x in 1..w - 1 {
                    let v = i[x] as u32 * 3;
                    o.push(((v + i[x - 1] as u32 + 1) >> 2) as u8);
                    o.push(((v + i[x + 1] as u32 + 2) >> 2) as u8);
                }
                o.push(((i[w - 1] as u32 * 3 + i[w - 2] as u32 + 1) >> 2) as u8);
                o.push(i[w - 1]);
                put_row(r, &o);
            }
        }
        (2, 2) if fancy => {
            for r in 0..h {
                for v in 0..2 {
                    let i0 = row(r as isize);
                    let i1 = row(if v == 0 { r as isize - 1 } else { r as isize + 1 });
                    let sum = |x: usize| i0[x] as u32 * 3 + i1[x] as u32;
                    let mut o = Vec::with_capacity(2 * w);
                    let first = sum(0);
                    let (mut last, mut this) = (first, sum(1));
                    o.push(((first * 4 + 8) >> 4) as u8);
                    o.push(((first * 3 + this + 7) >> 4) as u8);
                    for x in 2..w {
                        let next = sum(x);
                        o.push(((this * 3 + last + 8) >> 4) as u8);
                        o.push(((this * 3 + next + 7) >> 4) as u8);
                        (last, this) = (this, next);
                    }
                    o.push(((this * 3 + last + 8) >> 4) as u8);
                    o.push(((this * 4 + 7) >> 4) as u8);
                    put_row(2 * r + v, &o);
                }
            }
        }
        (1, 2) => {
            for r in 0..h {
                for v in 0..2 {
                    let i0 = row(r as isize);
                    let i1 = row(if v == 0 { r as isize - 1 } else { r as isize + 1 });
                    let bias = if v == 0 { 1 } else { 2 };
                    let o: Vec<u8> = (0..w).map(|x| ((i0[x] as u32 * 3 + i1[x] as u32 + bias) >> 2) as u8).collect();
                    put_row(2 * r + v, &o);
                }
            }
        }
        _ => {
            // Replication.
            for r in 0..h {
                let i = row(r as isize);
                let o: Vec<u8> = i.iter().flat_map(|v| std::iter::repeat_n(*v, hs)).collect();
                for k in 0..vs {
                    put_row(r * vs + k, &o);
                }
            }
        }
    }
    out
}

/// Converts YCbCr samples to RGB in place, with libjpeg's fixed-point tables (`jdcolor.c`).
pub(crate) fn ycc_to_rgb(v: &mut [u8]) {
    const SCALEBITS: i32 = 16;
    const ONE_HALF: i32 = 1 << (SCALEBITS - 1);
    let fix = |x: f64| (x * (1i64 << SCALEBITS) as f64 + 0.5) as i32;
    let (cr_r, cb_b, cr_g, cb_g) = (fix(1.40200), fix(1.77200), fix(0.71414), fix(0.34414));
    for px in v.as_chunks_mut::<3>().0 {
        let y = px[0] as i32;
        let (cb, cr) = (px[1] as i32 - 128, px[2] as i32 - 128);
        let r = y + ((cr_r * cr + ONE_HALF) >> SCALEBITS);
        let g = y + ((-cb_g * cb + ONE_HALF - cr_g * cr) >> SCALEBITS);
        let b = y + ((cb_b * cb + ONE_HALF) >> SCALEBITS);
        px[0] = r.clamp(0, 255) as u8;
        px[1] = g.clamp(0, 255) as u8;
        px[2] = b.clamp(0, 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_frame_cannot_bypass_header_preflight() {
        let sof = |size: u16| {
            let mut raw = vec![0xff, 0xc0, 0, 11, 8];
            raw.extend_from_slice(&size.to_be_bytes());
            raw.extend_from_slice(&size.to_be_bytes());
            raw.extend_from_slice(&[1, 1, 0x11, 0]);
            raw
        };
        let stream = [vec![0xff, 0xd8], sof(1), sof(65535), vec![0xff, 0xd9]].concat();
        assert_eq!(frame_dimensions(&stream).unwrap(), (1, 1, 1));
        assert!(matches!(decode(&stream, Color::Raw), Err(e) if e == "multiple JPEG frame headers"));
    }

    #[test]
    fn ycc_converts_as_libjpeg() {
        // Grey stays grey; saturated chroma, worked out with libjpeg's tables.
        let mut v = [128, 128, 128, 0, 0, 255, 255, 255, 0];
        ycc_to_rgb(&mut v);
        assert_eq!(v, [128, 128, 128, 178, 0, 0, 76, 255, 255]);
    }

    #[test]
    fn range_limit_follows_libjpegs_table() {
        assert_eq!([-129, -128, 0, 127, 128, 511, 512].map(range_limit), [0, 0, 128, 255, 255, 255, 0]);
    }

    #[test]
    fn decodes_subsampled_and_restarted_streams() {
        // Images coded by another encoder, decoded within the rounding differences of another
        // decoder: every chroma subsampling libjpeg upsamples, with and without restart markers.
        let (w, h) = (37, 29);
        let rgb: Vec<u8> = (0..w * h).flat_map(|i| [(i * 7 % 256) as u8, (i / w * 8) as u8, (255 - i % w * 6) as u8]).collect();
        use jpeg_encoder::SamplingFactor as F;
        for (sampling, restart) in [(F::F_1_1, 0), (F::F_2_1, 3), (F::F_1_2, 0), (F::F_2_2, 1), (F::F_2_2, 0)] {
            let mut bytes = Vec::new();
            let mut enc = jpeg_encoder::Encoder::new(&mut bytes, 90);
            enc.set_sampling_factor(sampling);
            if restart > 0 {
                enc.set_restart_interval(restart);
            }
            enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).unwrap();
            let ours = decode(&bytes, Color::Auto).unwrap();
            assert_eq!((ours.width, ours.height, ours.components), (w, h, 3));
            let mut other = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
            let theirs = other.decode().unwrap();
            let max = ours.data.iter().zip(&theirs).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(max <= 4, "{sampling:?}, restart {restart}: differs by {max}");
        }
        // Grey.
        let grey: Vec<u8> = (0..w * h).map(|i| (i * 3 % 256) as u8).collect();
        let mut bytes = Vec::new();
        jpeg_encoder::Encoder::new(&mut bytes, 75).encode(&grey, w as u16, h as u16, jpeg_encoder::ColorType::Luma).unwrap();
        let ours = decode_file(&bytes).unwrap();
        assert_eq!((ours.width, ours.height, ours.components), (w, h, 1));
    }

    #[test]
    fn dc_only_block_is_flat() {
        let mut coef = [0i32; 64];
        coef[0] = 80; // with a quantizer of 1: 80 / 8 = 10 above the centre
        let mut out = [0u8; 64];
        idct_islow(&coef, &[1; 64], &mut out, 8);
        assert!(out.iter().all(|v| *v == 138));
    }
}
