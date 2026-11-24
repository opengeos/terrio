// SPDX-FileCopyrightText: Copyright (c) 2026 Terrio contributors
// SPDX-License-Identifier: MIT

//! Whole-chunk TIFF LZW: MSB, eight-bit alphabet, early change, twelve-bit codes.
//! Width and clear policy match weezl 0.2.1's TIFF encoder (including its final
//! EOI width adjustment). This is an independent implementation, not a port.

pub(super) struct Encoder {
    hash: HashEncoder,
    // Only stored codes have witnesses. Offsets must support chunks above 4 GiB.
    starts: Vec<usize>,
    lengths: Vec<u16>,
    parents: Vec<u16>,
}

impl Encoder {
    pub(super) const SCRATCH_BYTES: usize = 8192 * size_of::<u32>() + 4096 * (size_of::<u8>() + size_of::<usize>() + 2 * size_of::<u16>());

    #[cfg(test)]
    pub(super) fn allocated_scratch_bytes(&self) -> usize {
        self.hash.table.capacity() * size_of::<u32>()
            + self.hash.successors.capacity() * size_of::<u8>()
            + self.starts.capacity() * size_of::<usize>()
            + self.lengths.capacity() * size_of::<u16>()
            + self.parents.capacity() * size_of::<u16>()
    }

    pub(super) fn new() -> Self {
        Self { hash: HashEncoder::new(), starts: Vec::new(), lengths: Vec::new(), parents: Vec::new() }
    }

    /// Starts an independent strip/tile. No dictionary state crosses chunks.
    pub(super) fn encode(&mut self, data: &[u8]) -> Vec<u8> {
        if profitable_chunk(data) { self.encode_chunk(data) } else { self.hash.encode(data) }
    }

    fn encode_chunk(&mut self, data: &[u8]) -> Vec<u8> {
        // Retain witnesses after first use; plain chunks never allocate them.
        self.starts.resize(4096, 0);
        self.lengths.resize(4096, 0);
        self.parents.resize(4096, 0);
        self.hash.table.fill(0);
        self.hash.successors.fill(0);
        let mut out = Bits { bytes: Vec::with_capacity(data.len().min(4096)), pending: 0, count: 0 };
        let mut width = 9;
        let mut next = 258u16;
        out.put(256, width);
        if let Some(&first) = data.first() {
            let mut prefix = u16::from(first);
            let mut start = 0;
            let mut cursor = 1;
            // Four-byte phase is a hint for Float32 runs, never a format assumption.
            let mut latest = [0u16; 4];
            let mut longest = [0u16; 4];
            while cursor < data.len() {
                let symbol = data[cursor];
                let key = (u32::from(prefix) << 8) | u32::from(symbol);
                let mut slot = (key.wrapping_mul(0x9e3779b1) >> 19) as usize;
                loop {
                    let entry = self.hash.table[slot];
                    if entry == 0 {
                        // Weezl inserts entry 4096, then immediately clears; it
                        // can never be referenced and need not be stored.
                        // Compatibility: weezl 0.2.1 consumes a code but omits the
                        // seventeenth successor when promoting a sparse tree node.
                        let count = &mut self.hash.successors[usize::from(prefix)];
                        let omitted = *count == 16;
                        *count = count.saturating_add(1);
                        if next < 4096 && !omitted {
                            self.hash.table[slot] = (key << 12) | u32::from(next);
                            {
                                let code = usize::from(next);
                                let length = cursor - start + 1;
                                self.starts[code] = start;
                                self.lengths[code] = length as u16;
                                self.parents[code] = prefix;
                                let phase = start % 4;
                                latest[phase] = next;
                                if length >= usize::from(self.lengths[usize::from(longest[phase])]) {
                                    longest[phase] = next;
                                }
                            }
                        }
                        next += 1;
                        out.put(prefix, width);
                        if next >= (1 << width) && width < 12 {
                            width += 1;
                        }
                        if next > 4096 {
                            out.put(256, width);
                            self.hash.table.fill(0);
                            self.hash.successors.fill(0);
                            next = 258;
                            width = 9;
                            latest = [0; 4];
                            longest = [0; 4];
                        }
                        prefix = u16::from(symbol);
                        start = cursor;
                        cursor += 1;
                        {
                            for candidate in [longest[start % 4], latest[start % 4]] {
                                let code = usize::from(candidate);
                                let length = usize::from(self.lengths[code]);
                                if candidate == 0 || length < 12 {
                                    continue;
                                }
                                let limit = length.min(data.len() - start);
                                let witness = self.starts[code];
                                let matched = common_prefix(&data[start..start + limit], &data[witness..witness + limit]);
                                if matched == length {
                                    prefix = candidate;
                                    cursor = start + length;
                                    break;
                                }
                                // Bound parent walking by bytes consumed. Otherwise the
                                // ordinary loop follows the same stored ancestors.
                                if matched >= 12 && length - matched <= matched {
                                    let mut ancestor = candidate;
                                    for _ in matched..length {
                                        ancestor = self.parents[usize::from(ancestor)];
                                    }
                                    prefix = ancestor;
                                    cursor = start + matched;
                                    break;
                                }
                            }
                        }
                        break;
                    }
                    if entry >> 12 == key {
                        prefix = (entry & 4095) as u16;
                        cursor += 1;
                        break;
                    }
                    slot = (slot + 1) & 8191;
                }
            }
            out.put(prefix, width);
            // The decoder adds one final entry before it reads EOI.
            if next >= (1 << width) - 1 && width < 12 {
                width += 1;
            }
        }
        out.put(257, width);
        if out.count != 0 {
            out.bytes.push((out.pending << (8 - out.count)) as u8);
        }
        #[cfg(test)]
        assert_eq!(
            out.bytes,
            weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(data).unwrap(),
            "writer LZW oracle"
        );
        out.bytes
    }
}

// Keep the original hash dictionary layout and call boundary independent of
// witness metadata. This path performs no candidate bookkeeping.
struct HashEncoder {
    // Packed (20-bit prefix/symbol key, 12-bit code). Code zero marks empty.
    table: Vec<u32>,
    successors: Vec<u8>,
}

impl HashEncoder {
    fn new() -> Self {
        Self { table: vec![0; 8192], successors: vec![0; 4096] }
    }

    // The plain loop retains iterator traversal without witness bookkeeping.
    // Width growth, clearing and EOI mirror Encoder::encode_chunk, with both paths
    // checked against weezl in tests. Keep LLVM from combining the two hot loops.
    #[inline(never)]
    fn encode(&mut self, data: &[u8]) -> Vec<u8> {
        self.table.fill(0);
        self.successors.fill(0);
        let mut out = Bits { bytes: Vec::with_capacity(data.len().min(4096)), pending: 0, count: 0 };
        let mut width = 9;
        let mut next = 258u16;
        out.put(256, width);
        if let Some((&first, rest)) = data.split_first() {
            let mut prefix = u16::from(first);
            for &symbol in rest {
                let key = (u32::from(prefix) << 8) | u32::from(symbol);
                let mut slot = (key.wrapping_mul(0x9e3779b1) >> 19) as usize;
                loop {
                    let entry = self.table[slot];
                    if entry == 0 {
                        // Weezl inserts entry 4096, then immediately clears; it
                        // can never be referenced and need not be stored.
                        // Compatibility: weezl 0.2.1 consumes a code but omits the
                        // seventeenth successor when promoting a sparse tree node.
                        let count = &mut self.successors[usize::from(prefix)];
                        let omitted = *count == 16;
                        *count = count.saturating_add(1);
                        if next < 4096 && !omitted {
                            self.table[slot] = (key << 12) | u32::from(next);
                        }
                        next += 1;
                        out.put(prefix, width);
                        if next >= (1 << width) && width < 12 {
                            width += 1;
                        }
                        if next > 4096 {
                            out.put(256, width);
                            self.table.fill(0);
                            self.successors.fill(0);
                            next = 258;
                            width = 9;
                        }
                        prefix = u16::from(symbol);
                        break;
                    }
                    if entry >> 12 == key {
                        prefix = (entry & 4095) as u16;
                        break;
                    }
                    slot = (slot + 1) & 8191;
                }
            }
            out.put(prefix, width);
            // The decoder adds one final entry before it reads EOI.
            if next >= (1 << width) - 1 && width < 12 {
                width += 1;
            }
        }
        out.put(257, width);
        if out.count != 0 {
            out.bytes.push((out.pending << (8 - out.count)) as u8);
        }
        #[cfg(test)]
        assert_eq!(
            out.bytes,
            weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(data).unwrap(),
            "writer LZW oracle"
        );
        out.bytes
    }
}

// Require two period-four probes at least clamp(len / 4, 32, 4096) bytes apart. A lone
// small NoData margin must not select witnesses for a dense chunk. The
// decision depends only on bytes, never timing, dictionary or thread state.
fn profitable_chunk(data: &[u8]) -> bool {
    let separation = (data.len() / 4).clamp(32, 4096);
    let mut first = None;
    let initial = data.len().min(256);
    let offsets = (0..initial.saturating_sub(31)).step_by(4).chain((256..data.len().saturating_sub(31)).step_by(4096));
    for at in offsets {
        if periodic_probe(&data[at..at + 32]) {
            match first {
                None => first = Some(at),
                Some(start) if at - start >= separation => return true,
                _ => {}
            }
        }
    }
    false
}

fn periodic_probe(bytes: &[u8]) -> bool {
    bytes[..28] == bytes[4..32]
}

// Safe unaligned word comparisons, independent of the host byte order.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let limit = a.len().min(b.len());
    let mut matched = 0;
    while limit - matched >= 8 {
        let left = u64::from_le_bytes(a[matched..matched + 8].try_into().unwrap());
        let right = u64::from_le_bytes(b[matched..matched + 8].try_into().unwrap());
        let difference = left ^ right;
        if difference != 0 {
            return matched + (difference.trailing_zeros() / 8) as usize;
        }
        matched += 8;
    }
    while matched < limit && a[matched] == b[matched] {
        matched += 1;
    }
    matched
}

struct Bits {
    bytes: Vec<u8>,
    pending: u32,
    count: u8,
}

impl Bits {
    fn put(&mut self, code: u16, width: u8) {
        self.pending = (self.pending << width) | u32::from(code);
        self.count += width;
        while self.count >= 8 {
            self.count -= 8;
            self.bytes.push((self.pending >> self.count) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_on_backoff_and_reprobe_match_oracle() {
        let mut encoder = Encoder::new();
        let mut state = 37u32;
        let noise: Vec<_> = (0..65536)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        assert!(!profitable_chunk(&noise)); // on -> backed off -> failed probes
        assert!(encoder.starts.is_empty());
        encoder.encode(&noise);
        assert!(encoder.starts.is_empty());
        let mut later = noise.clone();
        later[4352..4384].fill(0); // backed off -> successful re-probe
        assert!(!profitable_chunk(&later)); // one isolated run is insufficient
        later[8448..8480].fill(0);
        assert!(profitable_chunk(&later));
        encoder.encode(&later);
        let run = vec![0; 65536];
        assert!(profitable_chunk(&run)); // on -> immediate success
        encoder.encode(&run);
        assert_eq!(encoder.starts.len(), 4096);
        for input in [&noise, &later, &run] {
            assert_eq!(encoder.encode_chunk(input), encoder.hash.encode(input));
        }
        for size in 0..300 {
            encoder.encode(&noise[..size]);
        }
    }

    #[test]
    fn dense_with_small_nodata_margin_keeps_plain_loop() {
        let mut state = 0x12345678u32;
        let mut data = (-9999.0f32).to_le_bytes().repeat(8);
        while data.len() < 8 << 20 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            data.extend(((state % 10000) as f32 / 100.0).to_le_bytes());
        }
        assert!(!profitable_chunk(&data));
        let mut encoder = Encoder::new();
        encoder.encode(&data); // cfg(test) checks against weezl.
        assert!(encoder.starts.is_empty());
    }

    #[test]
    fn mixed_runs_copies_and_adversarial_prefixes_match_oracle() {
        let mut encoder = Encoder::new();
        for seed in 1..=512u32 {
            let mut state = seed;
            let mut random = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state
            };
            let mut data = Vec::new();
            for _ in 0..32 {
                let size = 1 + random() as usize % 4096;
                match random() % 4 {
                    0 => {
                        let period = 1 + random() as usize % 9;
                        let pattern: Vec<_> = (0..period).map(|_| random() as u8).collect();
                        data.extend((0..size).map(|i| pattern[i % period]));
                    }
                    1 => data.extend((0..size).map(|_| random() as u8)),
                    2 => {
                        let phase = random() as usize % 4;
                        let nodata = (-9999.0f32).to_le_bytes();
                        data.extend((0..size).map(|i| nodata[(i + phase) % 4]));
                    }
                    _ if !data.is_empty() => {
                        let from = random() as usize % data.len();
                        data.extend_from_within(from..(from + size).min(data.len()));
                    }
                    _ => {}
                }
            }
            let encoded = encoder.encode(&data); // Every call asserts the weezl oracle.
            assert_eq!(
                weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).decode(&encoded).unwrap(),
                data,
                "seed {seed}"
            );
        }
        for period in 1..=9 {
            for phase in 0..4 {
                let mut data: Vec<_> = (0..phase).map(|i| i as u8).collect();
                data.extend((0..65537).map(|i| (i % period) as u8));
                // Long shared prefixes with different tails force failed candidates
                // and ancestor recovery, including truncation at the end of input.
                for tail in 0..32u8 {
                    data.extend_from_within(phase..phase + 4096);
                    data.push(tail);
                }
                encoder.encode(&data);
            }
        }
    }

    #[test]
    fn sparse_strips_do_not_retain_raw_sized_output_buffers() {
        let raw = vec![0; 217724];
        let out = Encoder::new().encode(&raw);
        assert!(out.capacity() < raw.len() / 8);
    }

    #[test]
    fn randomized_streams_and_sparse_node_promotion_match() {
        let mut enc = Encoder::new();
        for seed in 1..=64u32 {
            let mut rng = seed;
            let mut data = Vec::new();
            // Repeated prefix zero with many successors explicitly exercises
            // weezl's omitted seventeenth edge, then later encounters it again.
            for _ in 0..3 {
                for b in 1..=255u8 {
                    data.extend([0, b]);
                }
            }
            for _ in 0..(seed as usize * 1733) {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                data.push(rng as u8);
            }
            let actual = enc.encode(&data);
            let expected = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(&data).unwrap();
            assert!(actual == expected, "seed {seed}");
            assert_eq!(weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).decode(&actual).unwrap(), data);
        }
    }

    #[test]
    fn matches_weezl_and_decodes_across_widths_resets_and_chunk_boundaries() {
        let mut encoder = Encoder::new();
        let mut rng = 0x12345678u32;
        let random: Vec<u8> = (0..1_000_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                rng as u8
            })
            .collect();
        let patterns = [random, vec![0; 1_000_000], vec![255; 1_000_000], (0..1_000_000).map(|i| (i % 251) as u8).collect()];
        for data in &patterns {
            // All short lengths catch EOI at width changes; larger boundaries
            // cover typical strip sizes and repeated dictionary saturation.
            for n in (0..4096).chain([8191, 8192, 8193, 65535, 65536, 65537, 217723, 217724, 217725, 1_000_000]) {
                let input = &data[..n];
                let expected = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(input).unwrap();
                let actual = encoder.encode(input);
                assert!(actual == expected, "length {n}, first difference {:?}", actual.iter().zip(&expected).position(|(a, b)| a != b));
                assert_eq!(weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).decode(&actual).unwrap(), input);
            }
        }
    }
}
