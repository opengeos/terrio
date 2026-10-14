# Installation

terrio is not on crates.io, PyPI or npm yet. Until the first release, build it from source.

## Command-line tool

With [Rust](https://rustup.rs) installed:

```sh
cargo install --git https://github.com/opengeos/terrio terrio-cli
terrio --version
```

## Rust library

```toml
[dependencies]
terrio = { git = "https://github.com/opengeos/terrio" }
```

## Python

Build and install the package with [maturin](https://www.maturin.rs):

```sh
git clone https://github.com/opengeos/terrio
cd terrio/python
pip install maturin numpy
maturin develop --release
```

Once released: `pip install terrio`.

## JavaScript

Build the WebAssembly package with [wasm-pack](https://rustwasm.github.io/wasm-pack/):

```sh
git clone https://github.com/opengeos/terrio
cd terrio/js
wasm-pack build --target web      # or --target nodejs
```

Once released: `npm install terrio`.

## Requirements

terrio has no system dependencies: no GDAL, PROJ, GEOS or SQLite to install. A Rust toolchain
(1.90 or later) is the only requirement for building from source.

The optional GDAL backend (the `gdal` cargo feature, for formats terrio has no driver for) is the
exception: it needs GDAL 3.4 or later and its headers (`libgdal-dev` on Debian and Ubuntu,
`gdal` on Homebrew and conda-forge), found with `pkg-config` or the `GDAL_HOME` environment
variable. See [the GDAL backend](gdal.md#the-optional-gdal-backend).
