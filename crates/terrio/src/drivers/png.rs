//! PNG rasters, read and written as GDAL does: 8- or 16-bit, 1 to 4 bands (gray, gray and
//! alpha, RGB, RGBA; palette indices as one band, with GDAL's color table), a world file for the
//! georeferencing, the CRS in a `.aux.xml` sidecar, and a single-value `tRNS` chunk as the nodata
//! value.

use super::common;
use crate::dataset::fallible;
use crate::formats::RasterDriver;
use crate::{BandMetadata, Error, RasterDataset, Result};
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster};

fn png_err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("{}: {e}", path.display()))
}

/// Reads a PNG.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info().map_err(|e| png_err(path, e))?;
    let info = reader.info().clone();
    let (cols, rows) = (info.width as usize, info.height as usize);
    let bands = match info.color_type {
        png::ColorType::Grayscale | png::ColorType::Indexed => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
    };
    let bits = info.bit_depth as usize;
    // The sizes come from the header: allocated fallibly.
    let size = reader.output_buffer_size().ok_or_else(|| png_err(path, "image too large"))?;
    let mut buf = fallible::filled(size, 0u8, "the PNG image")?;
    let frame = reader.next_frame(&mut buf).map_err(|e| png_err(path, e))?;
    let line = frame.line_size;
    let n = cols * rows;
    let len = fallible::len(&[n, bands], "the PNG raster")?;
    let data = if bits == 16 {
        let mut v = fallible::filled(len, 0u16, "the PNG raster")?;
        for r in 0..rows {
            for c in 0..cols {
                for b in 0..bands {
                    let i = r * line + (c * bands + b) * 2;
                    v[b * n + r * cols + c] = u16::from_be_bytes([buf[i], buf[i + 1]]);
                }
            }
        }
        RasterData::U16(v)
    } else {
        let mut v = fallible::filled(len, 0u8, "the PNG raster")?;
        for r in 0..rows {
            for c in 0..cols {
                for b in 0..bands {
                    let bit = (c * bands + b) * bits;
                    let byte = buf[r * line + bit / 8];
                    v[b * n + r * cols + c] = if bits == 8 { byte } else { (byte >> (8 - bits - bit % 8)) & ((1 << bits) - 1) as u8 };
                }
            }
        }
        RasterData::U8(v)
    };
    // A grayscale tRNS chunk holds the one transparent (nodata) value.
    let nodata = match (&info.trns, info.color_type) {
        (Some(t), png::ColorType::Grayscale) if t.len() >= 2 => Some(u16::from_be_bytes([t[0], t[1]]) as f64),
        // The decoder keeps one byte for 8-bit images.
        (Some(t), png::ColorType::Grayscale) if t.len() == 1 => Some(t[0] as f64),
        _ => None,
    };
    // A palette is GDAL's color table: each entry's alpha from the tRNS chunk, 255 past its end;
    // the index of the only fully transparent entry, if just one is, is the nodata value
    // (`PNGDataset::Open`).
    let color_table: Vec<[u8; 4]> = match (&info.palette, info.color_type) {
        (Some(p), png::ColorType::Indexed) => {
            let alpha = |i: usize| info.trns.as_deref().and_then(|t| t.get(i)).copied().unwrap_or(255);
            p.as_chunks::<3>().0.iter().enumerate().map(|(i, c)| [c[0], c[1], c[2], alpha(i)]).collect()
        }
        _ => Vec::new(),
    };
    let nodata = nodata.or_else(|| {
        let mut transparent = color_table.iter().enumerate().filter(|(_, c)| c[3] == 0).map(|(i, _)| i as f64);
        match (transparent.next(), transparent.next()) {
            (Some(i), None) => Some(i),
            _ => None,
        }
    });
    let gt = read_world_file(path).unwrap_or(common::NO_GEOTRANSFORM);
    let raster = common::make_raster(cols, rows, bands, data, gt, Default::default(), nodata)?;
    let mut ds = RasterDataset::with_driver(raster, RasterDriver::PNG, nodata.is_some());
    // GDAL's colour interpretations (`PNGRasterBand::GetColorInterpretation`); gray, a single
    // band's default, is left unset.
    let interps: &[&str] = match info.color_type {
        png::ColorType::Grayscale => &[],
        png::ColorType::GrayscaleAlpha => &["Gray", "Alpha"],
        png::ColorType::Rgb => &["Red", "Green", "Blue"],
        png::ColorType::Rgba => &["Red", "Green", "Blue", "Alpha"],
        png::ColorType::Indexed => &["Palette"],
    };
    ds.band_metadata = interps.iter().map(|c| BandMetadata { color_interp: Some(c.to_string()), ..Default::default() }).collect();
    ds.color_table = color_table;
    Ok(ds)
}

/// The geotransform of a world file next to `path` (`.wld`, or `.pgw`-style).
/// Reads a JPEG file with its world file, decoded as GDAL's JPEG driver decodes it (libjpeg's
/// output, see [`crate::jpeg`]).
pub(crate) fn read_jpeg(path: &Path) -> Result<RasterDataset> {
    let img = crate::jpeg::decode_file(&std::fs::read(path)?).map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
    let (cols, rows, bands) = (img.width, img.height, img.components);
    let n = cols * rows;
    let mut data = fallible::filled(fallible::len(&[n, bands], "the JPEG raster")?, 0u8, "the JPEG raster")?;
    for (i, px) in img.data.chunks_exact(bands).enumerate() {
        for (b, v) in px.iter().enumerate() {
            data[b * n + i] = *v;
        }
    }
    let gt = read_world_file(path).unwrap_or(common::NO_GEOTRANSFORM);
    let raster = common::make_raster(cols, rows, bands, RasterData::U8(data), gt, Default::default(), None)?;
    Ok(RasterDataset::with_driver(raster, RasterDriver::JPEG, false))
}

/// Writes a JPEG file (see [`encode_jpeg`]), its world file and its CRS sidecar.
pub(crate) fn write_jpeg(r: &Raster, path: &Path, quality: Option<&str>) -> Result<()> {
    let bytes = encode_jpeg(r, quality)?;
    std::fs::write(path, bytes)?;
    write_world_file(r, path)?;
    common::write_pam(path, &r.crs)
}

/// Encodes a JPEG as GDAL's JPEG driver does (`JPGDataset::CreateCopy`, GDAL 3.12.1, with
/// libjpeg's defaults): 1 band as grayscale, 3 bands as YCbCr with 4:2:0 chroma subsampling (each
/// 2 x 2 block's chroma averaged, as libjpeg's `h2v2_downsample` averages it), the quantization
/// tables of the `QUALITY` creation option (1 to 100, default 75) scaled as libjpeg scales them,
/// and optimized Huffman tables (GDAL sets `optimize_coding`). Grayscale files hold only the
/// luminance table, as libjpeg's do. Other data types are rounded and saturated to Byte. GDAL also
/// writes 4 bands (as CMYK) and 12-bit samples; terrio does not.
pub(crate) fn encode_jpeg(r: &Raster, quality: Option<&str>) -> Result<Vec<u8>> {
    let quality = match quality {
        None => 75,
        // `atoi`, as GDAL reads it.
        Some(q) => match crate::ops::dem::atoi(q) {
            v @ 1..=100 => v as u8,
            _ => return Err(Error::InvalidArgument(format!("QUALITY={q} is not a legal value in the range 1-100."))),
        },
    };
    let color = match r.bands {
        1 => jpeg_encoder::ColorType::Luma,
        3 => jpeg_encoder::ColorType::Rgb,
        n => {
            return Err(Error::Unsupported(format!(
                "JPEG driver doesn't support {n} bands: terrio writes 1 (grey) or 3 (RGB) bands (GDAL also writes 4, as CMYK)"
            )));
        }
    };
    let (Ok(w), Ok(h)) = (u16::try_from(r.cols), u16::try_from(r.rows)) else {
        return Err(Error::Unsupported("JPEG files are at most 65535 pixels wide and high".into()));
    };
    let converted;
    let r = if r.data_type == DataType::U8 {
        r
    } else {
        converted = common::convert(r, DataType::U8);
        &converted
    };
    let RasterData::U8(v) = &r.data else { unreachable!("converted above") };
    let n = r.cols * r.rows;
    let mut samples = fallible::filled(fallible::len(&[n, r.bands], "the JPEG image")?, 0u8, "the JPEG image")?;
    for (i, px) in samples.chunks_exact_mut(r.bands).enumerate() {
        for (b, s) in px.iter_mut().enumerate() {
            *s = v[b * n + i];
        }
    }
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality);
    if r.bands == 3 {
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
        enc.set_chroma_subsampling_method(jpeg_encoder::ChromaSubsamplingMethod::Average);
    }
    enc.set_optimized_huffman_tables(true);
    enc.encode(&samples, w, h, color).map_err(|e| Error::InvalidData(format!("JPEG: {e}")))?;
    if r.bands == 1 {
        drop_chroma_table(&mut out);
    }
    Ok(out)
}

/// Removes the chroma quantization table that jpeg-encoder writes for a grayscale image too, in a
/// DQT segment of its own that no component uses: 69 bytes that GDAL's files do not have.
fn drop_chroma_table(jpeg: &mut Vec<u8>) {
    let mut i = 2;
    while i + 4 < jpeg.len() && jpeg[i] == 0xFF && jpeg[i + 1] != 0xDA {
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        // One 8-bit table (precision 0, id 1): the marker's length, then 1 + 64 bytes.
        if jpeg[i + 1] == 0xDB && len == 67 && jpeg[i + 4] == 0x01 && i + 2 + len <= jpeg.len() {
            jpeg.drain(i..i + 2 + len);
            return;
        }
        i += 2 + len;
    }
}

pub(crate) fn read_world_file(path: &Path) -> Option<[f64; 6]> {
    if crate::config::sidecars_disabled() {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let short = format!("{}{}w", &ext[..1], &ext[ext.len() - 1..]);
    let long = format!("{ext}w");
    let text = ["wld", short.as_str(), long.as_str()].iter().find_map(|e| std::fs::read_to_string(common::sibling(path, e)).ok())?;
    let v: Vec<f64> = text.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    if v.len() < 6 {
        return None;
    }
    // World files give the centre of the top-left pixel.
    Some([v[4] - v[0] / 2.0 - v[2] / 2.0, v[0], v[2], v[5] - v[1] / 2.0 - v[3] / 2.0, v[1], v[3]])
}

/// Writes a world file (`.wld`) for a raster.
pub(crate) fn write_world_file(r: &Raster, path: &Path) -> Result<()> {
    let (dx, dy) = (r.cell_size_x, -r.cell_size_y);
    let text = format!("{dx}\n0\n0\n{dy}\n{}\n{}\n", r.x_min + dx / 2.0, r.y_max() + dy / 2.0);
    std::fs::write(path.with_extension("wld"), text)?;
    Ok(())
}

/// Writes a PNG (Byte or UInt16, 1 to 4 bands), its world file and its CRS sidecar.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    let color = match r.bands {
        1 => png::ColorType::Grayscale,
        2 => png::ColorType::GrayscaleAlpha,
        3 => png::ColorType::Rgb,
        4 => png::ColorType::Rgba,
        n => return Err(Error::Unsupported(format!("PNG holds 1 to 4 bands, not {n}"))),
    };
    let sixteen = !matches!(r.data_type, DataType::U8 | DataType::I8);
    let converted;
    let r = if sixteen && r.data_type != DataType::U16 {
        converted = common::convert(r, DataType::U16);
        &converted
    } else if r.data_type == DataType::I8 {
        converted = common::convert(r, DataType::U8);
        &converted
    } else {
        r
    };
    let n = r.cols * r.rows;
    let mut bytes = Vec::with_capacity(n * r.bands * if sixteen { 2 } else { 1 });
    for p in 0..n {
        for b in 0..r.bands {
            match &r.data {
                RasterData::U8(v) => bytes.push(v[b * n + p]),
                RasterData::U16(v) => bytes.extend(v[b * n + p].to_be_bytes()),
                _ => unreachable!("converted above"),
            }
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, r.cols as u32, r.rows as u32);
    enc.set_color(color);
    enc.set_depth(if sixteen { png::BitDepth::Sixteen } else { png::BitDepth::Eight });
    if let (Some(nd), png::ColorType::Grayscale) = (nodata, color)
        && nd >= 0.0
        && nd <= if sixteen { 65535.0 } else { 255.0 }
        && nd.fract() == 0.0
    {
        enc.set_trns((nd as u16).to_be_bytes().to_vec());
    }
    let mut w = enc.write_header().map_err(|e| png_err(path, e))?;
    w.write_image_data(&bytes).map_err(|e| png_err(path, e))?;
    w.finish().map_err(|e| png_err(path, e))?;
    write_world_file(r, path)?;
    common::write_pam(path, &r.crs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a PNG of 8-bit palette indices: `entries` colours `(i, 255 - i, 3i)`, and the
    /// alphas of a tRNS chunk.
    fn paletted(path: &Path, indices: &[u8], cols: u32, entries: usize, trns: Option<Vec<u8>>) {
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut enc = png::Encoder::new(file, cols, indices.len() as u32 / cols);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette((0..entries).flat_map(|i| [i as u8, 255 - i as u8, (3 * i) as u8]).collect::<Vec<u8>>());
        if let Some(t) = trns {
            enc.set_trns(t);
        }
        let mut w = enc.write_header().unwrap();
        w.write_image_data(indices).unwrap();
        w.finish().unwrap();
    }

    /// GDAL's `PNGDataset::Open`: the palette is band 1's color table, each alpha from the tRNS
    /// chunk and 255 past its end, the band's interpretation Palette; the index of the only fully
    /// transparent entry is the nodata value, and there is none when two entries are.
    #[test]
    fn reads_a_palette_as_gdal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("p.png");
        let cases =
            [(None, [255, 255, 255], None), (Some(vec![255, 0]), [255, 0, 255], Some(1.0)), (Some(vec![0, 0, 128]), [0, 0, 128], None)];
        for (trns, alphas, nodata) in cases {
            paletted(&p, &[0, 1, 2, 1, 0, 2], 3, 3, trns.clone());
            let ds = read(&p).unwrap();
            let expected: Vec<[u8; 4]> = (0..3).map(|i| [i as u8, 255 - i as u8, 3 * i as u8, alphas[i]]).collect();
            assert_eq!(ds.color_table, expected, "tRNS {trns:?}");
            assert_eq!(ds.band_metadata.len(), 1);
            assert_eq!(ds.band_metadata[0].color_interp.as_deref(), Some("Palette"));
            assert_eq!(ds.nodata(), nodata, "tRNS {trns:?}");
            assert_eq!(ds.has_nodata, nodata.is_some());
            let RasterData::U8(v) = &ds.raster.data else { panic!("Byte indices") };
            assert_eq!(v, &[0, 1, 2, 1, 0, 2]);
        }
        // A grayscale PNG has no color table.
        let gray = common::make_raster(3, 2, 1, RasterData::U8(vec![0, 1, 2, 3, 4, 5]), common::NO_GEOTRANSFORM, Default::default(), None)
            .unwrap();
        write(&gray, None, &p).unwrap();
        let ds = read(&p).unwrap();
        assert!(ds.color_table.is_empty());
        assert!(ds.band_metadata.is_empty());
    }

    /// GDAL's `PNGRasterBand::GetColorInterpretation`: gray (left unset), gray and alpha, RGB,
    /// RGBA.
    #[test]
    fn reads_colour_interpretations_as_gdal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.png");
        let expected: [&[&str]; 4] = [&[], &["Gray", "Alpha"], &["Red", "Green", "Blue"], &["Red", "Green", "Blue", "Alpha"]];
        for (bands, names) in (1..=4).zip(expected) {
            let r = common::make_raster(2, 2, bands, RasterData::U8(vec![7; 4 * bands]), common::NO_GEOTRANSFORM, Default::default(), None)
                .unwrap();
            write(&r, None, &p).unwrap();
            let ds = read(&p).unwrap();
            let got: Vec<&str> = ds.band_metadata.iter().filter_map(|m| m.color_interp.as_deref()).collect();
            assert_eq!(got, names, "{bands} bands");
            assert!(ds.color_table.is_empty());
        }
    }

    /// A COG of a paletted PNG, as GDAL's COG driver writes it: the color table carried
    /// (`Photometric` palette and its `ColorMap`), and the overviews by nearest neighbour, the
    /// driver's default for paletted rasters (`GetResampling` in `cogdriver.cpp`), not CUBIC on
    /// the indices.
    #[test]
    fn a_cog_of_a_palette_keeps_it_and_its_overviews_take_indices() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("p.png");
        // One-pixel stripes of indices 0 and 200, which cubic resampling would blend.
        let (cols, rows) = (600u32, 520u32);
        let indices: Vec<u8> = (0..rows * cols).map(|i| if i % 2 == 0 { 0 } else { 200 }).collect();
        paletted(&src, &indices, cols, 256, None);
        let ds = read(&src).unwrap();
        let out = dir.path().join("p.tif");
        let options =
            crate::RasterWriteOptions { driver: Some(RasterDriver::Cog), compression: Some("DEFLATE".into()), ..Default::default() };
        crate::write_raster_dataset(&ds, out.to_str().unwrap(), &options).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(dec.find_tag_unsigned::<u16>(tiff::tags::Tag::PhotometricInterpretation).unwrap(), Some(3));
        let map = dec.find_tag_unsigned_vec::<u16>(tiff::tags::Tag::ColorMap).unwrap().unwrap();
        assert_eq!(map.len(), 768);
        assert_eq!([map[5], map[256 + 5], map[512 + 5]], [5 * 257, 250 * 257, 15 * 257]);
        let mut lazy = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
        assert_eq!(lazy.overviews(), vec![(300, 260)]);
        let o = lazy.read_window(crate::Window::new(0, 0, 300, 260), &crate::WindowOptions { bands: vec![], overview: Some(0) }).unwrap();
        let RasterData::U8(v) = &o.raster.data else { panic!("Byte overview") };
        assert!(v.iter().all(|x| *x == 0 || *x == 200), "an overview value is no index: {:?}", v.iter().find(|x| **x != 0 && **x != 200));
    }

    /// The marker segments of a JPEG up to its first scan: `(marker, payload)`.
    fn segments(b: &[u8]) -> Vec<(u8, &[u8])> {
        let mut out = Vec::new();
        let mut i = 2;
        while i + 4 <= b.len() && b[i] == 0xFF {
            let marker = b[i + 1];
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            out.push((marker, &b[i + 4..i + 2 + len]));
            if marker == 0xDA {
                break;
            }
            i += 2 + len;
        }
        out
    }

    /// The quantization tables of a JPEG (8-bit, as baseline files have them), by table id.
    fn quant_tables(b: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        for (_, p) in segments(b).into_iter().filter(|(m, _)| *m == 0xDB) {
            for t in p.as_chunks::<65>().0.iter() {
                assert_eq!(t[0] >> 4, 0, "8-bit tables");
                out.push((t[0] & 0x0F, t[1..].to_vec()));
            }
        }
        out
    }

    /// The baseline frame header's components: `(id, sampling factors byte, table id)`.
    fn frame_components(b: &[u8]) -> Vec<(u8, u8, u8)> {
        let (_, sof) = segments(b).into_iter().find(|(m, _)| *m == 0xC0).expect("a baseline frame");
        sof[6..].as_chunks::<3>().0.iter().map(|c| (c[0], c[1], c[2])).collect()
    }

    /// Annex K's luminance table scaled as libjpeg scales it (`jpeg_quality_scaling`,
    /// `jpeg_add_quant_table` with baseline values), in zigzag order its first values are DC and
    /// the first AC coefficients: 16, 11, 12.
    fn libjpeg_luma(quality: u32) -> [u8; 3] {
        let scale = if quality < 50 { 5000 / quality } else { 200 - 2 * quality };
        [16u32, 11, 12].map(|v| ((v * scale + 50) / 100).clamp(1, 255) as u8)
    }

    fn rgb(cols: usize, rows: usize, v: Vec<u8>) -> Raster {
        common::make_raster(cols, rows, v.len() / (cols * rows), RasterData::U8(v), common::NO_GEOTRANSFORM, Default::default(), None)
            .unwrap()
    }

    /// `QUALITY` sets the tables as libjpeg scales them (GDAL's default 75 when not given), and
    /// three bands are YCbCr with 4:2:0 chroma, one band grayscale, as GDAL's JPEG driver writes
    /// them (`JPGDataset::CreateCopy`).
    #[test]
    fn jpeg_quality_and_chroma_subsampling_as_gdal() {
        let (cols, rows) = (40, 24);
        let v: Vec<u8> = (0..3 * cols * rows).map(|i| ((i * 37) % 256) as u8).collect();
        let r = rgb(cols, rows, v.clone());
        for (option, quality) in [(None, 75), (Some("50"), 50), (Some("75"), 75), (Some("90"), 90), (Some("95"), 95), (Some("85.9"), 85)] {
            let b = encode_jpeg(&r, option).unwrap();
            let tables = quant_tables(&b);
            let luma = &tables.iter().find(|(id, _)| *id == 0).expect("table 0").1;
            assert_eq!(luma[..3], libjpeg_luma(quality), "QUALITY={option:?}");
            // Y sampled 2 x 2, Cb and Cr 1 x 1: 4:2:0.
            let comps = frame_components(&b);
            assert_eq!(comps.iter().map(|c| c.1).collect::<Vec<_>>(), [0x22, 0x11, 0x11], "QUALITY={option:?}");
        }
        let gray = rgb(cols, rows, v[..cols * rows].to_vec());
        let b = encode_jpeg(&gray, Some("80")).unwrap();
        assert_eq!(frame_components(&b).iter().map(|c| c.1).collect::<Vec<_>>(), [0x11]);
        // The luminance table only, as libjpeg writes a grayscale file.
        let tables = quant_tables(&b);
        assert_eq!(tables.iter().map(|t| t.0).collect::<Vec<_>>(), [0]);
        assert_eq!(tables[0].1[..3], libjpeg_luma(80));
        let img = crate::jpeg::decode_file(&b).unwrap();
        assert_eq!((img.width, img.height, img.components), (cols, rows, 1));
    }

    /// The 4:2:0 chroma is each 2 x 2 block's average (libjpeg's `h2v2_downsample`), not one
    /// pixel's: on a checkerboard of red and blue pixels the decoded image keeps their mean, 127.5
    /// in red and in blue; taking the top-left (red) pixel's chroma gives a red mean near 230.
    #[test]
    fn jpeg_chroma_is_averaged_as_libjpeg_averages_it() {
        let (cols, rows) = (32, 32);
        let n = cols * rows;
        let red = |i: usize| (i / cols + i % cols).is_multiple_of(2);
        let mut v = vec![0u8; 3 * n];
        for i in 0..n {
            if red(i) {
                v[i] = 255;
            } else {
                v[2 * n + i] = 255;
            }
        }
        let b = encode_jpeg(&rgb(cols, rows, v), Some("100")).unwrap();
        let img = crate::jpeg::decode_file(&b).unwrap();
        let mean = |c: usize| img.data.as_chunks::<3>().0.iter().map(|p| p[c] as f64).sum::<f64>() / n as f64;
        assert!((mean(0) - 127.5).abs() < 4.0 && (mean(2) - 127.5).abs() < 4.0, "mean red {} blue {}", mean(0), mean(2));
    }

    /// GDAL's refusals: a quality outside 1 to 100 (read with `atoi`), and band counts it does not
    /// write; terrio does not write GDAL's 4-band CMYK either.
    #[test]
    fn jpeg_refuses_what_gdal_refuses() {
        let r = rgb(4, 4, vec![9; 48]);
        for q in ["0", "101", "-5", "abc"] {
            let err = encode_jpeg(&r, Some(q)).unwrap_err().to_string();
            assert!(err.contains(&format!("QUALITY={q} is not a legal value in the range 1-100.")), "{err}");
        }
        for bands in [2, 4, 5] {
            let err = encode_jpeg(&rgb(4, 4, vec![9; 16 * bands]), None).unwrap_err().to_string();
            assert!(err.contains(&format!("doesn't support {bands} bands")), "{err}");
        }
    }

    /// Through the public write path, as `terrio.rio` writes it: the `QUALITY` creation option
    /// reaches the encoder, and the pixels decode close to the source.
    #[test]
    fn jpeg_write_path_takes_the_quality_option() {
        let dir = tempfile::tempdir().unwrap();
        let (cols, rows) = (48, 32);
        let v: Vec<u8> = (0..3 * cols * rows).map(|i| ((i % cols) * 4 + (i / cols % rows) * 2) as u8).collect();
        let ds = RasterDataset::with_driver(rgb(cols, rows, v.clone()), RasterDriver::GTiff, false);
        for quality in [60u32, 90] {
            let p = dir.path().join(format!("q{quality}.jpg"));
            let options = crate::RasterWriteOptions {
                driver: Some(RasterDriver::JPEG),
                creation_options: vec![("quality".into(), quality.to_string())],
                ..Default::default()
            };
            crate::write_raster_dataset(&ds, p.to_str().unwrap(), &options).unwrap();
            let b = std::fs::read(&p).unwrap();
            assert_eq!(quant_tables(&b)[0].1[..3], libjpeg_luma(quality), "QUALITY={quality}");
            let back = read_jpeg(&p).unwrap();
            let RasterData::U8(got) = &back.raster.data else { panic!("Byte bands") };
            let mse = got.iter().zip(&v).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum::<f64>() / v.len() as f64;
            assert!(10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10() > 30.0, "QUALITY={quality}: MSE {mse}");
        }
    }
}
