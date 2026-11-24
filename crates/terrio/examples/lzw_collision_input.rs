// SPDX-FileCopyrightText: Copyright (c) 2026 Terrio contributors
// SPDX-License-Identifier: MIT
//! Generates a deterministic chosen-input regression for the original TIFF LZW hash.
fn clustered_input(n: usize) -> Vec<u8> {
    let mut table = vec![0u32; 8192];
    let mut successors = vec![0u8; 4096];
    let mut next = 258u16;
    let mut prefix = 0u16;
    let mut data = vec![0];
    let mut state = 37u32;
    while data.len() < n {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let mut best = (0, 0u8, 0usize, 0u32);
        for i in 0..256u32 {
            let symbol = i.wrapping_add(state) as u8;
            let key = (u32::from(prefix) << 8) | u32::from(symbol);
            let mut slot = (key.wrapping_mul(0x9e3779b1) >> 19) as usize;
            let mut probes = 0;
            while table[slot] != 0 && table[slot] >> 12 != key {
                probes += 1;
                slot = (slot + 1) & 8191;
            }
            if probes >= best.0 {
                best = (probes, symbol, slot, key);
            }
        }
        let (_, symbol, slot, key) = best;
        data.push(symbol);
        if table[slot] != 0 {
            prefix = (table[slot] & 4095) as u16;
        } else {
            let count = &mut successors[usize::from(prefix)];
            let omitted = *count == 16;
            *count = count.saturating_add(1);
            if next < 4096 && !omitted {
                table[slot] = (key << 12) | u32::from(next);
            }
            next += 1;
            if next > 4096 {
                table.fill(0);
                successors.fill(0);
                next = 258;
            }
            prefix = u16::from(symbol);
        }
    }
    data
}

fn main() {
    let output = std::env::args().nth(1).expect("usage: lzw_collision_input <out.bin>");
    std::fs::write(output, clustered_input(16384)).unwrap();
}
