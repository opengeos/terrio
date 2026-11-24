// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
static COUNT: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
struct Meter;
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNT.load(Relaxed) {
            CALLS.fetch_add(1, Relaxed);
            BYTES.fetch_add(l.size() as u64, Relaxed);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if COUNT.load(Relaxed) {
            CALLS.fetch_add(1, Relaxed);
            BYTES.fetch_add(n as u64, Relaxed);
        }
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;
use std::time::Instant;
fn main() {
    let mut raw = vec![0u8; 54431 * 4];
    let mut rng = 37u32;
    for (i, b) in raw.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        b.copy_from_slice(&(if i % 101 < 85 { -9999.0f32 } else { (rng % 10000) as f32 / 100. }).to_le_bytes());
    }
    let n = 4096;
    let mut setup = 0u128;
    let mut encode = 0u128;
    let mut reset = 0u128;
    let mut bytes = 0usize;
    COUNT.store(std::env::args().any(|a| a == "--alloc"), Relaxed);
    for _ in 0..n {
        let t = Instant::now();
        let mut e = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
        setup += t.elapsed().as_nanos();
        let t = Instant::now();
        bytes += std::hint::black_box(e.encode(&raw).unwrap()).len();
        encode += t.elapsed().as_nanos();
    }
    let fresh_calls = CALLS.swap(0, Relaxed);
    let fresh_bytes = BYTES.swap(0, Relaxed);
    let mut e = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
    let mut reused = 0u128;
    for _ in 0..n {
        let t = Instant::now();
        e.reset();
        reset += t.elapsed().as_nanos();
        let t = Instant::now();
        bytes += std::hint::black_box(e.encode(&raw).unwrap()).len();
        reused += t.elapsed().as_nanos();
    }
    COUNT.store(false, Relaxed);
    eprintln!(
        "alloc fresh_calls={} fresh_bytes={} reused_calls={} reused_bytes={}",
        fresh_calls,
        fresh_bytes,
        CALLS.load(Relaxed),
        BYTES.load(Relaxed)
    );
    println!(
        "{{\"strips_per_arm\":{n},\"setup_ns\":{setup},\"fresh_encode_ns\":{encode},\"reset_ns\":{reset},\"reused_encode_ns\":{reused},\"bytes\":{bytes}}}"
    );
}
