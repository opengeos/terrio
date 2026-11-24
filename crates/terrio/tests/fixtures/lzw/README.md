# TIFF LZW byte oracles

Generated with unmodified Terrio v0.8.0 (7f93ff1712b34e02a147753bd700364861dfed8e),
weezl 0.2.1, on x86_64-pc-windows-msvc (Rust 1.99.1).
`lzw_write_reuse.rs` reconstructs their deterministic inputs. Full-file comparison
covers GTiff/COG, strips/edge tiles, band/pixel interleaving, predictors 1/2/3,
classic/BigTIFF, NaN payloads, signed zero and infinities. `cog` is the first boolean
in each filename. These are engineering-neutral synthetic values, without an
assigned spatial reference or physical unit. Do not regenerate with candidate code.

`hash-collision-input.bin` is a separate 16 KiB uncompressed adversarial input,
not a v0.8.0 output oracle. It is generated deterministically by
`cargo run -p terrio --example lzw_collision_input -- <out.bin>`: at each byte,
choose the symbol with the longest collision chain in the original fixed hash,
breaking ties with the seeded candidate order. The generator models the original
dictionary's insertion, promotion and clearing rules. Both specialized paths
must exhaust their normal collision budget on this input; the full chunk is
then compared with the independent weezl encoder.
