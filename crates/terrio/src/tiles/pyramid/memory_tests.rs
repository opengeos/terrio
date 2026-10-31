//! The buffers of a tile pyramid sized from its source's header (rows of the level's width): the
//! rows of the full-resolution level read at once are refused, before any is read, when the
//! process cannot hold them, and every buffer of rows is allocated fallibly, so that a width no
//! allocator can satisfy fails the write with an error instead of ending the process (as
//! terrio's readers allocate the buffers a file sizes, see `crate::dataset::fallible`).

use super::*;

/// The rows of a full-resolution level, counting the rows asked for and giving none.
struct Counted(usize);

impl BaseRows for Counted {
    fn next_rows(&mut self, n: usize) -> Result<Vec<Vec<u8>>> {
        self.0 += n;
        Err(crate::Error::InvalidData(format!("{n} rows asked for")))
    }
}

/// A pyramid of 3 data bands (and the alpha band) on `levels`.
fn plan(levels: Vec<LevelGeom>, method: Method) -> Plan {
    Plan { levels, bands: 3, method, format: TileFormat::Png, quality: 75 }
}

/// The message of an allocation error; panics on any other result.
pub(crate) fn out_of_memory<T>(result: Result<T>) -> String {
    match result {
        Err(crate::Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory => e.to_string(),
        Err(e) => panic!("not an allocation error: {e}"),
        Ok(_) => panic!("allocated"),
    }
}

/// Asserts that `message` is a fallible allocation's error for a pyramid's rows.
pub(crate) fn cannot_allocate(message: &str) {
    assert!(message.starts_with("cannot allocate ") && message.ends_with(&format!(" for {ROWS}")), "{message}");
}

/// A width whose rows of 4 bands take more bytes than an allocation may (`isize::MAX`).
pub(crate) const TOO_WIDE: usize = usize::MAX / 4;

#[test]
fn pyramids_the_process_cannot_hold_are_refused_before_any_row_is_read() {
    if crate::usable_memory().is_none() {
        return; // Nothing to refuse against: `usable_memory` reads `/proc`.
    }
    // A level of 2^40 x 4096 pixels: its rows alone take more than any machine holds.
    let w = 1usize << 40;
    let level = LevelGeom { z: 30, w, h: 1 << 12, sx: 0, sy: 0 };
    let (mut base, mut tiles) = (Counted(0), Vec::new());
    let message = out_of_memory(build(&plan(vec![level], Method::Nearest), &mut base, &mut tiles, 256));
    let refusal = format!("{HELD} ({w} x 4096 pixels of 4 bands, zoom levels 30 to 30) are too large to allocate: ");
    assert!(message.starts_with(&refusal), "{message}");
    assert!(message.ends_with(" bytes of memory the process can use"), "{message}");
    assert_eq!(base.0, 0);
    assert!(tiles.is_empty());

    // Buffers past the address space: refused alike.
    let levels = vec![LevelGeom { z: 1, w: 1, h: 1, sx: 0, sy: 0 }, LevelGeom { z: 0, w: TOO_WIDE, h: 1, sx: 0, sy: 0 }];
    let message = out_of_memory(build(&plan(levels, Method::Nearest), &mut base, &mut tiles, 256));
    let refusal =
        format!("{HELD} (1 x 1 pixels of 4 bands, zoom levels 0 to 1) are too large to allocate: more bytes than the address space holds");
    assert_eq!(message, refusal);
    assert_eq!(base.0, 0);

    // A pyramid the process can hold is read.
    let small = LevelGeom { z: 10, w: 512, h: 300, sx: 0, sy: 0 };
    let e = build(&plan(vec![small], Method::Nearest), &mut base, &mut tiles, 256).unwrap_err();
    assert_eq!(e.to_string(), "256 rows asked for");
    assert_eq!(base.0, 256);
}

/// A sink that keeps nothing (as a PMTiles archive's, which spills its tiles to a file), for the
/// Linux-only test below.
#[cfg(target_os = "linux")]
struct Dropped;

#[cfg(target_os = "linux")]
impl TileSink for Dropped {
    fn put(&mut self, _z: u8, _x: u32, _y: u32, _data: Vec<u8>) -> Result<()> {
        Ok(())
    }
}

/// Two levels, 65536 x 512 pixels of one band and the level below, bilinear: a strip of 256 rows
/// (with the alpha band) takes 32 MiB.
fn strip_of_32_mib() -> Plan {
    let levels = vec![LevelGeom { z: 8, w: 1 << 16, h: 512, sx: 0, sy: 0 }, LevelGeom { z: 7, w: 1 << 15, h: 256, sx: 0, sy: 0 }];
    Plan { levels, bands: 1, method: Method::Bilinear, format: TileFormat::Png, quality: 75 }
}

/// In a process of its own whose resident-set limit, which the usable memory counts (and Linux
/// does not enforce), is 64 MiB: a pyramid whose strip of rows fits (32 MiB) but whose other
/// buffers do not is refused before a row is asked for; with no such limit its rows are read.
#[cfg(target_os = "linux")]
#[test]
fn a_pyramid_whose_strip_fits_but_not_its_other_buffers_is_refused_unread() {
    crate::par::alone(120, || {
        crate::vsi::tests::set_limit(libc::RLIMIT_RSS, 64 << 20);
        assert_eq!(crate::usable_memory(), Some(64 << 20));
        let plan = strip_of_32_mib();
        let mut base = Counted(0);
        let message = out_of_memory(build(&plan, &mut base, &mut Dropped, 256));
        let refusal = format!("{HELD} (65536 x 512 pixels of 2 bands, zoom levels 7 to 8) are too large to allocate: ");
        assert!(message.starts_with(&refusal), "{message}");
        assert!(message.ends_with(" bytes, more than the 67108864 bytes of memory the process can use"), "{message}");
        assert_eq!(base.0, 0);
        crate::vsi::tests::set_limit(libc::RLIMIT_RSS, libc::RLIM_INFINITY);
        let e = build(&plan, &mut base, &mut Dropped, 256).unwrap_err();
        assert_eq!((e.to_string().as_str(), base.0), ("256 rows asked for", 256));
        true
    });
}

/// The estimate counts each buffer [`build`] holds: the rows kept and given to each level, the
/// horizontally filtered rows a batch of the next level reads, the tile row being encoded and
/// the tiles gathered for an MBTiles archive (each lower bound below, from the code, in bytes).
#[test]
fn the_estimate_counts_every_buffer_the_pyramid_holds() {
    let held = |plan: &Plan, read: u64, copies: u64| held_bytes(plan, 256, (0, read), copies).unwrap();
    let bilinear = strip_of_32_mib();
    let nearest = Plan { method: Method::Nearest, ..strip_of_32_mib() };
    let (w0, w1, nb) = (1u64 << 16, 1u64 << 15, 2u64);
    // Rows: 255 kept and a strip of 256 given to the first level, the second's 256.
    let rows = 511 * nb * w0 + 256 * nb * w1;
    // A tile row of the first level being encoded: 256 tiles of 256 x 256 pixels of 2 bands, and
    // on each thread two tiles' pixels and the encoder's buffers.
    let tile_row = 256 * 256 * 256 * nb;
    let threads = crate::par::threads() as u64 * (256 * 256 * nb * 2 + (1 << 20));
    assert!(held(&nearest, 0, 0) >= rows + tile_row + threads, "{}", held(&nearest, 0, 0));
    // The bilinear pass: 63 x 2 + 6 = 132 filtered rows of the first level, 8 x 3 bytes a pixel.
    assert!(held(&bilinear, 0, 0) - held(&nearest, 0, 0) >= 132 * 8 * (nb + 1) * w1);
    // A strip being read instead of a tile row being encoded, when it is larger.
    assert!(held(&nearest, 1 << 40, 0) >= rows + (1 << 40));
    // MBTiles: the 256 x 2 + 128 tiles, three times while the archive is written.
    assert!(held(&nearest, 0, 3) >= 3 * 640 * 256 * 256 * nb);
}

#[test]
fn row_buffers_too_large_to_allocate_are_errors() {
    // The taps of a convolution, one per output pixel.
    for method in [Method::Bilinear, Method::Cubic] {
        cannot_allocate(&out_of_memory(Down::new(method, 4, 512, 512, TOO_WIDE, 1)));
    }
    // The rows a level is downsampled with: horizontally filtered rows (the mask's, then the
    // values' of every band), output rows.
    let down = Down::new(Method::Nearest, 4, 1, 1, TOO_WIDE, 1).unwrap();
    let row = [255u8; 4];
    cannot_allocate(&out_of_memory(down.horizontal(&row)));
    let many_bands = Down::new(Method::Nearest, 1 << 41, 0, 1, 1 << 20, 1).unwrap();
    cannot_allocate(&out_of_memory(many_bands.horizontal(&[])));
    let hrow = (Vec::new(), Vec::new());
    cannot_allocate(&out_of_memory(down.row(0, &|_| &row[..], &|_| &hrow)));
    // The rows of a level held in memory, copied a strip at a time.
    let mut rows = MemoryRows { w: TOO_WIDE, h: 1, bands: 3, data: &[], alpha: &[], next: 0 };
    cannot_allocate(&out_of_memory(rows.next_rows(1)));
}
