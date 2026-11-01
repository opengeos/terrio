# Contributing

Contributions are welcome, and you do not need to know Rust to make one. Bug reports with a file
that terrio reads differently from GDAL are especially useful, and the documentation, the Python
package's tests and the comparison scripts are mostly Markdown and Python.

If you are new to Rust, this page takes you from installing it to opening a pull request. A
first change can be small: look for
[good first issues](https://github.com/opengeos/terrio/labels/good%20first%20issue), or open an
issue to talk through what you have in mind before starting anything large.

## Set up

1. Install Rust with [rustup](https://rustup.rs), which also installs Cargo (Rust's build tool
   and package manager), `rustfmt` (the formatter) and Clippy (the linter). terrio's default build
   compiles no C code, but Rust links with your system's linker:

    - Linux: a C compiler package, such as `build-essential` on Debian and Ubuntu
    - macOS: `xcode-select --install`
    - Windows: the Visual Studio build tools, which the rustup installer offers

2. Get the code and check that it builds. Fork the repository first if you plan to open a pull
   request, and clone your fork.

    ```sh
    git clone https://github.com/opengeos/terrio
    cd terrio
    cargo run -p terrio-cli -- info crates/terrio/tests/data/float_reads/byte.tif
    ```

    The first build downloads and compiles every dependency, so expect it to take several minutes.
    Later builds only recompile what changed. If it prints the file's driver, size and bands, your
    setup works.

3. Optionally, install [rust-analyzer](https://rust-analyzer.github.io) for your editor. It shows
   types and errors as you type and jumps to definitions, which helps when reading unfamiliar code.

### Smaller, faster builds

terrio has many format codecs, so its build directory, `target/`, takes gigabytes. These settings
make it smaller by leaving out debug symbols, and limit the CPU cores a build uses. They are worth
adding to your shell's profile:

```sh
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_BUILD_JOBS=2  # raise this if your machine has cores and memory to spare
```

Remove the first two when you want to step through the code in a debugger.

## Find your way around

| Path | Contents |
|---|---|
| `crates/terrio/src` | The library, where most changes belong |
| `crates/terrio/src/drivers` | File formats, with vector formats in `drivers/vector` |
| `crates/terrio/src/ops` | Operations such as mosaic, contour and DEM analysis |
| `crates/terrio/tests` | Integration tests, with small test files in `tests/data` |
| `crates/terrio-crs` | Coordinate systems, transformations and datum grids |
| `crates/terrio-cli` | The `terrio` command, with one file per command group in `src/cmd` |
| `python` | Python bindings (PyO3) and the Python package |
| `js` | WebAssembly bindings (wasm-bindgen) and the npm package |
| `scripts` | Test data downloads and comparisons with GDAL |

The repository is a Cargo *workspace*: several *crates* (Rust packages) that build together. Most
commands below take `-p NAME` to work on one crate, which is much faster than the whole workspace.

Algorithms go in `crates/terrio`; the command line and the language bindings stay thin wrappers
around them. A new operation should be available from each of them where it applies.

## Everyday commands

| To | Run |
|---|---|
| Find compile errors quickly, without building | `cargo check -p terrio` |
| Run the command-line tool | `cargo run -p terrio-cli -- info FILE` |
| Run one integration test file | `cargo test -p terrio --test stac_geozarr` |
| Run tests whose names contain a word | `cargo test -p terrio --test stac_geozarr stac_items` |
| Show what passing tests print | `cargo test -p terrio --test stac_geozarr -- --nocapture` |
| Format the code | `cargo fmt --all` |
| Lint, treating warnings as errors | `cargo clippy --workspace --all-targets -- -D warnings` |
| Open the API documentation | `cargo doc -p terrio --no-deps --open` |

Arguments after a lone `--` go to your program or test runner rather than to Cargo. Debug builds
are slow to run: add `--release` when you time something or process a large file.

While editing, repeat `cargo check` and the tests closest to your change. Leave the whole workspace
for the end.

### Tests on real data

Tests that read files too large for the repository look for them in the directory named by
`TERRIO_TEST_DATA`, and **pass without checking anything when it is not set**. So a passing
`cargo test` does not cover them. To run them, download the data once, outside the repository, and
point the tests at it. The download is large, and GDAL's command-line tools are needed to derive
the copies in other formats (see [Testing Against GDAL](testing.md)).

```sh
scripts/fetch_test_data.sh ../terrio-data
TERRIO_TEST_DATA=../terrio-data cargo test -p terrio --test geopackage
```

### Python bindings

Use a virtual environment, and run `maturin develop` again after every change to Rust code. Python
does not see your change until you do.

```sh
python -m venv .venv
source .venv/bin/activate
pip install maturin pytest
cd python
maturin develop
python -m pytest tests/test_blend.py
```

If maturin reports that both `VIRTUAL_ENV` and `CONDA_PREFIX` are set, run `conda deactivate` until
no conda environment is active, then activate the virtual environment again. Some test files
compare with other libraries such as rasterio and xarray; the `python` job of
`.github/workflows/ci.yml` lists what to install to run them all.

### JavaScript and WebAssembly

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
wasm-pack build js --target nodejs --out-dir pkg-node --out-name terrio --no-opt
node --test js/tests/node.test.mjs
```

## Rust you will meet here

You can get a long way with chapters 1 to 10 of [the Rust Book](https://doc.rust-lang.org/book/).
A few things come up constantly in terrio:

- **Errors are values.** Functions that can fail return `terrio::Result<T>`, which holds either a
  `T` or a [`terrio::Error`](https://docs.rs/terrio/latest/terrio/enum.Error.html), and `?` returns
  the error to the caller. Files can be malformed, so library code should return an error rather
  than panic on bad input, which means no `unwrap()` or indexing that input can make fail. Tests
  may use `unwrap()` freely.
- **Ownership and borrowing.** A value has one owner, and `&value` lends it without copying. Most
  beginner compile errors are about this. When the compiler says a value was moved, try lending it
  before you reach for `.clone()`: rasters can be large.
- **Features.** Cargo features switch parts of the library on and off, as in
  `#[cfg(feature = "parquet")]`. A plain `cargo build` turns on the default ones; use
  `--features ndarray` to add another, and `--no-default-features` to start from none.
- **Doc comments.** `///` documents the item below it and `//!` the enclosing file. Their Markdown
  becomes the API documentation, and code blocks in them are compiled and run as tests.
- **Where tests live.** A test is a function marked `#[test]`. Tests of private code sit next to it
  in a `#[cfg(test)] mod tests` block; those in `crates/*/tests` use the public API as a user would.
  Each file in `tests/` is compiled separately, and copying the shape of a neighboring test is a
  good way to start.

## When something goes wrong

Read the compiler's errors from the top, because later ones are often caused by the first. They
usually say what to change, and `rustc --explain E0382` expands on any error code.

| What you see | What it means and what to do |
|---|---|
| A warning fails CI or pre-commit | `-D warnings` makes every warning an error. Fix it; `cargo clippy --fix --allow-dirty` applies the suggestions it is sure of. Read the resulting diff. |
| `missing documentation for ...` | Every public item needs a `///` comment. |
| `use of a disallowed method`, about rayon | Rayon's own thread pool hangs in forked processes. Use the functions of `crates/terrio/src/par.rs`, as the message says. |
| A dependency fails to build for `wasm32`, or asks for a C compiler | It needs C or something the browser does not have. Find a pure Rust crate, or put the code that needs it behind a feature or `#[cfg(not(target_arch = "wasm32"))]`. |
| An error only with `cargo +1.90` | You used something newer than the [minimum supported Rust version](#minimum-supported-rust-version). Rewrite it for Rust 1.90. |
| `Blocking waiting for file lock on build directory` | Another Cargo command, often your editor's rust-analyzer, is running. Wait for it. |
| The disk fills up | `cargo clean` deletes `target/`. The next build starts again from the beginning. |

Stuck? Open your pull request anyway and ask in it. Saying what you tried is enough, and nobody
expects a first Rust change to be perfect.

## Before opening a pull request

Work on a branch, and run these for every Rust change:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Then run what applies to your change:

```sh
TERRIO_TEST_DATA=../terrio-data cargo test --workspace    # drivers and operations, on real data
cargo build -p terrio --target wasm32-unknown-unknown     # library changes: it must build for the web
RUSTDOCFLAGS="-D warnings" cargo doc -p terrio --no-deps  # new or changed public items
cargo +1.90 check --workspace --all-targets               # new language features or dependencies
pre-commit run --all-files                                # also checks whitespace, TOML, YAML and JSON
```

The last two need a one-time install: `rustup toolchain install 1.90` and `pip install pre-commit`.

Continuous integration runs the cargo commands on Linux for pull requests, skipping the real-data
tests when only documentation changed. If you cannot run one locally, say so in the description; a
red check on your pull request is a normal way to find out what to fix. The job's log shows the
exact command that failed, so you can repeat it locally.

Changes to the optional GDAL backend need GDAL installed (`pkg-config gdal` must find it):

```sh
cargo clippy -p terrio -p terrio-cli --all-targets --features gdal -- -D warnings
TERRIO_TEST_DATA=../terrio-data cargo test -p terrio -p terrio-cli --features gdal
```

In the description, say what now behaves differently, how you checked it and any limits you know
about.

## Keep learning

- [The Rust Book](https://doc.rust-lang.org/book/), the standard introduction
- [Rust by Example](https://doc.rust-lang.org/rust-by-example/), the same ground through short
  programs
- [Rustlings](https://rustlings.rust-lang.org), small exercises to fix
- [The Cargo Book](https://doc.rust-lang.org/cargo/), for workspaces, features and profiles
- [The Clippy lint list](https://rust-lang.github.io/rust-clippy/master/), which explains each lint

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
  starts with SPDX headers: those files' copyright lines, copied verbatim, then the copyright
  line of terrio's `LICENSE`, the `SPDX-License-Identifier` of the licenses that apply, and the
  source files it derives from, with the version. A file written to match another program's
  behavior without translating its code says so in a comment instead.
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
