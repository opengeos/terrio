//! Diagnostic: separates warm source-byte reads from in-memory TIFF decode.
//! This reads complete sources; it is not an attribution of compose windows.
use std::time::Instant;
use terrio::{LazyRaster, Window, WindowOptions};
fn main() {
    let list = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let mut io = 0u128;
    let mut decode = 0u128;
    let mut bytes = 0;
    for path in list.lines() {
        let t = Instant::now();
        let raw = std::fs::read(path).unwrap();
        io += t.elapsed().as_nanos();
        bytes += raw.len();
        let t = Instant::now();
        let mut src = LazyRaster::from_tiff_bytes(raw).unwrap().unwrap();
        for y in (0..src.height()).step_by(256) {
            std::hint::black_box(
                src.read_window(Window::new(0, y, src.width(), 256.min(src.height() - y)), &WindowOptions::default()).unwrap(),
            );
        }
        decode += t.elapsed().as_nanos();
    }
    println!("source_read_ns={io} memory_decode_ns={decode} file_bytes={bytes}");
}
