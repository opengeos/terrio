# Terrio contributor instructions

## Scope and architecture

Terrio provides geospatial I/O and operations through Rust, CLI/MCP, Python and
JavaScript/WebAssembly. Keep the default implementation pure Rust; GDAL is an
optional backend and a reference for conformance testing.

- Put shared algorithms in `crates/terrio`; keep CLI and language bindings thin.
- For a new operation, expose applicable options consistently across the public
  surfaces. CLI commands also supply the MCP interface.
- Preserve lazy window/overview reads for large or remote rasters. Measure files
  opened or bytes transferred when changing I/O behavior.
- Attribute algorithm ports and copied code with SPDX headers (REUSE) and in the applicable
  `NOTICE` files; see "Credit what you port" in `docs/contributing.md`.
- Name identifiers, including tests, after behavior (`Float32Terrain`,
  `slope_f32`). Record a reference GDAL version in a doc comment. Version numbers
  belong in names only when a public selector chooses that version, such as
  `KernelVersion::Gdal313`.
- Read `docs/testing.md` for validation and `ROADMAP.md` for planned scope. Read
  `docs/gdal-parity-recommendations.md` when working on GDAL feature parity.

## Builds and worktrees

The codec-heavy workspace produces large artifacts. Use these settings for local
iteration, including Python and WASM development builds:

```sh
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_BUILD_JOBS=2
```

- Keep targets on disk rather than a memory-backed `/tmp`. Set
  `CARGO_TARGET_DIR` to a worktree-specific directory if necessary.
- Give concurrent worktrees separate target directories; Cargo locks serialize
  builds sharing a target directory. Bound total parallelism across workers.
- When `sccache` is available, reuse its compiler cache across worktrees with
  `RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0`. Incremental Rust builds cannot be
  cached by sccache. Without it, keep incremental compilation for repeated edits.
- Use `cargo check`, focused tests and development binding builds during edits.
  Run optimized package builds for final validation when affected.
- Reuse completed checks until source, features or build settings change. Preserve
  active builds and artifacts belonging to other worktrees.
- Enable debug symbols explicitly when debugging; release profile changes require
  their own performance/size validation.

## Validation

- Add a regression for a behavior change. For GDAL parity, compare real data with
  GDAL and register comparisons in `scripts/conformance.py` where applicable.
- Check values, nodata/masks, types, georeferencing and metadata as relevant.
  Report tolerances and known differences explicitly; do not describe approximate
  agreement as pixel-identical.
- Run `cargo fmt --all -- --check`, workspace Clippy with `-D warnings`, and tests
  appropriate to the change. Verify affected Python/JavaScript APIs and the WASM
  build when shared code or bindings change.
- Run pre-commit on changed files before committing. Use the full suite when a
  change spans the workspace or repository instructions require it.
- Keep downloaded datasets, wheels, build outputs and credentials out of commits.

## GitHub workflow

- Use a feature branch and a separate worktree for independent parallel features
  when parallel work is requested. Preserve other contributors' changes.
- Open regular pull requests, not drafts. State the resulting behavior, supported
  scope, validation evidence and material limitations in the description.
- Batch verified review fixes into a commit per round, then reply to each comment
  with the fix or rationale and resolve addressed threads.
- Merge only when authorized, CI is green and actionable reviews are addressed.
  Preserve branch protections and use ordinary pushes rather than force pushes.
- Update tracking issues and the roadmap as features land. Check an item only when
  its documented scope is complete; leave a multi-feature tracker open until all
  of its items are done.
