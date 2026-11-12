// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Profile a native vector page, including an optional bounding-box filter.
use serde_json::json;
use terrio::VectorOpenOptions;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 && args.len() != 8 {
        return Err("usage: vector_read_profile FILE SKIP MAX [MINX MINY MAXX MAXY]".into());
    }
    let options = VectorOpenOptions {
        skip_features: args[2].parse()?,
        max_features: Some(args[3].parse()?),
        spatial_filter: if args.len() == 8 { Some((args[4].parse()?, args[5].parse()?, args[6].parse()?, args[7].parse()?)) } else { None },
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let result = terrio::open_vector(&args[1], &options)?;
    let elapsed = started.elapsed().as_secs_f64() * 1000.;
    let fids: Vec<_> = result.layers.iter().flat_map(|l| l.features.iter().map(|f| f.fid)).collect();
    let peak = std::fs::read_to_string("/proc/self/status").ok().and_then(|status| {
        status.lines().find(|line| line.starts_with("VmHWM:")).and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
    });
    println!("{}", json!({"milliseconds": elapsed, "peak_rss_kib": peak, "features": fids.len(), "fids": fids}));
    Ok(())
}
