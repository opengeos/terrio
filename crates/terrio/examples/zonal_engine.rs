// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Times the zonal statistics engine alone: zones, lazy strip reads and reduction, with each
//! band's results discarded instead of building a layer. Used by `scripts/benchmark_zonal.py`.
//!
//! ```sh
//! cargo run --release -p terrio --example zonal_engine -- ZONES INPUTS.csv STATS [fractional]
//! ```
//!
//! `INPUTS.csv` has a header and `path` as its first column (other columns are ignored); every
//! row is band 1 of its file. `STATS` is comma-separated, as `terrio zonal-stats --stats`.

use terrio::ops::zonal::{Statistic, ZonalBand, ZonalOptions, ZonalSource, zonal_stats_with};

fn main() -> terrio::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, zones, inputs, stats, rest @ ..] = &args[..] else {
        eprintln!("usage: zonal_engine ZONES INPUTS.csv STATS [fractional]");
        std::process::exit(2);
    };
    let start = std::time::Instant::now();
    let zones = terrio::open_vector(zones, &Default::default())?.layers.remove(0);
    let stats = stats.split(',').map(Statistic::parse).collect::<terrio::Result<Vec<_>>>()?;
    let options = ZonalOptions { stats, fractional: rest.first().is_some_and(|m| m == "fractional"), ..Default::default() };
    let text = std::fs::read_to_string(inputs)?;
    let mut sources = text
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            let path = line.split(',').next().unwrap_or_default();
            Ok(ZonalSource { raster: terrio::open_raster_lazy(path)?.into(), bands: vec![ZonalBand::new(1, format!("r{i}"))] })
        })
        .collect::<terrio::Result<Vec<_>>>()?;
    let opened = start.elapsed();
    let bands = std::sync::atomic::AtomicUsize::new(0);
    zonal_stats_with(&zones, &mut sources, &options, &|stats| {
        std::hint::black_box(&stats.values);
        bands.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    })?;
    eprintln!(
        "{} zones, {} bands: opened in {:.2} s, total {:.2} s",
        zones.len(),
        bands.into_inner(),
        opened.as_secs_f64(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
