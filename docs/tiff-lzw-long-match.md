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

Contributor measurements were supplied with the
[original contribution, #1285](https://github.com/opengeos/terrio/pull/1285).
The standalone real-strip/adversarial harness and raw measurement packet are not
included in this repository. The public synthetic writer workload is documented
in [lzw-write-performance.md](lzw-write-performance.md).

## Performance qualification

Contributor measurements are recorded in the [pull request](https://github.com/opengeos/terrio/pull/1288).
They predate collision-budget fallback and do not establish its performance.
Use the public synthetic workload in [lzw-write-performance.md](lzw-write-performance.md)
to compare current checkouts. Dense workloads, COGs, WASM runtime performance and
inputs above 4 GiB require their own measurements; byte identity does not imply
a universal speed-up.
