// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2010, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     gcore/overview.cpp

//! Tile pyramids built a row at a time: the full-resolution level is read from the top in strips,
//! each tile row is encoded as soon as its rows are there, and each lower zoom level is
//! downsampled from the rows of the level above as they come. Only the rows still needed by a
//! tile row being filled or by the next level's resampling kernel are held, so memory is bounded
//! by about one tile row per zoom level (plus the kernel's rows), whatever the raster's height.
//!
//! The pixels are those of the in-memory pyramid GDAL's MBTiles driver and `gdaladdo` compute
//! (and terrio computed before it streamed): every output row is computed with the same
//! arithmetic, in the same order, from the same source rows.

use super::TileFormat;
use crate::Result;
use std::collections::VecDeque;

/// The tile size, in pixels.
const TS: i64 = 256;

/// Rows of the full-resolution level fed to [`build`], from the top: each row holds the data
/// bands then the alpha band, one after the other, `width` bytes each.
pub(crate) trait BaseRows {
    /// The next `n` rows.
    fn next_rows(&mut self, n: usize) -> Result<Vec<Vec<u8>>>;
}

/// Where encoded tiles go.
pub(crate) trait TileSink {
    /// Takes tile `(z, x, y)`.
    fn put(&mut self, z: u8, x: u32, y: u32, data: Vec<u8>) -> Result<()>;
}

impl TileSink for Vec<(u8, u32, u32, Vec<u8>)> {
    fn put(&mut self, z: u8, x: u32, y: u32, data: Vec<u8>) -> Result<()> {
        self.push((z, x, y, data));
        Ok(())
    }
}

/// A zoom level of a pyramid: its size, and the global pixel position of its first pixel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LevelGeom {
    pub z: u8,
    pub w: usize,
    pub h: usize,
    pub sx: i64,
    pub sy: i64,
}

/// How lower zoom levels are computed from the level above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Nearest,
    Average,
    Bilinear,
    Cubic,
}

impl Method {
    /// The method `RESAMPLING` names, as the pyramid maps it.
    pub(crate) fn parse(resampling: Option<&str>) -> Self {
        match resampling.unwrap_or("bilinear").to_ascii_lowercase().as_str() {
            "near" | "nearest" => Self::Nearest,
            "average" => Self::Average,
            "cubic" | "cubicspline" | "lanczos" => Self::Cubic,
            _ => Self::Bilinear,
        }
    }

    /// The kernel radius and weights of a convolution method.
    fn kernel(self) -> Option<(f64, Weight)> {
        match self {
            Self::Bilinear => Some((1.0, |x: f64| (1.0 - x.abs()).max(0.0))),
            Self::Cubic => Some((2.0, |x: f64| {
                let x = x.abs();
                let a = -0.5;
                if x <= 1.0 {
                    ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0
                } else if x < 2.0 {
                    a * (((x - 5.0) * x + 8.0) * x - 4.0)
                } else {
                    0.0
                }
            })),
            _ => None,
        }
    }
}

/// A convolution kernel's weight at a distance (in source pixels).
type Weight = fn(f64) -> f64;

/// A pyramid to build: its levels from the full resolution down, the data band count (the alpha
/// band comes on top), the overview method and the tile encoding.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub levels: Vec<LevelGeom>,
    pub bands: usize,
    pub method: Method,
    pub format: TileFormat,
    pub quality: u8,
}

/// Per-axis taps of a convolution, as GDAL computes them: for each output index, the first
/// source index and the weights.
fn taps(dst: usize, src_n: usize, ratio: f64, radius: f64, k: Weight) -> Vec<(usize, Vec<f64>)> {
    let scale = (1.0 / ratio).min(1.0);
    let r = radius / scale;
    (0..dst)
        .map(|i| {
            let c = (i as f64 + 0.5) * ratio;
            let lo = ((c - r + 0.5).floor() as i64).max(0) as usize;
            let hi = ((c + r + 0.5) as i64).min(src_n as i64).max(lo as i64) as usize;
            (lo, (lo..hi).map(|j| k(scale * (j as f64 - c + 0.5))).collect())
        })
        .collect()
}

/// Downsampling a level (the source, `sw` x `sh`) to the next one (`w` x `h`), a row at a time,
/// as `gdaladdo` computes overviews: nearest, average, or a separable convolution whose kernel is
/// stretched by the reduction factor, with the alpha band as the weights of every band.
struct Down {
    method: Method,
    nb: usize,
    sw: usize,
    sh: usize,
    w: usize,
    h: usize,
    rx: f64,
    ry: f64,
    tx: Vec<(usize, Vec<f64>)>,
    ty: Vec<(usize, Vec<f64>)>,
    /// The next output row.
    next: usize,
    /// Horizontally filtered source rows (values of every band, then the filtered mask), from
    /// source row `hfirst`.
    hrows: VecDeque<(Vec<f64>, Vec<f64>)>,
    hfirst: usize,
}

impl Down {
    fn new(method: Method, nb: usize, sw: usize, sh: usize, w: usize, h: usize) -> Self {
        let (rx, ry) = (sw as f64 / w as f64, sh as f64 / h as f64);
        let (tx, ty) = match method.kernel() {
            Some((radius, k)) => (taps(w, sw, rx, radius, k), taps(h, sh, ry, radius, k)),
            None => (Vec::new(), Vec::new()),
        };
        Self { method, nb, sw, sh, w, h, rx, ry, tx, ty, next: 0, hrows: VecDeque::new(), hfirst: 0 }
    }

    /// The source rows `lo..hi` output row `y` reads.
    fn src_rows(&self, y: usize) -> (usize, usize) {
        match self.method {
            Method::Nearest => {
                let s = (((y as f64 + 0.5) * self.ry) as usize).min(self.sh - 1);
                (s, s + 1)
            }
            Method::Average => {
                let y0 = (y as f64 * self.ry + 1e-8) as usize;
                let y1 = (((y + 1) as f64 * self.ry - 1e-8).ceil() as usize).clamp(y0 + 1, self.sh);
                (y0, y1)
            }
            Method::Bilinear | Method::Cubic => {
                let (lo, wt) = &self.ty[y];
                (*lo, lo + wt.len())
            }
        }
    }

    /// The horizontal pass over a source row: the values of every band, weighted by the alpha
    /// band, and the filtered mask.
    fn horizontal(&self, row: &[u8]) -> (Vec<f64>, Vec<f64>) {
        let (w, sw, nb) = (self.w, self.sw, self.nb);
        let alpha = &row[(nb - 1) * sw..nb * sw];
        let mut hv = vec![0.0f64; nb * w];
        let mut hm = vec![0.0f64; w];
        for (x, (lo, wt)) in self.tx.iter().enumerate() {
            let (mut sa, mut sw_) = (0.0, 0.0);
            let mut sv = [0.0f64; 4];
            for (t, &wj) in wt.iter().enumerate() {
                let i = lo + t;
                let m = wj * alpha[i] as f64;
                sa += m;
                sw_ += wj;
                for (b, v) in sv.iter_mut().enumerate().take(nb) {
                    *v += m * row[b * sw + i] as f64;
                }
            }
            if sa > 0.0 {
                for (b, v) in sv.iter().enumerate().take(nb) {
                    hv[b * w + x] = (v / sa).clamp(0.0, 255.0);
                }
                hm[x] = (sa / sw_ + 0.5).min(255.0).floor();
            }
        }
        (hv, hm)
    }

    /// Output row `y`, from the source rows (`src(r)` is source row `r`) and, for convolutions,
    /// the horizontally filtered rows (`hrow(r)`).
    fn row<'a>(&self, y: usize, src: &dyn Fn(usize) -> &'a [u8], hrow: &dyn Fn(usize) -> &'a (Vec<f64>, Vec<f64>)) -> Vec<u8> {
        let (w, sw, nb) = (self.w, self.sw, self.nb);
        let mut out = vec![0u8; nb * w];
        match self.method {
            Method::Bilinear | Method::Cubic => {
                let (lo, wt) = &self.ty[y];
                for x in 0..w {
                    let mut sa = 0.0;
                    let mut sv = [0.0f64; 4];
                    for (t, &wj) in wt.iter().enumerate() {
                        let (hv, hm) = hrow(lo + t);
                        let m = wj * hm[x];
                        sa += m;
                        for (b, v) in sv.iter_mut().enumerate().take(nb) {
                            *v += m * hv[b * w + x];
                        }
                    }
                    if sa > 0.0 {
                        for (b, v) in sv.iter().enumerate().take(nb) {
                            out[b * w + x] = (v / sa + 0.5).clamp(0.0, 255.0) as u8;
                        }
                    }
                }
            }
            Method::Average => {
                let (y0, y1) = self.src_rows(y);
                let rows: Vec<&[u8]> = (y0..y1).map(src).collect();
                for x in 0..w {
                    let x0 = (x as f64 * self.rx + 1e-8) as usize;
                    let x1 = (((x + 1) as f64 * self.rx - 1e-8).ceil() as usize).clamp(x0 + 1, sw);
                    let mut sa = 0.0;
                    let mut sv = [0.0f64; 4];
                    for row in &rows {
                        for sx in x0..x1 {
                            let m = row[(nb - 1) * sw + sx] as f64;
                            sa += m;
                            for (b, v) in sv.iter_mut().enumerate().take(nb) {
                                *v += m * row[b * sw + sx] as f64;
                            }
                        }
                    }
                    if sa > 0.0 {
                        for (b, v) in sv.iter().enumerate().take(nb) {
                            out[b * w + x] = (v / sa + 0.5).clamp(0.0, 255.0) as u8;
                        }
                    }
                }
            }
            Method::Nearest => {
                let row = src(self.src_rows(y).0);
                for x in 0..w {
                    let sx = (((x as f64 + 0.5) * self.rx) as usize).min(sw - 1);
                    for b in 0..nb {
                        out[b * w + x] = row[b * sw + sx];
                    }
                }
            }
        }
        out
    }
}

/// A level being built: its rows from row `first`, how many it has, and the next tile row to cut.
struct Level {
    g: LevelGeom,
    rows: VecDeque<Vec<u8>>,
    first: usize,
    /// The next tile row, and the last one (inclusive); `next_ty > last_ty` once all are cut.
    next_ty: i64,
    last_ty: i64,
    /// The first tile column and the last (inclusive).
    tx0: i64,
    tx1: i64,
}

impl Level {
    fn produced(&self) -> usize {
        self.first + self.rows.len()
    }

    fn row(&self, r: usize) -> &[u8] {
        &self.rows[r - self.first]
    }

    /// The rows of tile row `ty`, clamped to the level: `start..end`.
    fn tile_rows(&self, ty: i64) -> (usize, usize) {
        let h = self.g.h as i64;
        let start = (ty * TS - self.g.sy).clamp(0, h);
        let end = ((ty + 1) * TS - self.g.sy).clamp(0, h);
        (start as usize, end as usize)
    }
}

/// Builds the tile pyramid of `plan`, reading the full-resolution level from `base` in strips of
/// `strip` rows, and gives every tile that is not fully transparent to `sink`.
pub(crate) fn build(plan: &Plan, base: &mut dyn BaseRows, sink: &mut dyn TileSink, strip: usize) -> Result<()> {
    let nb = plan.bands + 1;
    let mut levels: Vec<Level> = plan
        .levels
        .iter()
        .map(|&g| {
            let nmax = (1i64 << g.z) - 1;
            let (tx0, tx1) = (g.sx.div_euclid(TS).max(0), (g.sx + g.w as i64 - 1).div_euclid(TS).min(nmax));
            let (ty0, ty1) = (g.sy.div_euclid(TS).max(0), (g.sy + g.h as i64 - 1).div_euclid(TS).min(nmax));
            Level { g, rows: VecDeque::new(), first: 0, next_ty: ty0, last_ty: ty1, tx0, tx1 }
        })
        .collect();
    // `downs[k]` downsamples level `k` to level `k + 1`.
    let mut downs: Vec<Down> = levels.windows(2).map(|p| Down::new(plan.method, nb, p[0].g.w, p[0].g.h, p[1].g.w, p[1].g.h)).collect();
    let h0 = levels[0].g.h;
    let strip = strip.max(1);
    while levels[0].produced() < h0 {
        let n = strip.min(h0 - levels[0].produced());
        let rows = base.next_rows(n)?;
        debug_assert_eq!(rows.len(), n);
        levels[0].rows.extend(rows);
        advance(plan, &mut levels, &mut downs, 0, sink)?;
    }
    Ok(())
}

/// The most rows of a lower level computed at once (in parallel).
const BATCH: usize = 64;

/// Cuts the tile rows of level `k` whose rows are all there, computes the rows of level `k + 1`
/// whose source rows are all there (and so on down), and drops the rows no longer needed.
fn advance(plan: &Plan, levels: &mut [Level], downs: &mut [Down], k: usize, sink: &mut dyn TileSink) -> Result<()> {
    // Tile rows.
    loop {
        let l = &levels[k];
        if l.next_ty > l.last_ty {
            break;
        }
        let (start, end) = l.tile_rows(l.next_ty);
        if l.produced() < end {
            break;
        }
        let ty = l.next_ty;
        let tiles = cut_tile_row(plan, l, ty, start, end)?;
        for (tx, data) in tiles {
            sink.put(l.g.z, tx as u32, ty as u32, data)?;
        }
        levels[k].next_ty += 1;
    }
    // The next level's rows.
    if k + 1 < levels.len() {
        loop {
            let (head, tail) = levels.split_at_mut(k + 1);
            let (src, dst) = (&head[k], &mut tail[0]);
            let down = &mut downs[k];
            let avail = src.produced();
            // The output rows whose source rows are there.
            let mut ys = Vec::new();
            while down.next + ys.len() < down.h && ys.len() < BATCH {
                let y = down.next + ys.len();
                if down.src_rows(y).1 > avail {
                    break;
                }
                ys.push(y);
            }
            if ys.is_empty() {
                break;
            }
            if plan.method == Method::Bilinear || plan.method == Method::Cubic {
                // Horizontally filtered rows up to the last needed, dropping those before the first.
                let lo = down.src_rows(ys[0]).0;
                while down.hfirst < lo && !down.hrows.is_empty() {
                    down.hrows.pop_front();
                    down.hfirst += 1;
                }
                if down.hrows.is_empty() {
                    down.hfirst = lo;
                }
                let hi = ys.iter().map(|&y| down.src_rows(y).1).max().unwrap_or(lo);
                let from = down.hfirst + down.hrows.len();
                if hi > from {
                    let d = &*down;
                    let new = crate::par::map_range(hi - from, |i| d.horizontal(src.row(from + i)));
                    down.hrows.extend(new);
                }
            }
            let d = &*down;
            let rows = crate::par::map_range(ys.len(), |i| d.row(ys[i], &|r| src.row(r), &|r| &d.hrows[r - d.hfirst]));
            down.next += ys.len();
            dst.rows.extend(rows);
            advance(plan, levels, downs, k + 1, sink)?;
        }
    }
    // Rows no longer needed: before the next tile row, and before the next level's next rows.
    let l = &mut levels[k];
    let mut keep = if l.next_ty > l.last_ty { usize::MAX } else { l.tile_rows(l.next_ty).0 };
    if let Some(d) = downs.get(k)
        && d.next < d.h
    {
        keep = keep.min(d.src_rows(d.next).0);
    }
    let keep = keep.min(l.produced());
    while l.first < keep {
        l.rows.pop_front();
        l.first += 1;
    }
    Ok(())
}

/// Encodes the tiles of tile row `ty` of a level (its rows `start..end`), leaving out fully
/// transparent ones. Every tile has an alpha band (but JPEG ones), so that readers that take the
/// band count of a tile (GDAL's PMTiles driver) see the same bands.
fn cut_tile_row(plan: &Plan, l: &Level, ty: i64, start: usize, end: usize) -> Result<Vec<(i64, Vec<u8>)>> {
    let (bands, g) = (plan.bands, l.g);
    let count = (l.tx1 - l.tx0 + 1).max(0) as usize;
    let tiles = crate::par::map_range(count, |i| -> Result<Option<(i64, Vec<u8>)>> {
        let tx = l.tx0 + i as i64;
        let ch = bands + 1;
        let mut px = vec![0u8; (TS * TS) as usize * ch];
        let mut any = false;
        for gy in start..end {
            let y = gy as i64 + g.sy - ty * TS;
            let row = l.row(gy);
            for x in 0..TS {
                let gx = tx * TS + x - g.sx;
                if gx < 0 || gx >= g.w as i64 {
                    continue;
                }
                let gx = gx as usize;
                let o = (y * TS + x) as usize * ch;
                let a = row[bands * g.w + gx];
                any |= a != 0;
                for b in 0..bands {
                    px[o + b] = row[b * g.w + gx];
                }
                px[o + bands] = a;
            }
        }
        if !any {
            return Ok(None);
        }
        Ok(Some((tx, super::raster::encode_tile(&px, bands, plan.format, plan.quality)?)))
    });
    tiles.into_iter().filter_map(|t| t.transpose()).collect()
}

/// The rows of a level held whole in memory, band after band (the alpha band in `alpha`).
pub(crate) struct MemoryRows<'a> {
    pub w: usize,
    pub h: usize,
    pub bands: usize,
    pub data: &'a [u8],
    pub alpha: &'a [u8],
    pub next: usize,
}

impl BaseRows for MemoryRows<'_> {
    fn next_rows(&mut self, n: usize) -> Result<Vec<Vec<u8>>> {
        let (w, total) = (self.w, self.w * self.h);
        let rows = (self.next..self.next + n)
            .map(|r| {
                let mut row = Vec::with_capacity((self.bands + 1) * w);
                for b in 0..self.bands {
                    row.extend_from_slice(&self.data[b * total + r * w..b * total + (r + 1) * w]);
                }
                row.extend_from_slice(&self.alpha[r * w..(r + 1) * w]);
                row
            })
            .collect();
        self.next += n;
        Ok(rows)
    }
}

/// Encoded tiles spilled to a temporary file as they come, to be assembled into an archive at the
/// end: their bytes are never all held in memory.
#[cfg(not(target_family = "wasm"))]
pub(crate) struct SpillSink {
    file: std::io::BufWriter<std::fs::File>,
    path: std::path::PathBuf,
    len: u64,
    pub tiles: Vec<super::pmtiles::SpilledTile>,
}

#[cfg(not(target_family = "wasm"))]
impl SpillSink {
    /// A spill file beside `output`, removed when the sink is dropped.
    pub(crate) fn beside(output: &str) -> Result<Self> {
        let out = std::path::Path::new(output);
        let name = out.file_name().and_then(|s| s.to_str()).unwrap_or("tiles");
        let path = out.with_file_name(format!(".{name}.terrio-tiles-{}.tmp", std::process::id()));
        // A leftover of an earlier run is removed, not written through: a link planted at the name
        // is removed rather than followed, and `create_new` never truncates what exists.
        let _ = std::fs::remove_file(&path);
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        Ok(Self { file: std::io::BufWriter::with_capacity(1 << 20, file), path, len: 0, tiles: Vec::new() })
    }

    /// The spill file, flushed, for reading the tiles back.
    pub(crate) fn file(&mut self) -> Result<&mut std::fs::File> {
        use std::io::Write;
        self.file.flush()?;
        Ok(self.file.get_mut())
    }
}

#[cfg(not(target_family = "wasm"))]
impl Drop for SpillSink {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(not(target_family = "wasm"))]
impl TileSink for SpillSink {
    fn put(&mut self, z: u8, x: u32, y: u32, data: Vec<u8>) -> Result<()> {
        use std::hash::{Hash, Hasher};
        use std::io::Write;
        let len = u32::try_from(data.len()).map_err(|_| crate::Error::InvalidData("a tile is larger than 4 GiB".into()))?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        data.hash(&mut h);
        self.file.write_all(&data)?;
        self.tiles.push(super::pmtiles::SpilledTile {
            id: super::pmtiles::zxy_to_tile_id(z, x, y),
            offset: self.len,
            length: len,
            hash: h.finish(),
        });
        self.len += len as u64;
        Ok(())
    }
}
