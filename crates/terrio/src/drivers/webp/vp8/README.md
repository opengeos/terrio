# Corrected VP8 encoder snapshot

These private modules adapt the lossy encoder from webp-rust 0.3.2
(`https://github.com/mith-mmk/webp-rust`, MIT, copyright 2026 MITH@mmk):

- `mod.rs`, `api.rs`, `predict.rs`, `bitstream.rs`: `src/encoder/lossy/`.
- `bool_writer.rs`: `src/encoder/vp8_bool_writer.rs`.

The token encoder omits coefficients for every skipped macroblock. Upstream's
`compute_skip_probability` sometimes suppresses the corresponding skip flag
when fewer than about two percent of the blocks are skipped. The decoder then
consumes the next block's coefficients for the skipped block and loses alignment.
Terrio always emits the skip probability when at least one block is skipped.

Only the fast method-zero entry point is used. Decoder tables and types come
from the pinned webp-rust dependency. The container writer uses ordinary Vec
operations, and unused public configuration/container/alpha wrappers are omitted.
Terrio's existing outer encoder handles alpha. The snapshot remains inside the
published Terrio crate so the correction survives Cargo packaging; it does not
depend on a workspace-only registry patch or a separately published fork.

The Rust skip-flag regression and `python/tests/test_webp_lossy_damage.py` cover
the correction. Remove this snapshot when the dependency exposes the same fixed
token grammar, after verifying the quality matrix and independent decoders.
