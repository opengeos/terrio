# Strip-heavy LZW write benchmark

Run `cargo build -p terrio --release --no-default-features --features geotiff
--example lzw_write_benchmark --example lzw_codec_profile`.
`RAYON_NUM_THREADS=3 target/release/examples/lzw_write_benchmark out.tif`
writes a deterministic 16384 by 2048 Float32 TIFF with one-row strips.
Most rows are NoData; the remainder contain deterministic pseudo-random values.
No engineering units or coordinate system are assigned to this synthetic fixture.

`lzw_codec_profile` separately measures fresh construction/encode and retained
reset/encode over 4096 identical 54431-sample Float32 strips. Use `--alloc` for
requested allocation counts/bytes, separately from acceptance timing. These are
synthetic microbenchmarks, not NOAA or end-to-end qualification. Input generation,
file writing, allocator instrumentation and phase clocks may affect results.
Compare baseline/candidate release executables on the same idle machine, warm up
both, then interleave at least seven pairs and verify full-file bytes each time.
