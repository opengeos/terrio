# Installation

terrio is published on [crates.io](https://crates.io/crates/terrio),
[PyPI](https://pypi.org/project/terrio/) and [npm](https://www.npmjs.com/package/terrio-js) (as `terrio-js`).

## Command-line tool

The installer downloads the standalone `terrio` binary for your platform (Linux x86_64 and aarch64,
static, any distribution; macOS Intel and Apple silicon; Windows x64 and ARM64) from the latest
[GitHub release](https://github.com/opengeos/terrio/releases), checks its SHA-256 checksum and
puts it in `$XDG_BIN_HOME` if set, otherwise `~/.local/bin` (`%USERPROFILE%\.local\bin` on
Windows). No Rust or Python is needed.

```sh
# Linux and macOS
curl -LsSf https://terrio.opengeos.org/install.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy ByPass -c "irm https://terrio.opengeos.org/install.ps1 | iex"
```

Set `TERRIO_VERSION` to install another release, and `TERRIO_INSTALL_DIR` to install elsewhere:

```sh
curl -LsSf https://terrio.opengeos.org/install.sh | TERRIO_VERSION=0.4.2 sh
```

```powershell
$env:TERRIO_VERSION = "0.4.2"; irm https://terrio.opengeos.org/install.ps1 | iex
```

The Windows installer adds the directory to your `PATH` (`TERRIO_NO_MODIFY_PATH=1` to leave it);
the shell installer prints the line to add when it isn't on it. Run the installer again to update
terrio, and delete the binary to uninstall it.

On Arch Linux and its derivatives, the [AUR](https://aur.archlinux.org/packages/terrio-bin) has
`terrio-bin` (the release's binary) and `terrio` (built from source); on Fedora,
[COPR](https://copr.fedorainfracloud.org/coprs/giswqs/terrio/) has `terrio`. Both are updated with
each release:

```sh
# Arch Linux, with an AUR helper such as yay or paru
yay -S terrio-bin

# Fedora
sudo dnf copr enable giswqs/terrio
sudo dnf install terrio
```

The command also comes with the Python package (`python -m terrio` runs it too):

```sh
pip install terrio      # or: uv tool install terrio, pipx install terrio
```

[cargo-binstall](https://github.com/cargo-bins/cargo-binstall) installs the release binaries
(`cargo binstall terrio-cli`), and with [Rust](https://rustup.rs) installed, `cargo install
terrio-cli` builds it from source.

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

Wheels are built for Linux (x86_64 and aarch64, glibc and musl), macOS (Intel and Apple silicon)
and Windows (x64 and ARM64), for Python 3.10 and later. They need only NumPy (and the small
`affine` package).

### Pyodide

A wheel for [Pyodide](https://pyodide.org) (Python in WebAssembly, in browsers and Node.js) is
attached to each [GitHub release](https://github.com/opengeos/terrio/releases) from 0.2.0 on,
built for the Pyodide release it names (0.29, Python 3.13). Install it with `micropip`, from the
release or a copy served next to the page:

```python
import micropip

await micropip.install(
    "https://github.com/opengeos/terrio/releases/download/v0.4.2/"
    "terrio-0.4.2-cp310-abi3-pyemscripten_2025_0_wasm32.whl"
)
```

Files are read from and written to Pyodide's in-memory file system, or from bytes
(`terrio.open_bytes`); remote URLs are not read (there is no HTTP client in WebAssembly): fetch
them with `pyodide.http` first.

### With the GDAL backend

The `terrio` wheels do not include the [GDAL backend](gdal.md). `terrio-gdal` is the same package
with it and GDAL bundled (with PROJ's database and GDAL's data files), for Linux (x86_64,
aarch64), macOS (arm64, x86_64) and Windows (x64):

```sh
pip install terrio-gdal
```

It installs the `terrio` package too, so it replaces `terrio` rather than adding to it (`pip
uninstall terrio` first). `terrio.open_raster`, `terrio.open_vector` and `terrio.rio.open` then fall
back to GDAL for the formats terrio doesn't read itself (NITF, GRIB, ESRI File Geodatabases...),
and `terrio.gdal_version()` gives GDAL's version (`None` without GDAL). Nothing else is needed: no
GDAL on the system.

For a GDAL with other drivers, or another platform, build terrio from source against an installed
GDAL 3.4 or later (where `pkg-config` or `GDAL_HOME` finds it):

```sh
git clone https://github.com/opengeos/terrio
cd terrio/python && pip install maturin numpy affine
maturin build --release --features gdal && pip install ../target/wheels/terrio-*.whl
```

The wheel links the GDAL found at build time, which must stay installed.

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
