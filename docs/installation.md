# Installation

terrio is published on [crates.io](https://crates.io/crates/terrio),
[PyPI](https://pypi.org/project/terrio/) and [npm](https://www.npmjs.com/package/terrio-js) (as `terrio-js`).

## Command-line tool

With [Rust](https://rustup.rs) installed:

```sh
cargo install terrio-cli
terrio --version
```

The optional [GDAL backend](gdal.md), for formats terrio doesn't read itself, needs GDAL installed:
`cargo install terrio-cli --features gdal`.

## Rust library

```sh
cargo add terrio
```

## Python

```sh
pip install terrio
```

Wheels are built for Linux (x86_64 and aarch64; musl on x86_64), macOS (Intel and Apple silicon)
and Windows, for Python 3.10 and later. They need only NumPy.

## JavaScript

```sh
npm install terrio-js
```

The package is built for browsers and bundlers; in Node.js, pass the WebAssembly bytes to `init`
(see [JavaScript](javascript.md)).

## From source

The development version, with [Rust](https://rustup.rs) installed:

```sh
cargo install --git https://github.com/opengeos/terrio terrio-cli

git clone https://github.com/opengeos/terrio
cd terrio/python && pip install maturin numpy && maturin develop --release   # Python
cd ../js && wasm-pack build --target web                                      # JavaScript
```

## Requirements

terrio has no system dependencies: no GDAL, PROJ, GEOS or SQLite to install. Building from source
needs a Rust toolchain, 1.90 or later.
