# Verified TIFF LZW long matches

The private whole-chunk encoder retains the writer API and weezl 0.2.1 byte
stream, including sparse-node promotion omissions and the unstored code 4096.
Only actually inserted dictionary codes become candidates. Each has a witness
input offset, length and prefix code. Length fits u16 because a twelve-bit
dictionary can grow a string by at most one byte per insertion; offsets use
usize so large chunks cannot truncate them. Witness storage adds 48 KiB per
encoder on 64-bit hosts (32 KiB on 32-bit hosts).

On each new phrase, the encoder tries the longest and latest witnesses with the
same offset modulo four. This phase is a performance hint for Float32 runs,
not a sample format restriction. Safe eight-byte comparisons verify the input.
A full match reaches the same dictionary code as successive byte lookups. A
partial match can reach its ancestor only when the parent walk is no longer
than the verified prefix. Otherwise the ordinary lookup loop resumes.

The dictionary is append-only between clears. Every stored string has its
stored prefix, so all ancestors exist and have identical assignments in the
ordinary loop. Skipping only hits cannot emit a code, add an entry, change width
or clear the dictionary. The next miss therefore sees exactly the same prefix,
next code, width and mappings. Candidates reset on every clear and every chunk;
stale witness slots are unreachable. Packing and final EOI adjustment are unchanged.

## Work bound

Let L be the ordinary greedy phrase length. Each of at most two comparisons
checks at most L matching bytes plus one mismatching word (at most seven extra
bytes). A fallback therefore charges at most 2L + 16 compared bytes to a phrase
that the ordinary loop consumes anyway. A successful full jump consumes its
verified bytes; partial recovery walks at most that many parent links. Including
constant insertion/candidate bookkeeping, added work is O(input bytes), not
quadratic even for long shared prefixes with differing tails. This is a bound
on operations, not a universal elapsed-time ratio: caches and host load matter.

The existing exhaustive short-length and boundary differential tests remain.
Mixed seeded tests add periods 1 to 9, unaligned NoData, copies, low-compressibility
noise and prefix-tail adversaries. Every encoder invocation in a unit-test build
asserts bytes against weezl. Full-file writer fixtures check assembly separately.
The deterministic writer example accepts an optional `dense` argument to remove
NoData from its synthetic Float32 raster for performance qualification.

The measurements below were supplied with the
[original contribution, #1285](https://github.com/opengeos/terrio/pull/1285).
The standalone real-strip/adversarial harness and raw measurement packet are not
included in this repository. The public synthetic writer workload is documented
in [lzw-write-performance.md](lzw-write-performance.md).

## Local qualification and tradeoff

Windows MSVC Rust 1.99.1, i7-11700K, three writer workers, one warm-up and
seven randomized interleaved blocks per arm. Retained Terrio 0.8.0 and
17773d2 binaries were re-run as controls. Builds/tests drained before timing;
unrelated host work remained uncontrolled. Wall includes process startup;
CPU is process user plus kernel. These are local results, not fleet qualification.

| Workload | Hash wall s | Skip wall s | Hash CPU s | Skip CPU s | CPU change |
|---|---:|---:|---:|---:|---:|
| synthetic | 2.109 | 1.555 | 0.797 | 0.594 | -25.5% |
| dense | 4.452 | 4.385 | 2.125 | 2.391 | +12.5% |
| frozen | 41.552 | 24.686 | 29.188 | 9.406 | -67.8% |

The dense CPU no-regression target was not met. This must be weighed before
adopting the change. Wall-time conditions changed substantially within the batch;
the contributor's raw ranges, paired differences and peak process working sets
are not included here. Dense uses 33,554,432 deterministic valid Float32 samples and no
NoData; its artificial grid is explicitly declared in the measurement setup.

Encoder-only, all 28,624 frozen strips (6,232,131,776 raw bytes),
one warm-up and seven alternating rounds: hash 21.515 s, skip
2.794 s (7.70x). Every encoded strip matched the stored bytes.
Noise and shared-prefix/different-tail adversaries covered 65,536 through
4,194,304 bytes; the worst observed median skip/hash ratio was
1.597. This is finite supporting evidence, not a universal timing bound.

Validation passed: package tests including doctests, workspace Clippy with
warnings denied, formatting, pre-commit, and the geotiff-only wasm32 compile.
All 48 baseline TIFF/COG combinations, concurrent writers and one/three-worker
fixtures passed. Explicit one- and three-thread frozen files matched SHA-256
`c50acda66d143167e568dc2efb6ea324297cbcb4da81896028b2816d472080b7`.
A separate flows2fim scratch source copy passed raster/core tests and seven NOAA
parity tests (18 stored reference rasters, 400 Rust-only controls; live Go unset).
Initial overlapping Cargo invocations hit a Windows executable file lock;
the subsequent full serial run passed. Dependency freshness was reviewed;
no dependencies or locks changed. COG performance, WASM runtime performance
and a giant input above 4 GiB remain unmeasured.
