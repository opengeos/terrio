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
pre-commit run --all-files
```

## Guidelines

- Compare with GDAL. A driver or operation change should come with a comparison on real files
  (see [Testing Against GDAL](testing.md)) and tests holding GDAL's values.
- Stay pure Rust. New dependencies must not need a C toolchain, and the library must keep building
  for WebAssembly.
- Name things as GDAL does: driver short names, option names in kebab case.

## Documentation

The site is built with [Zensical](https://zensical.org):

```sh
pip install zensical
zensical serve
```
