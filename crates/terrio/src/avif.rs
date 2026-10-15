//! AVIF images (AV1 Image File Format), decoded as GDAL's AVIF driver decodes them with libavif:
//! the HEIF container read for the primary image and its alpha plane, the AV1 bitstream decoded
//! by `rusty_av1d` (a pure-Rust port of dav1d), and YUV converted to RGB as libavif's built-in
//! converter does (bilinear chroma upsampling, the matrix coefficients and range of the `colr`
//! box or else of the sequence header). 8-bit images, with or without alpha, gray or color.

use crate::{Error, Result};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("AVIF: {msg}"))
}

/// Whether bytes start like an AVIF file: an `ftyp` box naming the `avif` brand.
pub(crate) fn is_avif(b: &[u8]) -> bool {
    if b.len() < 16 || &b[4..8] != b"ftyp" {
        return false;
    }
    let size = (u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).min(b.len());
    // The major brand, then the compatible brands after the minor version.
    std::iter::once(&b[8..12])
        .chain(b.get(16..size).unwrap_or_default().as_chunks::<4>().0.iter().map(|c| &c[..]))
        .any(|brand| brand == b"avif" || brand == b"avis")
}

/// A decoded image: width, height, channels (1 gray, 2 gray and alpha, 3 RGB, 4 RGBA) and 8-bit
/// samples, interleaved.
pub(crate) struct Image {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub data: Vec<u8>,
}

/// An ISOBMFF box: its type and payload.
struct Box<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

/// The boxes of a byte range.
fn boxes(mut b: &[u8]) -> Result<Vec<Box<'_>>> {
    let mut out = Vec::new();
    while b.len() >= 8 {
        let size32 = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
        let kind = [b[4], b[5], b[6], b[7]];
        let (header, size) = match size32 {
            0 => (8, b.len() as u64),
            1 => {
                let s = b.get(8..16).ok_or_else(|| bad("truncated box"))?;
                (16, u64::from_be_bytes(s.try_into().map_err(|_| bad("box size"))?))
            }
            s => (8, s),
        };
        let size = usize::try_from(size).map_err(|_| bad("box size"))?;
        if size < header || size > b.len() {
            return Err(bad("box size"));
        }
        out.push(Box { kind, body: &b[header..size] });
        b = &b[size..];
    }
    Ok(out)
}

fn find<'a, 'b>(list: &'b [Box<'a>], kind: &[u8; 4]) -> Option<&'b Box<'a>> {
    list.iter().find(|x| &x.kind == kind)
}

/// A big-endian reader.
struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos + n).ok_or_else(|| bad("truncated box"))?;
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let s = self.bytes(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.bytes(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// An unsigned integer of 0, 4 or 8 bytes (iloc's sizes).
    fn uint(&mut self, size: u8) -> Result<u64> {
        Ok(match size {
            0 => 0,
            4 => self.u32()? as u64,
            8 => {
                let s = self.bytes(8)?;
                u64::from_be_bytes(s.try_into().map_err(|_| bad("iloc"))?)
            }
            _ => return Err(bad("iloc field size")),
        })
    }

    /// A NUL-terminated string.
    fn cstr(&mut self) -> Result<&'a [u8]> {
        let rest = &self.b[self.pos.min(self.b.len())..];
        let n = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        self.pos += (n + 1).min(rest.len());
        Ok(&rest[..n])
    }
}

/// The colour description of an `nclx` `colr` box: matrix coefficients and full range.
#[derive(Clone, Copy)]
struct Nclx {
    matrix: u16,
    full_range: bool,
}

/// An item of the file: its type, the extents of its data, and its properties.
#[derive(Default)]
struct Item {
    id: u32,
    kind: [u8; 4],
    /// (construction method, extents `(offset, length)`).
    location: Option<(u8, Vec<(u64, u64)>)>,
    properties: Vec<usize>,
}

/// The AV1 bitstreams of an AVIF file's primary image and of its alpha plane, and the colour
/// description of the primary image.
/// An AVIF file's contents: the primary image's AV1 bitstream, its alpha plane's, its colours.
type Container = (Vec<u8>, Option<Vec<u8>>, Option<Nclx>);

fn read_container(b: &[u8]) -> Result<Container> {
    let top = boxes(b)?;
    let meta = find(&top, b"meta").ok_or_else(|| bad("no meta box"))?;
    let meta_boxes = boxes(meta.body.get(4..).ok_or_else(|| bad("meta"))?)?;
    let mut primary = None;
    if let Some(p) = find(&meta_boxes, b"pitm") {
        let mut r = Reader::new(p.body);
        let version = r.u8()?;
        r.bytes(3)?;
        primary = Some(if version == 0 { r.u16()? as u32 } else { r.u32()? });
    }
    let primary = primary.ok_or_else(|| bad("no primary item"))?;
    let mut items: Vec<Item> = Vec::new();
    let item = |items: &mut Vec<Item>, id: u32| -> usize {
        match items.iter().position(|i| i.id == id) {
            Some(i) => i,
            None => {
                items.push(Item { id, ..Default::default() });
                items.len() - 1
            }
        }
    };
    // Item types.
    if let Some(iinf) = find(&meta_boxes, b"iinf") {
        let mut r = Reader::new(iinf.body);
        let version = r.u8()?;
        r.bytes(3)?;
        let _count = if version == 0 { r.u16()? as u32 } else { r.u32()? };
        for infe in boxes(&iinf.body[r.pos..])?.iter().filter(|x| &x.kind == b"infe") {
            let mut r = Reader::new(infe.body);
            let version = r.u8()?;
            r.bytes(3)?;
            if version < 2 {
                continue;
            }
            let id = if version == 2 { r.u16()? as u32 } else { r.u32()? };
            r.u16()?; // protection index
            let kind: [u8; 4] = r.bytes(4)?.try_into().map_err(|_| bad("infe"))?;
            let i = item(&mut items, id);
            items[i].kind = kind;
        }
    }
    // Item locations.
    let iloc = find(&meta_boxes, b"iloc").ok_or_else(|| bad("no iloc box"))?;
    let mut r = Reader::new(iloc.body);
    let version = r.u8()?;
    r.bytes(3)?;
    let sizes = r.u16()?;
    let (offset_size, length_size, base_size) = ((sizes >> 12) as u8, ((sizes >> 8) & 15) as u8, ((sizes >> 4) & 15) as u8);
    let index_size = if version >= 1 { (sizes & 15) as u8 } else { 0 };
    let count = if version < 2 { r.u16()? as u32 } else { r.u32()? };
    for _ in 0..count {
        let id = if version < 2 { r.u16()? as u32 } else { r.u32()? };
        let method = if version >= 1 { (r.u16()? & 15) as u8 } else { 0 };
        r.u16()?; // data reference index
        let base = r.uint(base_size)?;
        let n = r.u16()?;
        let mut extents = Vec::with_capacity(n as usize);
        for _ in 0..n {
            r.uint(index_size)?;
            let offset = r.uint(offset_size)?;
            let length = r.uint(length_size)?;
            extents.push((base + offset, length));
        }
        let i = item(&mut items, id);
        items[i].location = Some((method, extents));
    }
    // Properties and their associations.
    let mut properties: Vec<Box> = Vec::new();
    if let Some(iprp) = find(&meta_boxes, b"iprp") {
        let inner = boxes(iprp.body)?;
        if let Some(ipco) = find(&inner, b"ipco") {
            properties = boxes(ipco.body)?;
        }
        for ipma in inner.iter().filter(|x| &x.kind == b"ipma") {
            let mut r = Reader::new(ipma.body);
            let version = r.u8()?;
            let flags = r.bytes(3)?[2];
            let count = r.u32()?;
            for _ in 0..count {
                let id = if version < 1 { r.u16()? as u32 } else { r.u32()? };
                let n = r.u8()?;
                let i = item(&mut items, id);
                for _ in 0..n {
                    let index = if flags & 1 == 1 { (r.u16()? & 0x7fff) as usize } else { (r.u8()? & 0x7f) as usize };
                    if index > 0 {
                        items[i].properties.push(index - 1);
                    }
                }
            }
        }
    }
    // The alpha plane: an item referring to the primary one with `auxl`, whose `auxC` names alpha.
    let mut alpha_id = None;
    if let Some(iref) = find(&meta_boxes, b"iref") {
        let mut r = Reader::new(iref.body);
        let version = r.u8()?;
        r.bytes(3)?;
        for reference in boxes(&iref.body[r.pos..])? {
            if &reference.kind != b"auxl" {
                continue;
            }
            let mut r = Reader::new(reference.body);
            let from = if version == 0 { r.u16()? as u32 } else { r.u32()? };
            let n = r.u16()?;
            let mut to = Vec::new();
            for _ in 0..n {
                to.push(if version == 0 { r.u16()? as u32 } else { r.u32()? });
            }
            if !to.contains(&primary) {
                continue;
            }
            let is_alpha = items.iter().find(|i| i.id == from).is_some_and(|i| {
                i.properties.iter().filter_map(|&p| properties.get(p)).any(|p| {
                    &p.kind == b"auxC" && {
                        let mut r = Reader::new(p.body);
                        r.bytes(4).is_ok()
                            && r.cstr().is_ok_and(|urn| {
                                urn == b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha" || urn == b"urn:mpeg:hevc:2015:auxid:1"
                            })
                    }
                })
            });
            if is_alpha {
                alpha_id = Some(from);
            }
        }
    }
    let idat = find(&meta_boxes, b"idat").map(|x| x.body);
    let data = |id: u32| -> Result<Vec<u8>> {
        let it = items.iter().find(|i| i.id == id).ok_or_else(|| bad(format!("no item {id}")))?;
        if &it.kind == b"grid" {
            return Err(Error::Unsupported("AVIF: grid images are not supported".into()));
        }
        if &it.kind != b"av01" {
            return Err(bad(format!("item {id} is not an AV1 image")));
        }
        let (method, extents) = it.location.as_ref().ok_or_else(|| bad(format!("item {id} has no location")))?;
        let source = match method {
            0 => b,
            1 => idat.ok_or_else(|| bad("no idat box"))?,
            _ => return Err(Error::Unsupported("AVIF: item data in other items".into())),
        };
        let mut out = Vec::new();
        for &(offset, length) in extents {
            let (o, l) = (usize::try_from(offset).map_err(|_| bad("offset"))?, usize::try_from(length).map_err(|_| bad("length"))?);
            let end = if l == 0 { source.len() } else { o.checked_add(l).ok_or_else(|| bad("extent"))? };
            out.extend_from_slice(source.get(o..end).ok_or_else(|| bad("item data out of range"))?);
        }
        Ok(out)
    };
    let nclx = items.iter().find(|i| i.id == primary).and_then(|it| {
        it.properties.iter().filter_map(|&p| properties.get(p)).find_map(|p| {
            if &p.kind != b"colr" || p.body.get(..4) != Some(b"nclx") {
                return None;
            }
            let mut r = Reader::new(&p.body[4..]);
            let (_primaries, _transfer, matrix) = (r.u16().ok()?, r.u16().ok()?, r.u16().ok()?);
            Some(Nclx { matrix, full_range: r.u8().ok()? & 0x80 != 0 })
        })
    });
    Ok((data(primary)?, alpha_id.map(data).transpose()?, nclx))
}

/// A decoded AV1 frame: its planes (Y, U, V), their strides, the chroma subsampling shifts, and
/// the matrix coefficients and range of its sequence header.
struct Frame {
    width: usize,
    height: usize,
    planes: [Vec<u8>; 3],
    strides: [usize; 3],
    /// `None` for monochrome frames (4:0:0).
    chroma_shift: Option<(usize, usize)>,
    matrix: u16,
    full_range: bool,
}

/// Decodes an AV1 bitstream (one frame).
fn decode_av1(obus: Vec<u8>) -> Result<Frame> {
    use rusty_av1d::rust_api::{Decoder, PixelLayout, PlanarImageComponent, Settings};
    let err = |e: &dyn std::fmt::Debug| bad(format!("AV1 decoding failed: {e:?}"));
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    let mut decoder = Decoder::with_settings(&settings).map_err(|e| err(&e))?;
    match decoder.send_data(obus.into_boxed_slice(), None, None, None) {
        Ok(()) => {}
        Err(rusty_av1d::Rav1dError::TryAgain) => {}
        Err(e) => return Err(err(&e)),
    }
    let picture = match decoder.get_picture() {
        Ok(p) => p,
        Err(rusty_av1d::Rav1dError::TryAgain) => {
            if decoder.send_pending_data().is_err() {
                decoder.flush();
            }
            match decoder.get_picture() {
                Ok(p) => p,
                Err(rusty_av1d::Rav1dError::TryAgain) => return Err(bad("no frame decoded")),
                Err(e) => return Err(err(&e)),
            }
        }
        Err(e) => return Err(err(&e)),
    };
    if picture.bit_depth() != 8 {
        return Err(Error::Unsupported(format!("AVIF: {}-bit images are not supported (8-bit only)", picture.bit_depth())));
    }
    let (width, height) = (picture.width() as usize, picture.height() as usize);
    let chroma_shift = match picture.pixel_layout() {
        PixelLayout::I400 => None,
        PixelLayout::I420 => Some((1, 1)),
        PixelLayout::I422 => Some((1, 0)),
        PixelLayout::I444 => Some((0, 0)),
    };
    let components = [PlanarImageComponent::Y, PlanarImageComponent::U, PlanarImageComponent::V];
    let mut planes: [Vec<u8>; 3] = Default::default();
    let mut strides = [0usize; 3];
    let n = if chroma_shift.is_some() { 3 } else { 1 };
    for (k, c) in components.into_iter().enumerate().take(n) {
        planes[k] = picture.plane(c).to_vec();
        strides[k] = picture.stride(c) as usize;
    }
    let matrix = picture.matrix_coefficients() as u16;
    let full_range = matches!(picture.color_range(), rusty_av1d::rust_api::pixel::YUVRange::Full);
    Ok(Frame { width, height, planes, strides, chroma_shift, matrix, full_range })
}

/// libavif's `kr` and `kb` for matrix coefficients (BT.601's when unspecified).
fn coefficients(matrix: u16) -> (f32, f32) {
    match matrix {
        1 => (0.2126, 0.0722),
        4 => (0.30, 0.11),
        5 | 6 => (0.299, 0.114),
        7 => (0.212, 0.087),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.299, 0.114),
    }
}

/// Converts a frame to RGB (or gray) as libavif's built-in converter does, with an alpha plane.
fn to_rgb(frame: &Frame, alpha: Option<&Frame>, nclx: Option<Nclx>) -> Result<Image> {
    let (w, h) = (frame.width, frame.height);
    let matrix = nclx.map_or(frame.matrix, |c| c.matrix);
    let full = nclx.map_or(frame.full_range, |c| c.full_range);
    // The unorm tables of libavif: Y and UV values to [0, 1] and [-0.5, 0.5].
    let (bias_y, range_y, range_uv) = if full { (0.0f32, 255.0f32, 255.0f32) } else { (16.0, 219.0, 224.0) };
    let table_y: Vec<f32> = (0..256).map(|v| (v as f32 - bias_y) / range_y).collect();
    let table_uv: Vec<f32> = (0..256).map(|v| (v as f32 - 128.0) / range_uv).collect();
    let (kr, kb) = coefficients(matrix);
    let kg = 1.0 - kr - kb;
    let gray = frame.chroma_shift.is_none();
    let channels = match (gray, alpha.is_some()) {
        (true, false) => 1,
        (true, true) => 2,
        (false, false) => 3,
        (false, true) => 4,
    };
    let mut data = vec![0u8; w * h * channels];
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5).floor() as u8;
    let y_at = |x: usize, yy: usize| frame.planes[0][yy * frame.strides[0] + x];
    for j in 0..h {
        for i in 0..w {
            let y = table_y[y_at(i, j) as usize];
            let o = (j * w + i) * channels;
            if let Some((sx, sy)) = frame.chroma_shift {
                // Bilinear chroma upsampling with libavif's weights (9, 3, 3, 1).
                let (ci, cj) = (i >> sx, j >> sy);
                let adj = |k: usize, shift: usize, size: usize| -> isize {
                    if shift == 0 || k == 0 || (k == size - 1 && !k.is_multiple_of(2)) {
                        0
                    } else if !k.is_multiple_of(2) {
                        1
                    } else {
                        -1
                    }
                };
                let (ai, aj) = (adj(i, sx, w), adj(j, sy, h));
                let (ci2, cj2) = ((ci as isize + ai) as usize, (cj as isize + aj) as usize);
                // Identity (GBR) samples are all read as luma.
                let table = if matrix == 0 { &table_y } else { &table_uv };
                let sample = |p: usize, x: usize, y: usize| table[frame.planes[p][y * frame.strides[p] + x] as usize];
                let filter = |p: usize| {
                    sample(p, ci, cj) * (9.0 / 16.0)
                        + sample(p, ci2, cj) * (3.0 / 16.0)
                        + sample(p, ci, cj2) * (3.0 / 16.0)
                        + sample(p, ci2, cj2) * (1.0 / 16.0)
                };
                let (cb, cr) = (filter(1), filter(2));
                let (r, g, b) = if matrix == 0 {
                    // Identity: GBR.
                    (cr, y, cb)
                } else {
                    let r = y + (2.0 * (1.0 - kr)) * cr;
                    let b = y + (2.0 * (1.0 - kb)) * cb;
                    let g = y - ((2.0 * ((kr * (1.0 - kr) * cr) + (kb * (1.0 - kb) * cb))) / kg);
                    (r, g, b)
                };
                data[o] = to_u8(r);
                data[o + 1] = to_u8(g);
                data[o + 2] = to_u8(b);
            } else {
                data[o] = to_u8(y);
            }
            if let Some(a) = alpha {
                let v = a.planes[0][j.min(a.height - 1) * a.strides[0] + i.min(a.width - 1)];
                data[o + channels - 1] = if a.full_range { v } else { to_u8((v as f32 - 16.0) / 219.0) };
            }
        }
    }
    Ok(Image { width: w, height: h, channels, data })
}

/// Decodes an AVIF image to 8-bit samples (gray, gray and alpha, RGB or RGBA).
pub(crate) fn decode(bytes: &[u8]) -> Result<Image> {
    let (color, alpha, nclx) = read_container(bytes)?;
    let frame = decode_av1(color)?;
    let alpha = alpha.map(decode_av1).transpose()?;
    to_rgb(&frame, alpha.as_ref(), nclx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32 x 24 images written by GDAL's AVIF driver (libavif with aom), as hex.
    const RGBA420: &str = concat!(
        "00000020667479706176696600000000617669666d6966316d6961664d413142000001866d6574610000000000000021",
        "68646c72000000000000000070696374000000000000000000000000000000000e7069746d0000000000010000002c69",
        "6c6f6300000000440000020001000000010000041600000095000200000001000001ae000002680000004269696e6600",
        "00000000020000001a696e6665020000000001000061763031436f6c6f72000000001a696e6665020000000002000061",
        "763031416c706861000000001a69726566000000000000000e6175786c000200010001000000c3697072700000009d69",
        "70636f0000001469737065000000000000002000000018000000107069786900000000030808080000000c6176314381",
        "000c0000000013636f6c726e636c780001000d0006800000000e706978690000000001080000000c6176314381001c00",
        "00000038617578430000000075726e3a6d7065673a6d706567423a636963703a73797374656d733a617578696c696172",
        "793a616c706861000000001e69706d6100000000000000020001040102830400020401058607000003056d6461741200",
        "0a0518113f761532dc041000d14641ac26805dfdfa29fe1266a2f0c1afe450a327fdf60b5749458c5efdffffd36c16f7",
        "5b91d8789947c2ee46e210bb79b91d91b79b91f1ecd3e393febbe9d022889df86d3fe500672890cd38d6e35e878f50c7",
        "bdada46f1e7d73cd207f71e6868827c1b1d4b47a6f8e5cf4cbb756cb7790ac7d27ba9efce36d0a17b648eda27008f40c",
        "074f1be27c8ba9a55b7c43f9d675640e873cba3b2084e9a7cc827d45f5ed89618dfd43eacc3129313a8c50c823aab7ed",
        "3d8f9a7dc7215f04dba7ee3a8c0bb48886958ed1a401308853ee933961057803f2a436f30cbe400299c87ba2686672fd",
        "bf6585cfb4187a7bc233b34dd9525ce891aa46996a2872085be37d1c7b1b17e383ee23586fd318dd95ae959321c37635",
        "94c0d28dd95d8d28dd8e171f9163f72569500a6a317817128417b58305b427309e05107e28e1f2dc8c4053639e503138",
        "a4b1d9712bfac85ac37a0b015b6f3d1e6943d25e0f4bcf775a9c1332737dba7d6a315c7139d7933b53ef67e5dab39fd0",
        "18a075469266ad7a426df7837fbb783ee0a6cd953027121cd2254f809c071c40150271270150272262812a61482bdcf4",
        "7dcb122d6420461c1657636e0d5a78c43c77729dcdec6fb27afe7ab9255a5d446df6d37c9edadbb906caa9561738c6f8",
        "3598b976e364022960c56ea7e5f555f85d4581145d530a8ac5ec97b6b5373ba75c87e8cd87ba5e2c5e22bc8f20f9391f",
        "0d55da5925b3c70f1bdaed0aceae6a1785f7afe2adb0228d3bb323907d432ff06c9359a65f25bac274de051386629d8f",
        "f4eb552d5b81dcd119f42ced97050c6737e1d1a3832c936fd9917637b8cad6e9290375d68e9a12000a0918113f768808",
        "6834203285011947878621879e79e68500008b326bb5fafa3b176640ab3c2de2927622d8cc8708e8a76267ab3d2c6838",
        "834b4b38e5ab4520bd3df14655bb5447cc17a5062ecc8c149b9c790cb7473d0c11f2d5c85856b6476607795fbf928588",
        "fa7dca6b1c63069673fc3f7042ac8d285b6b24af976457bb8b23fbb88367813fbe737731919b87f7c280b0",
    );
    const RGB444: &str = concat!(
        "00000020667479706176696600000000617669666d6966316d6961664d413141000000eb6d6574610000000000000021",
        "68646c72000000000000000070696374000000000000000000000000000000000e7069746d0000000000010000001e69",
        "6c6f63000000004400000100010000000100000113000001980000002869696e660000000000010000001a696e666502",
        "0000000001000061763031436f6c6f72000000006a697072700000004b6970636f000000146973706500000000000000",
        "2000000018000000107069786900000000030808080000000c617631438120000000000013636f6c726e636c78000100",
        "0d0006800000001769706d61000000000000000100010401028304000001a06d64617412000a0838113f769010d06932",
        "890315c26324a145145f2a8a8000cfb119f53c8cf0261d17e7c55e9bb9c2d745163191752c00843d7d6b2c2a0df57456",
        "2e8709d5231ba4b9d8ffc836d5cbca9440089cb4cd1e285f4dcd2bcf410f169d33b97bcd79f212dd9d192a3d340f430d",
        "cad614140d15a5dfd30bdbad3a8dea45a8b9cb2d82d430b6b9e7717d1bba3b4832a57bd90aafe8788c434ad2b5a2d4eb",
        "39f2266b568ce9dd6a3f678e6d8c1e01d9ac85aca64d157ebea5f321e7464ed56bc436ba3e726c6a112c471e5cfdedff",
        "4465cf7cf31cf0b3e9d3d512046a72c6316d2880abf12ed47d79f977e452ea10e3e8d55df47f6f345e25bfcf99860d12",
        "ea37a86c65826c3f33530e70a60d0a97d17da3ad1d3f05e6978d19570a638f46eae3161216e00838354fc67f58522c26",
        "3537dd3f40a8f122e864972200f21fb596a928e9999d9ae24aa9f81caca3b55c9ae5000419031de4c813a5df6a9abcae",
        "fe6fbe2389ab9bccab05d4f2abc6ee1a30a8d6a6edc3fdeeace4894d8c4bcb870ee3743c72fc0e331012fb8cde30305b",
        "a5e8abd931deddc034e59c",
    );
    const GRAY: &str = concat!(
        "0000001c667479706176696600000000617669666d6966316d696166000000e96d657461000000000000002168646c72",
        "000000000000000070696374000000000000000000000000000000000e7069746d0000000000010000001e696c6f6300",
        "000000440000010001000000010000010d000000270000002869696e660000000000010000001a696e66650200000000",
        "01000061763031436f6c6f72000000006869707270000000496970636f00000014697370650000000000000020000000",
        "180000000e706978690000000001080000000c6176314381001c0000000013636f6c726e636c780001000d0006800000",
        "001769706d610000000000000001000104010283040000002f6d64617412000a0818113f769808683532191786218492",
        "65000045ca1d96e412c1e48de0d667a5011eece0",
    );

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    /// Per band: the sum of the samples, and their sum weighted by the pixel index.
    fn sums(img: &Image) -> Vec<(u64, u64)> {
        (0..img.channels)
            .map(|b| {
                let v = img.data.iter().skip(b).step_by(img.channels);
                v.enumerate().fold((0, 0), |(s, w), (i, &x)| (s + x as u64, w + i as u64 * x as u64))
            })
            .collect()
    }

    #[test]
    fn decodes_as_gdal_does() {
        // Sums of the pixels GDAL 3.12's AVIF driver reads (libavif 1.4, dav1d).
        type Case<'a> = (&'a str, usize, &'a [(u64, u64)]);
        let cases: [Case; 3] = [
            (RGBA420, 4, &[(95748, 37305268), (88177, 45433556), (98142, 37371111), (96000, 36847360)]),
            (RGB444, 3, &[(94936, 36970569), (88265, 45601450), (97898, 37371217)]),
            (GRAY, 1, &[(95208, 37036848)]),
        ];
        for (hex, channels, expected) in cases {
            let b = bytes(hex);
            assert!(is_avif(&b));
            let img = decode(&b).unwrap();
            assert_eq!((img.width, img.height, img.channels), (32, 24, channels));
            assert_eq!(sums(&img), expected);
        }
        assert!(!is_avif(b"\x89PNG\r\n\x1a\n0000000000"));
        assert!(decode(&bytes(RGB444)[..300]).is_err());
    }
}
