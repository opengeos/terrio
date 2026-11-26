# Corrected JPEG XL encoder snapshot

These private modules adapt the lossless encoder of zune-jpegxl 0.5.2
(`https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpegxl`, copyright
zune-image developers, licensed MIT OR Apache-2.0 OR Zlib and used here under MIT), file
for file: `src/{bit_depth,bit_writer,color_convert,encoder,errors}.rs`, and `src/lib.rs`
as `mod.rs`.

`ChunkEncoder::encode_rle` joins a long zero run's extra bits, its LZ77 token's prefix code
and symbol 0's prefix code before writing them. zune-jpegxl 0.5.2 joins them in a `u32`;
together they can need up to 37 bits, so the top bits were lost and the group's bitstream
could not be decoded. Tiles of noise beside a large uniform area, such as an image smaller
than its tile with zero padding, were written unreadable for terrio and GDAL alike
(opengeos/terrio#1355). The snapshot builds the code in a `u64`, as zune-image's commit
97557d0 (zune-image issue #511) does after 0.5.2.

`encode_inner` keeps only the sequential group loop of the crate's `no_std` build. Before,
terrio ran 0.5.2's `std` runner with the `threads` feature, which another dependency enables:
a tile of several groups was encoded on up to two scoped threads. Each group writes to its own
bit writers, joined in order, so the bytes are the same; terrio compresses tiles in parallel.
The `std` and `threads` variants and the doc examples are omitted. Paths point at this
module, `alloc` at `std`. Each file's header keeps the source's copyright line and adds the
holder named by the crate's LICENSE-MIT. The snapshot
remains inside the published terrio crate so the correction survives Cargo packaging.

`jpeg_xl_tiles_of_noise_over_uniform_runs_read_back` and, with GDAL 3.13.3,
`gdal_reads_jpeg_xl_tiles_of_noise_over_uniform_runs` in `gtiff_write.rs` cover the correction.
Remove this snapshot when a zune-jpegxl release includes the fix.
