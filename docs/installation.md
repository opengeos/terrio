# Installation

terrio is published on [crates.io](https://crates.io/crates/terrio),
[PyPI](https://pypi.org/project/terrio/), [conda-forge](https://anaconda.org/conda-forge/terrio) and
[npm](https://www.npmjs.com/package/terrio-js) (as `terrio-js`).

## Command-line tool

The installer downloads the standalone `terrio` binary for your platform (Linux x86_64 and aarch64,
static, any distribution; macOS Intel and Apple silicon; Windows x64 and ARM64) from the latest
[GitHub release](https://github.com/opengeos/terrio/releases), checks its SHA-256 checksum and
puts it in `$XDG_BIN_HOME` if set, otherwise `~/.local/bin` (`%USERPROFILE%\.local\bin` on
Windows). No Rust or Python is needed.

```sh
# Linux and macOS
curl -LsSf https://terrio.dev/install.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy ByPass -c "irm https://terrio.dev/install.ps1 | iex"
```

Set `TERRIO_VERSION` to install another release, and `TERRIO_INSTALL_DIR` to install elsewhere:

```sh
curl -LsSf https://terrio.dev/install.sh | TERRIO_VERSION=0.6.0 sh
```

```powershell
$env:TERRIO_VERSION = "0.6.0"; irm https://terrio.dev/install.ps1 | iex
```

The Windows installer adds the directory to your `PATH` (`TERRIO_NO_MODIFY_PATH=1` to leave it);
the shell installer prints the line to add when it isn't on it. Run the installer again to update
terrio, and see [Uninstall](#uninstall) to remove it.

On macOS and Linux, [Homebrew](https://brew.sh) installs the release's binary from the
[opengeos/terrio tap](https://github.com/opengeos/homebrew-terrio), updated with each release:

```sh
brew install --cask opengeos/terrio/terrio
```

`brew upgrade --cask terrio` updates it and `brew uninstall --cask terrio` removes it. The macOS
binaries are not notarized, so the cask removes the quarantine attribute that would make Gatekeeper
refuse to run the command. Its checksums come from the same GitHub release as the binaries: they
catch a corrupted download, but are not an independent check of the release.

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
conda install -c conda-forge terrio
```

A Docker image for linux/amd64 and linux/arm64 is published to the
[GitHub Container Registry](https://github.com/opengeos/terrio/pkgs/container/terrio) with each
release after 0.5.0, tagged with the version (`X.Y.Z`, `X.Y`) and `latest`. It runs as a non-root user in
`/data`, so mount your data there and pass your own user to write to it:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" ghcr.io/opengeos/terrio info input.tif
```

With rootless Docker, use `--user 0:0` instead: the container's root is your own user there.

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
# or, from conda-forge, with either of
conda install -c conda-forge terrio
mamba install -c conda-forge terrio
```

Wheels are built for Linux (x86_64 and aarch64, glibc and musl), macOS (Intel and Apple silicon)
and Windows (x64 and ARM64), for Python 3.10 and later. They need only NumPy (and the small
`affine` package).

The [conda-forge package](https://github.com/conda-forge/terrio-feedstock) needs Python 3.12 or
later, as its recipe currently sets. It can follow a new release after a delay, so check
`terrio.__version__` for the version you have.

### Pyodide

A wheel for [Pyodide](https://pyodide.org) (Python in WebAssembly, in browsers and Node.js) is
attached to each [GitHub release](https://github.com/opengeos/terrio/releases) from 0.2.0 on,
built for the Pyodide release it names (0.29, Python 3.13). To try it without installing anything,
use the [notebooks in your browser](notebooks.md#in-your-browser). In your own page, serve a copy of
the wheel next to the page and install it with `micropip` (browsers refuse to download it from the
release, which does not allow cross-origin requests):

```python
import micropip

await micropip.install("terrio-0.6.0-cp310-abi3-pyemscripten_2025_0_wasm32.whl")
```

Files are read from and written to Pyodide's in-memory file system, or from bytes
(`terrio.open_bytes`); remote URLs are not read (there is no HTTP client in WebAssembly): fetch
them with `pyodide.http` first. Palette functions take `Raster` objects, bytes or RGBA sequences
for their color maps and do not write a `dst` (use `Raster.write`). `raster_index` and
`vector_index` take a mapping of locations to in-memory datasets, and their results serialize with
`DatasetIndex.to_bytes`: these paths rely on native output protection absent from this build.

### With the GDAL backend

The wheels on PyPI do not include the [GDAL backend](gdal.md). With GDAL 3.4 or later installed
(where `pkg-config` or `GDAL_HOME` finds it), build terrio with it from source:

```sh
git clone https://github.com/opengeos/terrio
cd terrio/python && pip install maturin numpy affine
maturin build --release --features gdal && pip install ../target/wheels/terrio-*.whl
```

`terrio.open_raster`, `terrio.open_vector` and `terrio.rio.open` then fall back to GDAL for the
formats terrio doesn't read itself (NITF, HDF4...). The wheel links the GDAL found at build
time, which must stay installed.

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

## Uninstall

Remove terrio the way you installed it:

| Installed with | Command |
| --- | --- |
| `install.sh` | `rm "${XDG_BIN_HOME:-$HOME/.local/bin}/terrio"` |
| `install.ps1` | `Remove-Item "$env:USERPROFILE\.local\bin\terrio.exe"` |
| Homebrew | `brew uninstall --cask terrio` |
| AUR | `sudo pacman -R terrio-bin` (or `terrio`) |
| Fedora COPR | `sudo dnf remove terrio` |
| pip | `pip uninstall terrio` |
| uv | `uv tool uninstall terrio` |
| pipx | `pipx uninstall terrio` |
| conda or mamba | `conda remove terrio` or `mamba remove terrio` |
| Cargo or cargo-binstall | `cargo uninstall terrio-cli` |
| npm | `npm uninstall terrio-js` |
| Pyodide | `micropip.uninstall("terrio")` |
| Docker | `docker image rm ghcr.io/opengeos/terrio:<tag>` (`latest` or the version you pulled) |

If you installed with `TERRIO_INSTALL_DIR`, delete the binary from that directory instead. The
Windows installer adds its directory to your `PATH` if it isn't there already; if it did, you can
remove the entry from your user environment variables when nothing else uses that directory.

To find the copies on your `PATH`, run `which -a terrio` (`where.exe terrio` on Windows). Their
locations usually show how each was installed.

## Requirements

terrio has no system dependencies: no GDAL, PROJ, GEOS or SQLite to install. Building from source
needs a Rust toolchain, 1.90 or later.
