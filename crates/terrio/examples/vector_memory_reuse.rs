// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Measure read time and host-allocator reuse after dropping a vector dataset.
use serde_json::json;

fn peak_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|status| {
        status.lines().find(|line| line.starts_with("VmHWM:")).and_then(|line| line.split_whitespace().nth(1)?.parse().ok())
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: vector_memory_reuse FILE [256-KiB BLOCKS, default 768]".into());
    }
    let count = args.get(2).map(|v| v.parse::<usize>()).transpose()?.unwrap_or(768);
    let base = peak_kib();
    let started = std::time::Instant::now();
    let dataset = terrio::open_vector(&args[1], &Default::default())?;
    let read_ms = started.elapsed().as_secs_f64() * 1000.;
    let features: usize = dataset.layers.iter().map(|l| l.features.len()).sum();
    let read_peak = peak_kib();
    drop(dataset);
    let blocks: Vec<_> = (0..count).map(|_| vec![1u8; 256 * 1024]).collect();
    std::hint::black_box(&blocks);
    let final_peak = peak_kib();
    println!(
        "{}",
        json!({"features": features, "read_ms": read_ms, "base_kib": base,
        "read_peak_kib": read_peak, "after_blocks_peak_kib": final_peak, "blocks": count})
    );
    Ok(())
}
