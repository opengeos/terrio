// SPDX-FileCopyrightText: Copyright 2016 Eric Biggers
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   libdeflate v1.26:
//     lib/deflate_compress.c
//     lib/hc_matchfinder.h
//     lib/matchfinder_common.h
//     lib/deflate_constants.h
//     lib/zlib_compress.c

//! A zlib (RFC 1950) / Deflate (RFC 1951) compressor: libdeflate's level 6, giving the same bytes.
//! Lazy parsing with the hash-chains matchfinder (max_search_depth 35, nice_match_length 65),
//! libdeflate's block splitting, length-limited Huffman codes and the stored / static / dynamic
//! block choice. GDAL (3.12) built with libdeflate compresses GeoTIFF Deflate chunks at this level.
//!
//! The other levels, 1 to 9, change the parser and how far the matchfinder searches, with the
//! parameters of libdeflate's `libdeflate_alloc_compressor_ex()` (see [`Level`]).

use std::cell::Cell;
use std::hint::select_unpredictable;

/// Marks the branch calling it as unlikely (`std::hint::cold_path`, stable only since Rust 1.95).
#[cold]
#[inline(never)]
fn cold_path() {}

const MIN_BLOCK_LENGTH: usize = 5000;
const SOFT_MAX_BLOCK_LENGTH: usize = 300000;
const SEQ_STORE_LENGTH: usize = 50000;
const MAX_LITLEN_CODEWORD_LEN: u32 = 14;
const MAX_OFFSET_CODEWORD_LEN: u32 = 15;
const MAX_PRE_CODEWORD_LEN: u32 = 7;

const MIN_MATCH_LEN: u32 = 3;
const MAX_MATCH_LEN: u32 = 258;
const NUM_LITLEN_SYMS: usize = 288;
const NUM_OFFSET_SYMS: usize = 32;
const NUM_PRECODE_SYMS: usize = 19;
const END_OF_BLOCK: usize = 256;
const FIRST_LEN_SYM: usize = 257;

/// The parser of a compression level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Parser {
    /// The longest match at each position (deflate_compress_greedy()).
    Greedy,
    /// A match is kept only if the next position has no better one (deflate_compress_lazy()).
    Lazy,
    /// As `Lazy`, looking two positions ahead (deflate_compress_lazy2()).
    Lazy2,
}

/// How a compression level searches: its parser, how many hash-chain candidates the matchfinder
/// examines at a position, and the match length that ends the search. Levels 2 to 9 take the
/// parsers and parameters of libdeflate's `libdeflate_alloc_compressor_ex()`. Its level 1 is
/// another compressor (a hash table of two entries per bucket, blocks of fixed length); here
/// level 1 is the greedy parser over the hash chains, examining two candidates, with that
/// level's nice match length (32).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Level {
    level: u32,
    parser: Parser,
    max_search_depth: u32,
    nice_match_length: u32,
}

impl Level {
    /// The default level, 6.
    pub(crate) const DEFAULT: Self = Self { level: 6, parser: Parser::Lazy, max_search_depth: 35, nice_match_length: 65 };

    /// Level `level`, from 1 (the least search) to 9 (the most); `None` for another number.
    pub(crate) fn new(level: u32) -> Option<Self> {
        let (parser, max_search_depth, nice_match_length) = match level {
            1 => (Parser::Greedy, 2, 32),
            2 => (Parser::Greedy, 6, 10),
            3 => (Parser::Greedy, 12, 14),
            4 => (Parser::Greedy, 16, 30),
            5 => (Parser::Lazy, 16, 30),
            6 => return Some(Self::DEFAULT),
            7 => (Parser::Lazy, 100, 130),
            8 => (Parser::Lazy2, 300, MAX_MATCH_LEN),
            9 => (Parser::Lazy2, 600, MAX_MATCH_LEN),
            _ => return None,
        };
        Some(Self { level, parser, max_search_depth, nice_match_length })
    }

    /// The largest input written as a stored block without trying to compress it: the higher the
    /// level, the smaller the inputs it compresses.
    fn max_passthrough_size(self) -> usize {
        55 - 4 * self.level as usize
    }

    /// The zlib header (CMF and FLG): a 32 KiB Deflate window, and the level's FLEVEL (fastest,
    /// fast, default or slowest), with FCHECK making it a multiple of 31 (RFC 1950, section 2.2).
    fn zlib_header(self) -> [u8; 2] {
        let flevel = match self.level {
            0..2 => 0,
            2..6 => 1,
            6..8 => 2,
            _ => 3,
        };
        let hdr: u16 = 0x7800 | flevel << 6;
        (hdr | (31 - hdr % 31)).to_be_bytes()
    }
}

const LENGTH_SLOT_BASE: [u32; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const EXTRA_LENGTH_BITS: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const OFFSET_SLOT_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385,
    24577,
];
const EXTRA_OFFSET_BITS: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const PRECODE_LENS_PERMUTATION: [u8; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
const EXTRA_PRECODE_BITS: [u8; 19] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];

/// length => length slot (deflate_length_slot[]).
const LENGTH_SLOT: [u8; 259] = {
    let mut t = [0u8; 259];
    let mut slot = 0;
    let mut len = 3;
    while len <= 258 {
        while slot + 1 < 29 && LENGTH_SLOT_BASE[slot + 1] <= len as u32 {
            slot += 1;
        }
        t[len] = slot as u8;
        len += 1;
    }
    t
};

/// offset - 1 => offset slot, for offset <= 256 (deflate_offset_slot[]).
const OFFSET_SLOT: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut slot = 0;
    let mut i = 0;
    while i < 256 {
        while slot + 1 < 30 && OFFSET_SLOT_BASE[slot + 1] <= i as u32 + 1 {
            slot += 1;
        }
        t[i] = slot as u8;
        i += 1;
    }
    t
};

#[inline(always)]
fn offset_slot(offset: u32) -> usize {
    let n = 256u32.wrapping_sub(offset) >> 29;
    (OFFSET_SLOT[((offset - 1) >> n) as usize & 255] as u32 + (n << 1)) as usize
}

#[inline(always)]
fn bsr32(v: u32) -> i32 {
    31 - v.leading_zeros() as i32
}

/******************************** Huffman codes *******************************/

const NUM_SYMBOL_BITS: u32 = 10;
const SYMBOL_MASK: u32 = (1 << NUM_SYMBOL_BITS) - 1;
const FREQ_MASK: u32 = !SYMBOL_MASK;

/// Codewords and lengths; litlen in [0, 288), offset in [288, 320) so that the
/// lengths are contiguous like `struct deflate_lens`.
#[derive(Clone)]
struct Codes {
    cw: [u32; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS],
    lens: [u8; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS],
}

/// Sorts the used symbols by (frequency, symbol). libdeflate uses a counting
/// sort plus heapsort; entries are distinct, so any sort gives the same order.
fn sort_symbols(freqs: &[u32], lens: &mut [u8], symout: &mut [u32]) -> usize {
    let mut n = 0;
    for (sym, &f) in freqs.iter().enumerate() {
        if f != 0 {
            symout[n] = sym as u32 | (f << NUM_SYMBOL_BITS);
            n += 1;
        } else {
            lens[sym] = 0;
        }
    }
    symout[..n].sort_unstable();
    n
}

fn build_tree(a: &mut [u32], sym_count: usize) {
    let last_idx = sym_count - 1;
    let (mut i, mut b, mut e) = (0usize, 0usize, 0usize);
    loop {
        let new_freq;
        if i < last_idx && (b == e || (a[i + 1] & FREQ_MASK) <= (a[b] & FREQ_MASK)) {
            new_freq = (a[i] & FREQ_MASK) + (a[i + 1] & FREQ_MASK);
            i += 2;
        } else if b + 2 <= e && (i > last_idx || (a[b + 1] & FREQ_MASK) < (a[i] & FREQ_MASK)) {
            new_freq = (a[b] & FREQ_MASK) + (a[b + 1] & FREQ_MASK);
            a[b] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b] & SYMBOL_MASK);
            a[b + 1] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b + 1] & SYMBOL_MASK);
            b += 2;
        } else {
            new_freq = (a[i] & FREQ_MASK) + (a[b] & FREQ_MASK);
            a[b] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b] & SYMBOL_MASK);
            i += 1;
            b += 1;
        }
        a[e] = new_freq | (a[e] & SYMBOL_MASK);
        e += 1;
        if e >= last_idx {
            break;
        }
    }
}

fn compute_length_counts(a: &mut [u32], root_idx: usize, len_counts: &mut [u32; 16], max_len: u32) {
    len_counts.fill(0);
    len_counts[1] = 2;
    a[root_idx] &= SYMBOL_MASK;
    for node in (0..root_idx).rev() {
        let parent = (a[node] >> NUM_SYMBOL_BITS) as usize;
        let parent_depth = a[parent] >> NUM_SYMBOL_BITS;
        let mut depth = parent_depth + 1;
        a[node] = (a[node] & SYMBOL_MASK) | (depth << NUM_SYMBOL_BITS);
        if depth >= max_len {
            depth = max_len;
            loop {
                depth -= 1;
                if len_counts[depth as usize] != 0 {
                    break;
                }
            }
        }
        len_counts[depth as usize] -= 1;
        len_counts[depth as usize + 1] += 2;
    }
}

fn gen_codewords(a: &mut [u32], lens: &mut [u8], len_counts: &[u32; 16], max_len: u32) {
    let mut i = 0;
    for len in (1..=max_len).rev() {
        for _ in 0..len_counts[len as usize] {
            lens[(a[i] & SYMBOL_MASK) as usize] = len as u8;
            i += 1;
        }
    }
    let mut next = [0u32; 16];
    for len in 2..=max_len as usize {
        next[len] = (next[len - 1] + len_counts[len - 1]) << 1;
    }
    for sym in 0..lens.len() {
        let len = lens[sym] as u32;
        a[sym] = next[len as usize].reverse_bits() >> ((32 - len) & 31);
        next[len as usize] += 1;
    }
}

/// deflate_make_huffman_code(): length-limited canonical Huffman code.
fn make_huffman_code(max_len: u32, freqs: &[u32], lens: &mut [u8], codewords: &mut [u32]) {
    let num_used = sort_symbols(freqs, lens, codewords);
    if num_used < 2 {
        let sym = if num_used != 0 { (codewords[0] & SYMBOL_MASK) as usize } else { 0 };
        let nonzero_idx = if sym != 0 { sym } else { 1 };
        codewords[0] = 0;
        lens[0] = 1;
        codewords[nonzero_idx] = 1;
        lens[nonzero_idx] = 1;
        return;
    }
    build_tree(codewords, num_used);
    let mut len_counts = [0u32; 16];
    compute_length_counts(codewords, num_used - 2, &mut len_counts, max_len);
    gen_codewords(codewords, lens, &len_counts, max_len);
}

#[derive(Clone)]
struct Freqs {
    litlen: [u32; NUM_LITLEN_SYMS],
    offset: [u32; NUM_OFFSET_SYMS],
}

fn make_huffman_codes(freqs: &Freqs, codes: &mut Codes) {
    let (ll_lens, off_lens) = codes.lens.split_at_mut(NUM_LITLEN_SYMS);
    let (ll_cw, off_cw) = codes.cw.split_at_mut(NUM_LITLEN_SYMS);
    make_huffman_code(MAX_LITLEN_CODEWORD_LEN, &freqs.litlen, ll_lens, ll_cw);
    make_huffman_code(MAX_OFFSET_CODEWORD_LEN, &freqs.offset, off_lens, off_cw);
}

fn static_codes() -> Codes {
    let mut f = Freqs { litlen: [0; NUM_LITLEN_SYMS], offset: [1; NUM_OFFSET_SYMS] };
    // 1 << (9 - codeword length) for the RFC 1951 static lengths 8, 9, 7, 8.
    for (i, x) in f.litlen.iter_mut().enumerate() {
        *x = match i {
            0..144 => 2,
            144..256 => 1,
            256..280 => 4,
            _ => 2,
        };
    }
    let mut c = Codes { cw: [0; 320], lens: [0; 320] };
    make_huffman_codes(&f, &mut c);
    c
}

/********************************** Precode ***********************************/

struct Precode {
    freqs: [u32; NUM_PRECODE_SYMS],
    cw: [u32; NUM_PRECODE_SYMS],
    lens: [u8; NUM_PRECODE_SYMS],
    items: [u32; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS],
    num_litlen_syms: usize,
    num_offset_syms: usize,
    num_explicit_lens: usize,
    num_items: usize,
}

fn compute_precode_items(lens: &[u8], freqs: &mut [u32; 19], items: &mut [u32]) -> usize {
    freqs.fill(0);
    let num_lens = lens.len();
    let mut n = 0;
    let mut run_start = 0;
    loop {
        let len = lens[run_start];
        let mut run_end = run_start;
        loop {
            run_end += 1;
            if run_end == num_lens || len != lens[run_end] {
                break;
            }
        }
        if len == 0 {
            while run_end - run_start >= 11 {
                let extra = ((run_end - run_start) - 11).min(0x7F);
                freqs[18] += 1;
                items[n] = 18 | (extra as u32) << 5;
                n += 1;
                run_start += 11 + extra;
            }
            if run_end - run_start >= 3 {
                let extra = ((run_end - run_start) - 3).min(0x7);
                freqs[17] += 1;
                items[n] = 17 | (extra as u32) << 5;
                n += 1;
                run_start += 3 + extra;
            }
        } else if run_end - run_start >= 4 {
            freqs[len as usize] += 1;
            items[n] = len as u32;
            n += 1;
            run_start += 1;
            loop {
                let extra = ((run_end - run_start) - 3).min(0x3);
                freqs[16] += 1;
                items[n] = 16 | (extra as u32) << 5;
                n += 1;
                run_start += 3 + extra;
                if run_end - run_start < 3 {
                    break;
                }
            }
        }
        while run_start != run_end {
            freqs[len as usize] += 1;
            items[n] = len as u32;
            n += 1;
            run_start += 1;
        }
        if run_start == num_lens {
            return n;
        }
    }
}

fn precompute_huffman_header(codes: &mut Codes, p: &mut Precode) {
    let lens = &mut codes.lens;
    p.num_litlen_syms = NUM_LITLEN_SYMS;
    while p.num_litlen_syms > 257 && lens[p.num_litlen_syms - 1] == 0 {
        p.num_litlen_syms -= 1;
    }
    p.num_offset_syms = NUM_OFFSET_SYMS;
    while p.num_offset_syms > 1 && lens[NUM_LITLEN_SYMS + p.num_offset_syms - 1] == 0 {
        p.num_offset_syms -= 1;
    }
    let (nl, no) = (p.num_litlen_syms, p.num_offset_syms);
    if nl != NUM_LITLEN_SYMS {
        lens.copy_within(NUM_LITLEN_SYMS..NUM_LITLEN_SYMS + no, nl);
    }
    p.num_items = compute_precode_items(&lens[..nl + no], &mut p.freqs, &mut p.items);
    make_huffman_code(MAX_PRE_CODEWORD_LEN, &p.freqs, &mut p.lens, &mut p.cw);
    p.num_explicit_lens = NUM_PRECODE_SYMS;
    while p.num_explicit_lens > 4 && p.lens[PRECODE_LENS_PERMUTATION[p.num_explicit_lens - 1] as usize] == 0 {
        p.num_explicit_lens -= 1;
    }
    if nl != NUM_LITLEN_SYMS {
        lens.copy_within(nl..nl + no, NUM_LITLEN_SYMS);
    }
}

/******************************** Bit writer **********************************/

/// The output buffer is grown before each block to hold the block plus 8 bytes
/// of slack, so a whole word can always be stored at `pos`.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    pos: usize,
    bitbuf: u64,
    bitcount: u32,
}

impl BitWriter<'_> {
    #[inline(always)]
    fn add(&mut self, bits: u32, n: u32) {
        self.bitbuf |= (bits as u64) << self.bitcount;
        self.bitcount += n;
    }

    #[inline(always)]
    fn flush(&mut self) {
        self.out[self.pos..self.pos + 8].copy_from_slice(&self.bitbuf.to_le_bytes());
        self.bitbuf >>= self.bitcount & !7;
        self.pos += (self.bitcount >> 3) as usize;
        self.bitcount &= 7;
    }
}

/***************************** Block split stats ******************************/

const NUM_LITERAL_OBSERVATION_TYPES: usize = 8;
const NUM_OBSERVATION_TYPES: usize = 10;
const NUM_OBSERVATIONS_PER_BLOCK_CHECK: u32 = 512;

#[derive(Default)]
struct SplitStats {
    new_observations: [u32; NUM_OBSERVATION_TYPES],
    observations: [u32; NUM_OBSERVATION_TYPES],
    num_new_observations: u32,
    num_observations: u32,
}

impl SplitStats {
    #[inline(always)]
    fn observe_literal(&mut self, lit: u8) {
        self.new_observations[(((lit >> 5) & 0x6) | (lit & 1)) as usize] += 1;
        self.num_new_observations += 1;
    }

    #[inline(always)]
    fn observe_match(&mut self, length: u32) {
        self.new_observations[NUM_LITERAL_OBSERVATION_TYPES + (length >= 9) as usize] += 1;
        self.num_new_observations += 1;
    }

    fn merge_new_observations(&mut self) {
        for i in 0..NUM_OBSERVATION_TYPES {
            self.observations[i] += self.new_observations[i];
            self.new_observations[i] = 0;
        }
        self.num_observations += self.num_new_observations;
        self.num_new_observations = 0;
    }

    #[inline(never)]
    fn do_end_block_check(&mut self, block_length: u32) -> bool {
        if self.num_observations > 0 {
            let mut total_delta = 0u32;
            for i in 0..NUM_OBSERVATION_TYPES {
                let expected = self.observations[i] * self.num_new_observations;
                let actual = self.new_observations[i] * self.num_observations;
                total_delta += actual.abs_diff(expected);
            }
            let num_items = self.num_observations + self.num_new_observations;
            let mut cutoff = self.num_new_observations * 200 / 512 * self.num_observations;
            if block_length < 10000 && num_items < 8192 {
                cutoff += (cutoff as u64 * (8192 - num_items) as u64 / 8192) as u32;
            }
            if total_delta + (block_length / 4096) * self.num_observations >= cutoff {
                return true;
            }
        }
        self.merge_new_observations();
        false
    }

    #[inline(always)]
    fn should_end_block(&mut self, block_begin: usize, in_next: usize, in_end: usize) -> bool {
        if self.num_new_observations < NUM_OBSERVATIONS_PER_BLOCK_CHECK
            || in_next - block_begin < MIN_BLOCK_LENGTH
            || in_end - in_next < MIN_BLOCK_LENGTH
        {
            return false;
        }
        self.do_end_block_check((in_next - block_begin) as u32)
    }
}

/********************************* Compressor *********************************/

/// A literal run followed by a match (length 0 = end of block).
#[derive(Clone, Copy, Default, Debug)]
struct Sequence {
    litrunlen_and_length: u32,
    offset: u16,
    offset_slot: u16,
}
const SEQ_LENGTH_SHIFT: u32 = 23;
const SEQ_LITRUNLEN_MASK: u32 = (1 << SEQ_LENGTH_SHIFT) - 1;

struct Compressor {
    /// Literals since the last match, and the number of sequences, of the
    /// current block (kept in memory, like libdeflate's `seq`).
    litrunlen: u32,
    num_seqs: usize,
    freqs: Freqs,
    split_stats: SplitStats,
    codes: Codes,
    static_codes: Codes,
    precode: Precode,
}

impl Compressor {
    #[inline(always)]
    fn choose_literal(&mut self, literal: u8) {
        self.freqs.litlen[literal as usize] += 1;
        self.split_stats.observe_literal(literal);
        self.litrunlen += 1;
    }

    #[inline(always)]
    fn choose_match(&mut self, length: u32, offset: u32, seqs: &mut Vec<Sequence>) {
        let length_slot = LENGTH_SLOT[length as usize] as usize;
        let offset_slot = offset_slot(offset);
        self.freqs.litlen[FIRST_LEN_SYM + length_slot] += 1;
        self.freqs.offset[offset_slot] += 1;
        self.split_stats.observe_match(length);
        seqs.push(Sequence {
            litrunlen_and_length: self.litrunlen | length << SEQ_LENGTH_SHIFT,
            offset: offset as u16,
            offset_slot: offset_slot as u16,
        });
        self.num_seqs += 1;
        self.litrunlen = 0;
    }

    fn finish_block<'a>(&mut self, w: BitWriter<'a>, block: &[u8], seqs: &[Sequence], is_final: bool) -> BitWriter<'a> {
        self.freqs.litlen[END_OF_BLOCK] += 1;
        make_huffman_codes(&self.freqs, &mut self.codes);
        self.flush_block(w, block, seqs, is_final)
    }

    /// deflate_flush_block(): choose the cheapest of dynamic, static and
    /// stored, then write it.
    /// The writer is passed by value so that its state stays in registers.
    fn flush_block<'a>(&mut self, mut w: BitWriter<'a>, block: &[u8], seqs: &[Sequence], is_final: bool) -> BitWriter<'a> {
        let block_length = block.len();
        let mut dynamic_cost = 3u32;
        let mut static_cost = 3u32;
        let mut uncompressed_cost = 3u32;

        precompute_huffman_header(&mut self.codes, &mut self.precode);
        let p = &self.precode;
        let f = &self.freqs;
        let lens = &self.codes.lens;

        dynamic_cost += 5 + 5 + 4 + 3 * p.num_explicit_lens as u32;
        for ((&freq, &len), &extra) in p.freqs.iter().zip(&p.lens).zip(&EXTRA_PRECODE_BITS) {
            dynamic_cost += freq * (extra as u32 + len as u32);
        }
        for (sym, (&freq, &len)) in f.litlen[..256].iter().zip(&lens[..256]).enumerate() {
            dynamic_cost += freq * len as u32;
            static_cost += freq * if sym < 144 { 8 } else { 9 };
        }
        dynamic_cost += lens[END_OF_BLOCK] as u32;
        static_cost += 7;
        for (i, &extra) in EXTRA_LENGTH_BITS.iter().enumerate() {
            let sym = FIRST_LEN_SYM + i;
            dynamic_cost += f.litlen[sym] * (extra as u32 + lens[sym] as u32);
            static_cost += f.litlen[sym] * (extra as u32 + self.static_codes.lens[sym] as u32);
        }
        for (sym, &extra) in EXTRA_OFFSET_BITS.iter().enumerate() {
            dynamic_cost += f.offset[sym] * (extra as u32 + lens[NUM_LITLEN_SYMS + sym] as u32);
            static_cost += f.offset[sym] * (extra as u32 + 5);
        }
        uncompressed_cost +=
            ((w.bitcount + 3).wrapping_neg() & 7) + 32 + 40 * (block_length.div_ceil(65535) as u32 - 1) + 8 * block_length as u32;

        let best_cost = dynamic_cost.min(static_cost.min(uncompressed_cost));
        // Grow the output to the chosen block's size only: a buffer of the whole compress bound
        // would be zeroed, and its pages touched, on every call.
        let end = w.pos + (w.bitcount + best_cost).div_ceil(8) as usize + 8;
        if w.out.len() < end {
            w.out.resize(end, 0);
        }

        if best_cost == uncompressed_cost {
            let mut in_next = 0;
            loop {
                let mut bfinal = 0u8;
                let mut len = 65535;
                if block_length - in_next <= 65535 {
                    bfinal = is_final as u8;
                    len = block_length - in_next;
                }
                w.out[w.pos] = (bfinal << w.bitcount) | w.bitbuf as u8;
                w.pos += 1;
                if w.bitcount > 5 {
                    w.out[w.pos] = 0;
                    w.pos += 1;
                }
                w.bitbuf = 0;
                w.bitcount = 0;
                w.out[w.pos..w.pos + 2].copy_from_slice(&(len as u16).to_le_bytes());
                w.out[w.pos + 2..w.pos + 4].copy_from_slice(&(!(len as u16)).to_le_bytes());
                w.pos += 4;
                w.out[w.pos..w.pos + len].copy_from_slice(&block[in_next..in_next + len]);
                w.pos += len;
                in_next += len;
                if in_next == block_length {
                    return w;
                }
            }
        }

        let codes = if best_cost == static_cost {
            w.add(is_final as u32, 1);
            w.add(1, 2);
            w.flush();
            &self.static_codes
        } else {
            w.add(is_final as u32, 1);
            w.add(2, 2);
            w.add(p.num_litlen_syms as u32 - 257, 5);
            w.add(p.num_offset_syms as u32 - 1, 5);
            w.add(p.num_explicit_lens as u32 - 4, 4);
            w.add(p.lens[PRECODE_LENS_PERMUTATION[0] as usize] as u32, 3);
            w.flush();
            for &sym in &PRECODE_LENS_PERMUTATION[1..p.num_explicit_lens] {
                w.add(p.lens[sym as usize] as u32, 3);
            }
            w.flush();
            for &item in &p.items[..p.num_items] {
                let sym = (item & 0x1F) as usize;
                w.add(p.cw[sym], p.lens[sym] as u32);
                w.add(item >> 5, EXTRA_PRECODE_BITS[sym] as u32);
                w.flush();
            }
            &self.codes
        };

        // deflate_compute_full_len_codewords()
        let mut len_cw = [0u32; 259];
        let mut len_lens = [0u8; 259];
        for len in MIN_MATCH_LEN as usize..=MAX_MATCH_LEN as usize {
            let slot = LENGTH_SLOT[len] as usize;
            let sym = FIRST_LEN_SYM + slot;
            let extra = len as u32 - LENGTH_SLOT_BASE[slot];
            len_cw[len] = codes.cw[sym] | (extra << codes.lens[sym]);
            len_lens[len] = codes.lens[sym] + EXTRA_LENGTH_BITS[slot];
        }

        let ll_cw = &codes.cw[..256];
        let ll_lens = &codes.lens[..256];
        let mut in_next = 0;
        for seq in seqs {
            let litrunlen = (seq.litrunlen_and_length & SEQ_LITRUNLEN_MASK) as usize;
            let length = seq.litrunlen_and_length >> SEQ_LENGTH_SHIFT;
            let lits = &block[in_next..in_next + litrunlen];
            in_next += litrunlen;
            // Up to 4 literals (4 * 14 bits) between flushes.
            let (quads, rest) = lits.as_chunks::<4>();
            for quad in quads {
                for &lit in quad {
                    w.add(ll_cw[lit as usize], ll_lens[lit as usize] as u32);
                }
                w.flush();
            }
            if !rest.is_empty() {
                for &lit in rest {
                    w.add(ll_cw[lit as usize], ll_lens[lit as usize] as u32);
                }
                w.flush();
            }
            if length == 0 {
                break;
            }
            // WRITE_MATCH
            let os = seq.offset_slot as usize;
            w.add(len_cw[length as usize], len_lens[length as usize] as u32);
            w.add(codes.cw[NUM_LITLEN_SYMS + os], codes.lens[NUM_LITLEN_SYMS + os] as u32);
            w.add(seq.offset as u32 - OFFSET_SLOT_BASE[os], EXTRA_OFFSET_BITS[os] as u32);
            w.flush();
            in_next += length as usize;
        }
        debug_assert_eq!(in_next, block_length);
        w.add(codes.cw[END_OF_BLOCK], codes.lens[END_OF_BLOCK] as u32);
        w.flush();
        w
    }
}

/***************************** hc_matchfinder *********************************/

const WINDOW_SIZE: u32 = 32768;
const WINDOW_MASK: usize = WINDOW_SIZE as usize - 1;
const HASH3_ORDER: u32 = 15;
const HASH4_ORDER: u32 = 16;
const MF_INITVAL: i16 = i16::MIN;
// One array holds hash3_tab, hash4_tab and next_tab, like struct hc_matchfinder.
const HASH3_TAB: usize = 0;
const HASH4_TAB: usize = 1 << HASH3_ORDER;
const NEXT_TAB: usize = HASH4_TAB + (1 << HASH4_ORDER);
const MF_SIZE: usize = NEXT_TAB + WINDOW_SIZE as usize;
type Matchfinder = [i16; MF_SIZE];

#[inline(always)]
fn lz_hash(seq: u32, num_bits: u32) -> usize {
    (seq.wrapping_mul(0x1E35A7BD) >> (32 - num_bits)) as usize
}

/// Loads 4 bytes at `p`.
///
/// # Safety
/// `p..p + 4` is in bounds of one allocation.
#[inline(always)]
unsafe fn load_u32_at(p: *const u8) -> u32 {
    // SAFETY: the caller guarantees `p..p + 4` is readable.
    u32::from_le(unsafe { p.cast::<u32>().read_unaligned() })
}

#[inline(always)]
fn prefetchw(mf: &Matchfinder, i: usize) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch is only a hint and never faults.
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(mf.as_ptr().wrapping_add(i) as *const i8);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (mf, i);
}

/// matchfinder_rebase(): subtract 32768 from every entry, saturating.
fn slide_window(mf: &mut Matchfinder) {
    for x in mf.iter_mut() {
        *x = i16::MIN | (*x & !(*x >> 15));
    }
}

/// lz_extend(): number of bytes at which `a` and `b` agree, counting from
/// `len` (already known to agree) up to `max_len`.
///
/// # Safety
/// `a..a + max_len` and `b..b + max_len` are readable.
#[inline(always)]
unsafe fn lz_extend(a: *const u8, b: *const u8, mut len: u32, max_len: u32) -> u32 {
    while len + 8 <= max_len {
        // SAFETY: len + 8 <= max_len.
        let (x, y) = unsafe {
            let x = a.add(len as usize).cast::<u64>().read_unaligned();
            (x, b.add(len as usize).cast::<u64>().read_unaligned())
        };
        let v = u64::from_le(x) ^ u64::from_le(y);
        if v != 0 {
            return len + (v.trailing_zeros() >> 3);
        }
        len += 8;
    }
    // SAFETY: len < max_len.
    while len < max_len && unsafe { *a.add(len as usize) == *b.add(len as usize) } {
        len += 1;
    }
    len
}

/// Position-relative matchfinder state: `base` is the input index that the
/// i16 positions are relative to, `next_hashes` the hashes for the next byte.
struct Mf<'t> {
    tab: &'t mut Matchfinder,
    base: usize,
    next_hashes: [usize; 2],
}

impl Mf<'_> {
    /// hc_matchfinder_longest_match(): returns (length, offset); length is
    /// `best_len` and offset 0 if nothing longer than `best_len` was found.
    ///
    /// # Safety
    /// `in_next + max_len <= s.len()` and `nice_len <= max_len`: the input
    /// loads are unchecked.
    #[inline(always)]
    unsafe fn longest_match(
        &mut self,
        s: &[u8],
        in_next: usize,
        mut best_len: u32,
        max_len: u32,
        nice_len: u32,
        max_search_depth: u32,
    ) -> (u32, u32) {
        debug_assert!(in_next + max_len as usize <= s.len() && nice_len <= max_len);
        let mut depth_remaining = max_search_depth;
        let mut cur_pos = (in_next - self.base) as u32;
        if cur_pos == WINDOW_SIZE {
            cold_path();
            slide_window(self.tab);
            self.base += WINDOW_SIZE as usize;
            cur_pos = 0;
        }
        let cutoff = (cur_pos as i32 - WINDOW_SIZE as i32) as i16;
        let mf = &mut *self.tab;
        let next = |mf: &Matchfinder, node: i16| mf[NEXT_TAB + (node as usize & WINDOW_MASK)];
        // Like libdeflate, work with pointers: `ip` is s[in_next], and `win + node`
        // is the input byte at matchfinder position `node`. Matchfinder invariant:
        // every node > cutoff is the position of an earlier input byte that was
        // inserted, so `win + node` is in [ip - 32767, ip). Chains only link to
        // older positions: next_tab[p] gets the bucket head from before p was
        // inserted, and the current position is inserted after reading the heads.
        let ip = s.as_ptr().wrapping_add(in_next);
        let win = s.as_ptr().wrapping_add(self.base);
        let at = |node: i16| {
            let p = win.wrapping_offset(node as isize);
            debug_assert!(p < ip && ip as usize - p as usize <= 32767);
            p
        };
        let mut best_match = ip;

        'out: {
            if max_len < 5 {
                break 'out;
            }
            let hash3 = self.next_hashes[0] & ((1 << HASH3_ORDER) - 1);
            let hash4 = self.next_hashes[1] & ((1 << HASH4_ORDER) - 1);
            let cur_node3 = mf[HASH3_TAB + hash3];
            let mut cur_node4 = mf[HASH4_TAB + hash4];
            mf[HASH3_TAB + hash3] = cur_pos as i16;
            mf[HASH4_TAB + hash4] = cur_pos as i16;
            mf[NEXT_TAB + (cur_pos as usize & WINDOW_MASK)] = cur_node4;

            // SAFETY (all loads at ip): in_next + 5 <= in_next + max_len <= s.len().
            let next_hashseq = unsafe { load_u32_at(ip.add(1)) };
            self.next_hashes = [lz_hash(next_hashseq & 0xFFFFFF, HASH3_ORDER), lz_hash(next_hashseq, HASH4_ORDER)];
            prefetchw(mf, HASH3_TAB + self.next_hashes[0]);
            prefetchw(mf, HASH4_TAB + self.next_hashes[1]);
            // SAFETY: as above.
            let head = unsafe { load_u32_at(ip) };

            if best_len < 4 {
                // libdeflate exits if cur_node3 is out of the window, else checks for a
                // length-3 match, then exits if cur_node4 is out of the window. Both
                // tests are data-dependent branches that often mispredict, so this does
                // the length-3 check without branching and the two tests in one branch;
                // the result is the same.
                let valid3 = cur_node3 > cutoff;
                if best_len < 3 {
                    // ip stands in for an invalid cur_node3: any readable bytes do.
                    let p = select_unpredictable(valid3, win.wrapping_offset(cur_node3 as isize), ip);
                    // SAFETY: p is in [ip - 32767, ip], and in_next + 5 <= s.len().
                    let v = unsafe { load_u32_at(p) };
                    let got3 = valid3 & ((v ^ head) & 0xFFFFFF == 0);
                    best_len = select_unpredictable(got3, 3, best_len);
                    best_match = select_unpredictable(got3, p, best_match);
                }
                // One compare for both: !valid3 || cur_node4 <= cutoff.
                if cur_node3.min(cur_node4) <= cutoff {
                    break 'out;
                }
                loop {
                    // SAFETY: at(cur_node4) is in [ip - 32767, ip), and in_next + 5 <= s.len().
                    if unsafe { load_u32_at(at(cur_node4)) } == head {
                        break;
                    }
                    cur_node4 = next(mf, cur_node4);
                    depth_remaining -= 1;
                    if depth_remaining == 0 || cur_node4 <= cutoff {
                        break 'out;
                    }
                }
                best_match = at(cur_node4);
                // SAFETY: best_match < ip, and in_next + max_len <= s.len().
                best_len = unsafe { lz_extend(ip, best_match, 4, max_len) };
                if best_len >= nice_len {
                    break 'out;
                }
                cur_node4 = next(mf, cur_node4);
                depth_remaining -= 1;
                if depth_remaining == 0 || cur_node4 <= cutoff {
                    break 'out;
                }
            } else if cur_node4 <= cutoff || best_len >= nice_len {
                break 'out;
            }

            loop {
                let t = best_len as usize - 3;
                // SAFETY: best_len < nice_len <= max_len, so
                // in_next + t + 4 = in_next + best_len + 1 <= in_next + max_len <= s.len().
                let tail = unsafe { load_u32_at(ip.add(t)) };
                let mut p;
                loop {
                    p = at(cur_node4);
                    // SAFETY: p < ip, so p + t + 4 < ip + t + 4 <= s.len() (above).
                    if unsafe { load_u32_at(p.add(t)) == tail && load_u32_at(p) == head } {
                        break;
                    }
                    cur_node4 = next(mf, cur_node4);
                    depth_remaining -= 1;
                    if depth_remaining == 0 || cur_node4 <= cutoff {
                        break 'out;
                    }
                }
                // SAFETY: p < ip, and in_next + max_len <= s.len().
                let len = unsafe { lz_extend(ip, p, 4, max_len) };
                if len > best_len {
                    best_len = len;
                    best_match = p;
                    if best_len >= nice_len {
                        break 'out;
                    }
                }
                cur_node4 = next(mf, cur_node4);
                depth_remaining -= 1;
                if depth_remaining == 0 || cur_node4 <= cutoff {
                    break 'out;
                }
            }
        }
        (best_len, (ip as usize - best_match as usize) as u32)
    }

    /// hc_matchfinder_skip_bytes(): insert `count` positions without searching.
    #[inline(always)]
    fn skip_bytes(&mut self, s: &[u8], in_next: usize, count: u32) {
        if count as usize + 5 > s.len() - in_next {
            return;
        }
        let mut cur_pos = (in_next - self.base) as u32;
        // libdeflate carries each next position's hashes over in the loop; they
        // are recomputed here from the bytes instead (the same values: the
        // caller's next_hashes are those of in_next, never position 0), so that
        // no hash lives across iterations and LLVM keeps the loop in registers.
        debug_assert!(in_next > 0);
        let hashes = |p: *const u8| {
            // SAFETY: p is at most s[in_next + count], and in_next + count + 5 <= s.len().
            let seq = unsafe { load_u32_at(p) };
            (lz_hash(seq & 0xFFFFFF, HASH3_ORDER), lz_hash(seq, HASH4_ORDER))
        };
        let mut p = s.as_ptr().wrapping_add(in_next);
        for _ in 0..count {
            if cur_pos == WINDOW_SIZE {
                cold_path();
                slide_window(self.tab);
                self.base += WINDOW_SIZE as usize;
                cur_pos = 0;
            }
            let (hash3, hash4) = hashes(p);
            let mf = &mut *self.tab;
            mf[HASH3_TAB + hash3] = cur_pos as i16;
            mf[NEXT_TAB + (cur_pos as usize & WINDOW_MASK)] = mf[HASH4_TAB + hash4];
            mf[HASH4_TAB + hash4] = cur_pos as i16;
            p = p.wrapping_add(1);
            cur_pos += 1;
        }
        let (hash3, hash4) = hashes(p);
        prefetchw(self.tab, HASH3_TAB + hash3);
        prefetchw(self.tab, HASH4_TAB + hash4);
        self.next_hashes = [hash3, hash4];
    }
}

/************************************ Parser **********************************/

fn choose_min_match_len(num_used_literals: u32, max_search_depth: u32) -> u32 {
    const MIN_LENS: [u8; 80] = [
        9, 9, 9, 9, 9, 9, 8, 8, 7, 7, 6, 6, 6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
        5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    ];
    let min_len = MIN_LENS.get(num_used_literals as usize).map_or(3, |&x| x as u32);
    // With a low max_search_depth, long matches are hard to find.
    match max_search_depth {
        0..5 => min_len.min(4),
        5..10 => min_len.min(5),
        10..16 => min_len.min(7),
        _ => min_len,
    }
}

fn calculate_min_match_len(data: &[u8], max_search_depth: u32) -> u32 {
    if data.len() < 512 {
        return MIN_MATCH_LEN;
    }
    let mut used = [false; 256];
    for &b in &data[..data.len().min(4096)] {
        used[b as usize] = true;
    }
    choose_min_match_len(used.iter().filter(|&&u| u).count() as u32, max_search_depth)
}

fn recalculate_min_match_len(freqs: &Freqs, max_search_depth: u32) -> u32 {
    let literal_freq: u32 = freqs.litlen[..256].iter().sum();
    let cutoff = literal_freq >> 10;
    choose_min_match_len(freqs.litlen[..256].iter().filter(|&&f| f > cutoff).count() as u32, max_search_depth)
}

#[inline(always)]
fn adjust_max_and_nice_len(max_len: &mut u32, nice_len: &mut u32, remaining: usize) {
    if remaining < MAX_MATCH_LEN as usize {
        cold_path();
        *max_len = remaining as u32;
        *nice_len = (*nice_len).min(*max_len);
    }
}

thread_local! {
    /// The matchfinder (256 KiB) and sequence store (up to 400 KiB), kept per thread:
    /// allocating them for every call costs page faults. The sequence store is reserved, not
    /// filled, so only the pages of the sequences a block uses are touched.
    /// The sequences of a block end with the literal run that ends the block
    /// (match length 0).
    static SCRATCH: Cell<Option<(Box<Matchfinder>, Vec<Sequence>)>> = const { Cell::new(None) };
}

/// The scratch matchfinder and sequence store of this thread ([`SCRATCH`]), the matchfinder
/// cleared.
fn scratch() -> (Box<Matchfinder>, Vec<Sequence>) {
    let (mut tab, seqs) = SCRATCH.take().unwrap_or_else(|| {
        let tab = vec![MF_INITVAL; MF_SIZE].into_boxed_slice().try_into().unwrap();
        (tab, Vec::with_capacity(SEQ_STORE_LENGTH + 1))
    });
    tab.fill(MF_INITVAL);
    (tab, seqs)
}

/// The end of the block that starts at `in_block_begin`: at most SOFT_MAX_BLOCK_LENGTH bytes on,
/// unless the rest would make a block shorter than MIN_BLOCK_LENGTH (choose_max_block_end()).
fn max_block_end(in_block_begin: usize, in_end: usize) -> usize {
    if in_end - in_block_begin < SOFT_MAX_BLOCK_LENGTH + MIN_BLOCK_LENGTH { in_end } else { in_block_begin + SOFT_MAX_BLOCK_LENGTH }
}

/// Starts a block: no literals or sequences yet, and no statistics.
fn begin_block(c: &mut Compressor, seqs: &mut Vec<Sequence>) {
    c.litrunlen = 0;
    c.num_seqs = 0;
    seqs.clear();
    c.split_stats = SplitStats::default();
    c.freqs.litlen.fill(0);
    c.freqs.offset.fill(0);
}

/// deflate_compress_greedy(): the longest match at each position, with a length-3 match only
/// within 4096 bytes.
fn compress_greedy<'a>(c: &mut Compressor, s: &[u8], mut w: BitWriter<'a>, level: Level) -> BitWriter<'a> {
    let in_end = s.len();
    let mut in_next = 0;
    let mut max_len = MAX_MATCH_LEN;
    let mut nice_len = level.nice_match_length.min(max_len);
    let (mut tab, mut seqs) = scratch();
    let mut mf = Mf { tab: &mut tab, base: 0, next_hashes: [0, 0] };

    loop {
        let in_block_begin = in_next;
        let in_max_block_end = max_block_end(in_block_begin, in_end);
        begin_block(c, &mut seqs);
        let min_len = calculate_min_match_len(&s[in_next..in_max_block_end], level.max_search_depth);

        loop {
            adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
            // SAFETY: adjust_max_and_nice_len() keeps nice_len <= max_len <= in_end - in_next,
            // and in_end == s.len().
            let (length, offset) = unsafe { mf.longest_match(s, in_next, min_len - 1, max_len, nice_len, level.max_search_depth) };
            if length >= min_len && (length > MIN_MATCH_LEN || offset <= 4096) {
                c.choose_match(length, offset, &mut seqs);
                mf.skip_bytes(s, in_next + 1, length - 1);
                in_next += length as usize;
            } else {
                c.choose_literal(s[in_next]);
                in_next += 1;
            }
            if in_next >= in_max_block_end
                || c.num_seqs >= SEQ_STORE_LENGTH
                || c.split_stats.should_end_block(in_block_begin, in_next, in_end)
            {
                break;
            }
        }
        seqs.push(Sequence { litrunlen_and_length: c.litrunlen, offset: 0, offset_slot: 0 });
        w = c.finish_block(w, &s[in_block_begin..in_next], &seqs, in_next == in_end);
        if in_next == in_end {
            SCRATCH.set(Some((tab, seqs)));
            return w;
        }
    }
}

/// deflate_compress_lazy_generic(): deflate_compress_lazy(), or with `Parser::Lazy2`
/// deflate_compress_lazy2(), which looks a second position ahead.
fn compress_lazy<'a>(c: &mut Compressor, s: &[u8], mut w: BitWriter<'a>, level: Level) -> BitWriter<'a> {
    let in_end = s.len();
    let mut in_next = 0;
    let mut max_len = MAX_MATCH_LEN;
    let mut nice_len = level.nice_match_length.min(max_len);
    let depth = level.max_search_depth;
    let (mut tab, mut seqs) = scratch();
    let mut mf = Mf { tab: &mut tab, base: 0, next_hashes: [0, 0] };

    loop {
        let in_block_begin = in_next;
        let in_max_block_end = max_block_end(in_block_begin, in_end);
        let mut next_recalc_min_len = in_next + (in_end - in_next).min(10000);
        begin_block(c, &mut seqs);
        let mut min_len = calculate_min_match_len(&s[in_next..in_max_block_end], depth);

        loop {
            if in_next >= next_recalc_min_len {
                cold_path();
                min_len = recalculate_min_match_len(&c.freqs, depth);
                next_recalc_min_len += (in_end - next_recalc_min_len).min(in_next - in_block_begin);
            }
            adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
            // SAFETY: adjust_max_and_nice_len() keeps nice_len <= max_len <= in_end - in_next,
            // and in_end == s.len().
            let (mut cur_len, mut cur_offset) = unsafe { mf.longest_match(s, in_next, min_len - 1, max_len, nice_len, depth) };
            if cur_len < min_len || (cur_len == MIN_MATCH_LEN && cur_offset > 8192) {
                c.choose_literal(s[in_next]);
                in_next += 1;
            } else {
                in_next += 1;
                loop {
                    // have_cur_match
                    if cur_len >= nice_len {
                        c.choose_match(cur_len, cur_offset, &mut seqs);
                        mf.skip_bytes(s, in_next, cur_len - 1);
                        in_next += cur_len as usize - 1;
                        break;
                    }
                    adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                    // SAFETY: as above.
                    let (next_len, next_offset) = unsafe { mf.longest_match(s, in_next, cur_len - 1, max_len, nice_len, depth >> 1) };
                    in_next += 1;
                    if next_len >= cur_len && 4 * (next_len - cur_len) as i32 + (bsr32(cur_offset) - bsr32(next_offset)) > 2 {
                        c.choose_literal(s[in_next - 2]);
                        cur_len = next_len;
                        cur_offset = next_offset;
                        continue;
                    }
                    if level.parser == Parser::Lazy2 {
                        // A much better match two positions ahead: two literals, then that match.
                        adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                        // SAFETY: as above.
                        let (next_len, next_offset) = unsafe { mf.longest_match(s, in_next, cur_len - 1, max_len, nice_len, depth >> 2) };
                        in_next += 1;
                        if next_len >= cur_len && 4 * (next_len - cur_len) as i32 + (bsr32(cur_offset) - bsr32(next_offset)) > 6 {
                            c.choose_literal(s[in_next - 3]);
                            c.choose_literal(s[in_next - 2]);
                            cur_len = next_len;
                            cur_offset = next_offset;
                            continue;
                        }
                        c.choose_match(cur_len, cur_offset, &mut seqs);
                        if cur_len > 3 {
                            mf.skip_bytes(s, in_next, cur_len - 3);
                            in_next += cur_len as usize - 3;
                        }
                        break;
                    }
                    c.choose_match(cur_len, cur_offset, &mut seqs);
                    mf.skip_bytes(s, in_next, cur_len - 2);
                    in_next += cur_len as usize - 2;
                    break;
                }
            }
            if in_next >= in_max_block_end
                || c.num_seqs >= SEQ_STORE_LENGTH
                || c.split_stats.should_end_block(in_block_begin, in_next, in_end)
            {
                break;
            }
        }
        seqs.push(Sequence { litrunlen_and_length: c.litrunlen, offset: 0, offset_slot: 0 });
        w = c.finish_block(w, &s[in_block_begin..in_next], &seqs, in_next == in_end);
        if in_next == in_end {
            SCRATCH.set(Some((tab, seqs)));
            return w;
        }
    }
}

/// zlib-wrapped DEFLATE of `input`, byte-for-byte what libdeflate 1.26
/// produces at compression level 6.
/// That is at [`Level::DEFAULT`]; another `level` searches as [`Level`] says.
pub(crate) fn zlib_compress(input: &[u8], level: Level) -> Vec<u8> {
    let n = input.len();
    let passthrough = level.max_passthrough_size();
    // Room for the header, a stored input of up to `passthrough` bytes and the trailer;
    // flush_block() grows it for each block.
    let mut out = vec![0u8; 2 + 5 + passthrough + 4];
    out[..2].copy_from_slice(&level.zlib_header());
    let mut pos = 2;

    if n <= passthrough {
        // deflate_compress_none()
        if n == 0 {
            out[2..7].copy_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
            pos += 5;
        } else {
            out[2] = 1; // BFINAL, stored; n <= 51 fits one block
            out[3..5].copy_from_slice(&(n as u16).to_le_bytes());
            out[5..7].copy_from_slice(&(!(n as u16)).to_le_bytes());
            out[7..7 + n].copy_from_slice(input);
            pos += 5 + n;
        }
    } else {
        let mut c = Compressor {
            litrunlen: 0,
            num_seqs: 0,
            freqs: Freqs { litlen: [0; NUM_LITLEN_SYMS], offset: [0; NUM_OFFSET_SYMS] },
            split_stats: SplitStats::default(),
            codes: Codes { cw: [0; 320], lens: [0; 320] },
            static_codes: static_codes(),
            precode: Precode {
                freqs: [0; 19],
                cw: [0; 19],
                lens: [0; 19],
                items: [0; 320],
                num_litlen_syms: 0,
                num_offset_syms: 0,
                num_explicit_lens: 0,
                num_items: 0,
            },
        };
        let w = BitWriter { out: &mut out, pos, bitbuf: 0, bitcount: 0 };
        let mut w = match level.parser {
            Parser::Greedy => compress_greedy(&mut c, input, w, level),
            Parser::Lazy | Parser::Lazy2 => compress_lazy(&mut c, input, w, level),
        };
        if w.bitcount != 0 {
            w.out[w.pos] = w.bitbuf as u8;
            w.pos += 1;
        }
        pos = w.pos;
    }
    out[pos..pos + 4].copy_from_slice(&zlib_rs::adler32::adler32(1, input).to_be_bytes());
    out.truncate(pos + 4);
    // Callers keep every chunk until the file is assembled: give the slack back.
    out.shrink_to_fit();
    out
}

#[cfg(test)]
mod tests {
    use super::{Level, zlib_compress};

    /// The output equals libdeflate's at level 6 (`libdeflate_zlib_compress`), given by its length
    /// and CRC-32: text, incompressible bytes (stored blocks), a smooth Int16 tile of 512 KiB (several
    /// blocks) and a 27-byte input, under the passthrough size (one stored block). The expected
    /// values were computed with libdeflate 1.19 (the port follows 1.26, which was compared on
    /// raster tiles outside the suite).
    #[test]
    fn output_matches_libdeflate() {
        let mut v = 0i64;
        let walk: Vec<u8> = lcg(512 * 512, 2, 256)
            .into_iter()
            .flat_map(|s| {
                v += i64::from(s % 7) - 3;
                (v.div_euclid(8) as u16).to_le_bytes()
            })
            .collect();
        let cases: [(&str, Vec<u8>, usize, u32); 4] = [
            ("text", b"The quick brown fox jumps over the lazy dog. ".repeat(3000), 460, 0x8706a878),
            ("random", lcg(70000, 1, 256), 70016, 0x78d8eb6e),
            ("walk", walk, 60719, 0xb685dbf7),
            ("small", b"abcabcabc".repeat(3), 38, 0x5cbab911),
        ];
        for (name, input, len, crc) in cases {
            let z = zlib_compress(&input, Level::DEFAULT);
            assert_eq!((z.len(), zlib_rs::crc32::crc32(0, &z)), (len, crc), "{name}");
        }
    }

    /// Every level decompresses to `data`, within the stored-block bound.
    fn roundtrip(data: &[u8]) {
        for level in (1..=9).map(|l| Level::new(l).unwrap()) {
            let z = zlib_compress(data, level);
            let mut out = vec![0u8; data.len() + 1];
            let (dec, rc) = zlib_rs::decompress_slice(&mut out, &z, zlib_rs::InflateConfig::default());
            assert_eq!(rc, zlib_rs::ReturnCode::Ok, "len {}, {level:?}", data.len());
            assert!(dec == data, "mismatch at len {}, {level:?}", data.len());
            assert!(z.len() <= 6 + 5 * data.len().div_ceil(5000).max(1) + data.len(), "{level:?}");
        }
    }

    fn lcg(n: usize, seed: u64, alphabet: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as u32 % alphabet) as u8
            })
            .collect()
    }

    #[test]
    fn edge_inputs() {
        roundtrip(&[]);
        roundtrip(&[42]);
        roundtrip(&vec![0u8; 1 << 20]);
        roundtrip(&lcg(1 << 20, 1, 256));
        // Lengths around the passthrough size, MIN_BLOCK_LENGTH, the window,
        // stored-block limit and SOFT_MAX_BLOCK_LENGTH (+ MIN_BLOCK_LENGTH).
        let text = lcg(700_000, 7, 12);
        let rnd = lcg(700_000, 9, 256);
        for n in [
            2, 3, 4, 5, 6, 31, 32, 33, 257, 258, 259, 511, 512, 4999, 5000, 5001, 9999, 10000, 10001, 32767, 32768, 32769, 65535, 65536,
            65537, 299_999, 300_000, 300_001, 304_999, 305_000, 305_001, 600_000, 610_000,
        ] {
            roundtrip(&text[..n]);
            roundtrip(&rnd[..n]);
        }
        // Alternating compressible / incompressible regions: block splits,
        // stored blocks after non-byte-aligned compressed ones, and back.
        let mut mixed = Vec::new();
        for i in 0..12 {
            let len = 3000 + i * 7919;
            mixed.extend_from_slice(if i % 2 == 0 { &text[..len] } else { &rnd[i * 1000..i * 1000 + len] });
        }
        roundtrip(&mixed);
        let mut runs = Vec::new();
        for i in 0..400_000u32 {
            runs.push((i / 300) as u8);
        }
        roundtrip(&runs);
    }

    /// A smooth Int16 tile of 512 KiB, as in `output_matches_libdeflate`.
    fn walk() -> Vec<u8> {
        let mut v = 0i64;
        lcg(512 * 512, 2, 256)
            .into_iter()
            .flat_map(|s| {
                v += i64::from(s % 7) - 3;
                (v.div_euclid(8) as u16).to_le_bytes()
            })
            .collect()
    }

    /// Levels 1 to 9, and no others. On an Int16 tile, each gives other bytes than its neighbours,
    /// and a higher level no larger an output; the zlib header gives the level's FLEVEL; and an
    /// input of at most 55 - 4 x level bytes is stored.
    #[test]
    fn each_level_trades_time_for_size() {
        assert_eq!((Level::new(0), Level::new(10)), (None, None));
        assert_eq!(Level::new(6), Some(Level::DEFAULT));
        let walk = walk();
        let outputs: Vec<Vec<u8>> = (1..=9).map(|l| zlib_compress(&walk, Level::new(l).unwrap())).collect();
        let sizes: Vec<usize> = outputs.iter().map(Vec::len).collect();
        for l in 1..9 {
            assert_ne!(outputs[l - 1], outputs[l], "levels {l} and {}", l + 1);
            assert!(sizes[l] <= sizes[l - 1], "level {} larger than level {l}: {sizes:?}", l + 1);
        }
        let headers: Vec<[u8; 2]> = outputs.iter().map(|z| [z[0], z[1]]).collect();
        let (fastest, fast, default, slowest) = ([0x78, 0x01], [0x78, 0x5E], [0x78, 0x9C], [0x78, 0xDA]);
        assert_eq!(headers, [fastest, fast, fast, fast, fast, default, default, slowest, slowest]);
        // A stored input takes 11 bytes more: the zlib header and trailer, and the block's header.
        for l in 1..=9 {
            let stored = |n: usize| zlib_compress(&b"a".repeat(n), Level::new(l).unwrap()).len() == n + 11;
            let at_most = 55 - 4 * l as usize;
            assert!(stored(at_most) && !stored(at_most + 1), "level {l}");
        }
    }

    /// The output of each level, 1 to 9, given by its length and CRC-32: on the Int16 tile; on
    /// 40 bytes, stored up to level 3 and compressed from level 4, as the size under which an
    /// input is stored shrinks with the level; on 300,000 bytes of 12 letters; on 100,000 bytes of
    /// 5 letters, where the minimum match length that a level's search depth allows decides; on
    /// matches of every length from 3 to 258, each with a longer one further back, where the
    /// level's nice match length decides; and on noise with 3-byte strings repeated 6,000 bytes
    /// later, where the greedy parser's 4,096-byte limit for length-3 matches decides (levels 1 to
    /// 4 store it; the lazy parser's limit is 8,192). Level 6 gives the values of
    /// `output_matches_libdeflate`.
    #[test]
    fn each_level_gives_these_outputs() {
        // The length and CRC-32 of the output of levels 1 to 9.
        type Outputs = [(usize, u32); 9];
        // A 300-byte string, then, for each length k from 3 to 258, its first k bytes and the whole
        // string again: there, the nearest match has length k, and the one before it 258.
        let base = lcg(300, 11, 256);
        let mut nice = base.clone();
        for (k, seed) in (3..=258).zip((101..).step_by(2)) {
            nice.extend(lcg(16, seed, 256));
            nice.extend_from_slice(&base[..k]);
            nice.push(!base[k]);
            nice.extend(lcg(16, seed + 1, 256));
            nice.extend_from_slice(&base[..260]);
        }
        // Noise of 256 letters (a minimum match length of 3 at every level), the 3 bytes at every
        // 97th position repeated 6,000 bytes later.
        let mut far = lcg(40_000, 3, 256);
        let n = far.len();
        for i in (0..).step_by(97).take_while(|i| i + 6003 <= n) {
            let three = [far[i], far[i + 1], far[i + 2]];
            far[i + 6000..i + 6003].copy_from_slice(&three);
        }
        let cases: [(&str, Vec<u8>, Outputs); 6] = [
            (
                "Int16 tile",
                walk(),
                [
                    (68469, 0x5e59f553),
                    (65923, 0x0bcfb63e),
                    (63879, 0xeb7a5d27),
                    (62142, 0x4cc90cf4),
                    (61993, 0xf18206cd),
                    (60719, 0xb685dbf7),
                    (59340, 0xe28281a5),
                    (58844, 0x4ccce4f1),
                    (58484, 0xb22b2b14),
                ],
            ),
            (
                "40 bytes",
                b"abcdefghij".repeat(4),
                [
                    (51, 0xed79f363),
                    (51, 0xab3c1311),
                    (51, 0xab3c1311),
                    (21, 0xbaecab36),
                    (21, 0xbaecab36),
                    (21, 0x2e2b3d80),
                    (21, 0x2e2b3d80),
                    (21, 0x9ff00043),
                    (21, 0x9ff00043),
                ],
            ),
            (
                "12 letters",
                lcg(300_000, 7, 12),
                [
                    (149967, 0x6749d11b),
                    (141927, 0x9d5633f7),
                    (139619, 0x1ce17eed),
                    (139619, 0x1ce17eed),
                    (139618, 0x3ab80c37),
                    (139618, 0x975f1e70),
                    (139618, 0x975f1e70),
                    (139618, 0x13a5e94b),
                    (139618, 0x13a5e94b),
                ],
            ),
            (
                "5 letters",
                lcg(100_000, 5, 5),
                [
                    (35163, 0x8fbb55a7),
                    (34203, 0xc14c4b79),
                    (31724, 0x073d02ea),
                    (32204, 0x9cff22f6),
                    (32204, 0xa42790c4),
                    (32042, 0xa40c56ab),
                    (31925, 0x089df2f7),
                    (31925, 0x9d156be3),
                    (31925, 0x9d156be3),
                ],
            ),
            (
                "nice lengths",
                nice,
                [
                    (10915, 0xd81e36e8),
                    (10937, 0xc3c75cdc),
                    (10934, 0x34238382),
                    (10917, 0xf4305c8a),
                    (10914, 0x9fb22478),
                    (10856, 0x7678b549),
                    (10731, 0x9fc6ef29),
                    (10435, 0x00934499),
                    (10435, 0x00934499),
                ],
            ),
            (
                "3-byte strings 6,000 bytes back",
                far,
                [
                    (40011, 0xa32dd0db),
                    (40011, 0x8df33568),
                    (40011, 0x8df33568),
                    (40011, 0x8df33568),
                    (39929, 0x5c237e89),
                    (39929, 0x8ac4f6ff),
                    (39929, 0x8ac4f6ff),
                    (39929, 0x5d682a6c),
                    (39929, 0x5d682a6c),
                ],
            ),
        ];
        for (name, input, want) in cases {
            let got: Vec<(usize, u32)> = (1..=9)
                .map(|l| {
                    let z = zlib_compress(&input, Level::new(l).unwrap());
                    (z.len(), zlib_rs::crc32::crc32(0, &z))
                })
                .collect();
            assert_eq!(got, want, "{name}");
        }
    }
}
