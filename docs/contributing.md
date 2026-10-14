# Contributing

Contributions are welcome: bug reports with a file that terrio reads differently from GDAL are
especially useful.

## Development

```sh
git clone https://github.com/opengeos/terrio
cd terrio
cargo build
cargo test --workspace
```

Before opening a pull request:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
TERRIO_TEST_DATA=../terrio-data cargo test --workspace
cargo build -p terrio --target wasm32-unknown-unknown
RUSTDOCFLAGS="-D warnings" cargo doc -p terrio --no-deps   # every public item documented
cargo +1.90 check --workspace --all-targets                # the minimum supported Rust version
pre-commit run --all-files
```

Changes to the optional GDAL backend need GDAL installed (`pkg-config gdal` must find it):

```sh
cargo clippy -p terrio -p terrio-cli --all-targets --features gdal -- -D warnings
TERRIO_TEST_DATA=../terrio-data cargo test -p terrio -p terrio-cli --features gdal
```

## Minimum supported Rust version

terrio builds with Rust 1.90 and later: the `rust-version` of `Cargo.toml`, checked by the
`msrv` CI job. The minimum is raised only when a dependency or a language feature needs it, never
to a Rust release less than six months old, and from 1.0 on only in a minor release. The CI job,
`Cargo.toml`, the crate documentation and this page change together.

## Guidelines

- Compare with GDAL. A driver or operation change should come with a comparison on real files
  (see [Testing Against GDAL](testing.md)) and tests holding GDAL's values.
- Stay pure Rust. New dependencies must not need a C toolchain, and the library must keep building
  for WebAssembly. The optional `gdal` feature is the one exception.
- Document every public item (`#![warn(missing_docs)]`), with a `no_run` example on entry points.
- Name things as GDAL does: driver short names, option names in kebab case.

## Documentation

The site is built with [Zensical](https://zensical.org):

```sh
pip install zensical
zensical serve
```
