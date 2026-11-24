# TIFF LZW encoder compatibility

The writer specializes whole independent chunks for TIFF's MSB, eight-bit alphabet,
early-change LZW. Its public writer API and ordered parallel strip/tile collection
are unchanged. A reusable 8192-slot packed-key hash table (32 KiB) and successor
counts (4 KiB) replace the general-purpose weezl encoder's tree. Linear probing
always has space: at most 3838 dictionary entries are live. Every chunk starts
with clear code 256, ends with EOI 257, and resets its dictionary independently.
No unsafe code or new dependency is needed.
Compressed outputs start with at most 4 KiB capacity and grow as needed: ordered
collection must not retain a raw-sized buffer for every sparse strip.

Exact output compatibility includes weezl 0.2.1's sparse-node promotion behavior:
the seventeenth successor consumes a code but is not entered in the lookup table.
Later occurrences therefore miss again. Removing that quirk is a bitstream change.
The final EOI width adjustment and insertion of code 4096 immediately before clear
are also part of the compatibility contract. The latter entry cannot be referenced
and need not be stored. Keep weezl as the test oracle; a dependency upgrade must
revalidate bytes rather than silently changing that contract.

Validation:

```sh
cargo test -p terrio
cargo test -p terrio --lib tiff_lzw
cargo test -p terrio --test lzw_write_reuse
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Under library unit tests (`cfg(test)`) every completed specialized encode call asserts exact compressed bytes
against weezl, including existing writer tests. Independent differential tests cover
all prefix lengths 0 through 4095, selected larger strip boundaries, 64 randomized
streams, dictionary clears, sparse-node promotion, constant and periodic inputs,
plus decode round trips. Writer tests cover predicted 8/16/32-bit sample bytes and
float NaNs; the retained 48 complete baseline TIFF/COG fixtures exercise one and
three threads. The TIFF writer emits little-endian files. Big-endian sample-byte
coverage at the encoder layer does not claim a big-endian writer feature.

`lzw_source_profile` separates whole-file reads from in-memory decode in 256-row
windows over a supplied newline-delimited file list. Its complete-source traversal
is a diagnostic, not a measurement of compose's clipped source windows or CPU
shares. Synthetic in-memory writer workloads perform no source I/O or decode.
Performance evidence belongs with workload identities and raw paired measurements;
no cross-platform performance guarantee follows from the encoder design.


## Integration with the writer

The encoder keeps the direct `weezl` 0.2.1 oracle (the lockfile
also contains transitive 0.1.12). The writer still encodes MSB-first eight-bit
symbols with TIFF early size switching. No compression level applies to LZW.
Deflate's row-aware search and level, LERC secondary levels and Zstandard levels
remain on the release's original paths. Their option validation is unchanged.

Packed `NBITS` serialization and clipping warnings run before compression;
unpacked chunks retain the existing predictor transform. Strip/tile sizing,
padding, planar ordering, masks, LERC's valid-area masking of partial edge tiles
(`ChunkInfo::valid`) and overview assembly are unchanged. The coarse scratch
helper `par::map_range_init_coarse` is separate from the waiting-thread
scheduler; like `par::map_init`, it enters the pool through `Pool::run`, so
overlapping outside callers still run serially. Remote decoding, cancellation,
scoped read policies and read-budget checks are unchanged. State belongs to a
Rayon work piece or one serial call, not a worker thread or a source raster, and
each chunk resets the dictionary.

The crate disables automatic integration-test discovery. The LZW writer golden
suite is an explicit `[[test]]` target, retaining its fresh-process one/three-thread
checks rather than becoming a silently unbuilt test file.

## Adaptive selection and memory

Two sampled period-four hits, separated by min(chunk length / 4, 4096)
bytes (at least 32), enable the verified long-match path. A single 32-byte
NoData border or isolated periodic island no longer enables it. False negatives
only affect speed; sampling and all dictionary decisions depend only on bytes.

Witness arrays allocate on the first selected chunk and remain reusable. Heap
state excluding output and Vec headers is initially 36 KiB on both targets.
First skip use adds
48 KiB (64-bit) or 32 KiB (32-bit). Rayon `map_init` initializes per split
leaf, not strictly once per thread, so savings apply to each unused leaf.

## Bounded hash work

CLI and MCP callers can supply untrusted raster values. The fixed multiplicative
hash is not collision-resistant: the [original contribution's review](https://github.com/opengeos/terrio/pull/1285)
reported a hash-cluster input 60 to 80 times slower than weezl before mitigation.
Both specialized loops now allow at most `8 * input_bytes + 256` additional
linear-probe collisions per chunk, using saturating arithmetic. Dictionary clears
do not reset this budget. Direct hits and empty slots do not consume it.
The retained `hash-collision-input.bin` regression exhausts the normal budget
in both specialized paths; `lzw_collision_input` reproduces its raw bytes.

Exhausting the budget drops the partial output and re-encodes the entire chunk
with weezl 0.2.1's MSB eight-bit TIFF encoder. Restarting preserves exact bytes,
including width changes and sparse-node promotion behavior. The bound limits
specialized hash work; it does not promise a universal elapsed-time ratio.

The writer reserves a further 512 KiB per work piece for fallback allocations,
in addition to retained specialized scratch. This conservatively covers pinned
weezl 0.2.1's dictionary vector capacities, their largest reallocation overlap,
and boxed state. Its 4 KiB output growth fits the existing compressed-buffer
estimate. No partial specialized output remains live during fallback.

`lzw_reused_streams_match_fresh_tiff_size_switch` compares reused specialized
scratch with a fresh weezl encoder. Both specialized paths also assert their
independent weezl oracle in the library's `cfg(test)` checks.
Integration tests compile the library without that oracle; `lzw_write_reuse`
uses the 48 retained complete baseline TIFF fixtures instead.
