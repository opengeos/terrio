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
- Credit what you port. terrio follows the [REUSE specification](https://reuse.software), checked
  in CI with `reuse lint`. A file translated or adapted from another project's source files
  starts with SPDX headers: those files' copyright lines, copied verbatim, an
  `SPDX-FileCopyrightText: Terrio contributors` line, the `SPDX-License-Identifier` of the
  licenses that apply, and the source files it derives from, with the version. A file written to
  match another program's behavior without translating its code says so in a comment instead.
  Add the project to `NOTICE` and [Acknowledgements](acknowledgements.md), and data files from
  elsewhere to `REUSE.toml`.

## Release notes

The release workflow publishes notes generated from the merged pull requests. Edit the release
after it is published to end its notes with a **Credits** section naming the projects whose code
that release ports or adapts (GDAL, PROJ, PDAL, JTS and others), the features that came from
each, a link to `NOTICE`, and a link to
[sponsoring GDAL](https://gdal.org/en/stable/sponsors/index.html); release announcements carry
the same credits. Describe terrio as running without installing GDAL, not as being free of GDAL:
its commands follow GDAL's and many of its algorithms are ports of GDAL code.

## Documentation

The site is built with [Zensical](https://zensical.org):

```sh
pip install zensical
zensical serve
```
