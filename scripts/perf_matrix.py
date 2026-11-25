# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Performance matrix: every way a format's files and reads vary, measured in one run, against GDAL.

A perf pull request for each newly noticed case does not scale. For each format, this script lists
the axes its files vary along (codec, filter, type, chunking, layout) and the ways they are read
(one window, many windows, the whole file, an overview, several threads; a bbox or a few
columns), builds a fixture for each relevant file combination with GDAL or zarr-python, and reads
each fixture every way from a local path and over HTTP with a simulated round-trip time. terrio
and GDAL each read in a fresh process. A case records time, and what the read cost against what it
needed: bytes read or transferred against the bytes of the chunks the read touches, HTTP requests,
sequential rounds of requests and peak memory. The report ranks the worst cases and groups the
time ratios by axis, so a round of optimization starts from the slow areas, not from one file.

    python scripts/perf_matrix.py fixtures --format geotiff        # once: ~0.9 GB
    python scripts/perf_matrix.py run --format geotiff             # every case, 3 samples
    python scripts/perf_matrix.py run --format zarr --only v3_zstd --access window --quick
    python scripts/perf_matrix.py report RESULTS.jsonl             # the summary of a run again
    python scripts/perf_matrix.py run --format zarr --base /tmp/terrio-main   # also time another build

`--base DIR` times a second terrio build (`pip install --target DIR` of its wheel) in the same
run, interleaved with this one and GDAL: the report then gives this build's time against the base.

Formats: geotiff, zarr, geoparquet, flatgeobuf. Needs terrio's Python module (a release build:
`maturin develop --release` in python/), GDAL's Python bindings, pyogrio and PyArrow (GeoParquet,
FlatGeobuf) and zarr-python 3 (Zarr fixtures) in one environment. Fixtures and results stay in
--dir, out of the repository.
docs/benchmarks.md describes the method.
"""

from __future__ import annotations

import argparse
import contextlib
import errno
import itertools
import json
import math
import os
import resource
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

WIN = 512  # side of a raster window, in pixels


def windows(size: int, n: int) -> list[tuple[int, int, int, int]]:
    """The n windows of WIN pixels, off the chunk grid, spread over the raster (deterministic).

    Args:
        size: The raster's side, in pixels.
        n: The number of windows.

    Returns:
        `(x, y, width, height)` of each window.
    """
    step = (size - WIN) / max(1, n - 1)
    return [
        (int(100 + i * step * 0.97) % (size - WIN), int(37 + ((i * 7) % n) * step * 0.97) % (size - WIN), WIN, WIN)
        for i in range(n)
    ]


def terrain(np, size: int, bands: int, dtype: str):
    """Smooth terrain plus noise, per band, that compresses like real imagery or a DEM.

    8-bit bands are noisier, so JPEG/WebP tiles have real sizes.

    Args:
        np: The numpy module.
        size: The side of each band, in pixels.
        bands: The number of bands.
        dtype: The numpy data type's name.

    Returns:
        An array of shape `(bands, size, size)`.
    """
    y, x = np.mgrid[0:size, 0:size].astype("float32") / size
    rng = np.random.default_rng(1)
    out = np.empty((bands, size, size), dtype)
    for b in range(bands):
        z = np.sin(6 * x + b) * np.cos(5 * y) + 0.3 * np.sin(40 * x * y + b)
        z += (0.15 if dtype == "uint8" else 0.02) * rng.standard_normal(z.shape, dtype="float32")
        if dtype == "uint8":
            z = np.clip((z + 1.5) * 80, 0, 255)
        elif dtype in ("uint16", "int16"):
            z = (z + 1.5) * 8000 - (12000 if dtype == "int16" else 0)
        else:
            z = z * 1000 + 500
        out[b] = z
    return out


def checksum(a) -> tuple[float, int]:
    """The sum and count of a read's values, summed in float64 without a float64 copy.

    Args:
        a: The values read (an array, or anything numpy takes as one).

    Returns:
        `(sum, count)`.
    """
    import numpy as np

    a = np.asarray(a)
    return float(a.sum(dtype="float64")), int(a.size)


def table_checksum(res) -> tuple[float, int]:
    """The sum and row count of a vector read: of `id`, or of `value` when `id` is not read.

    Args:
        res: A PyArrow table, or pyogrio's `(meta, table)`.

    Returns:
        `(sum, count)`.
    """
    table = res[1] if isinstance(res, tuple) else res
    col = "id" if "id" in table.column_names else "value"
    return float(table.column(col).to_numpy().sum(dtype="float64")), table.num_rows


@contextlib.contextmanager
def staged_fixture(path: Path):
    """Stage one fixture independently and preserve an already completed publisher.

    Args:
        path: Final fixture path.

    Yields:
        Path: Unique staging path with the final format suffix.
    """
    with tempfile.TemporaryDirectory(prefix=f".{path.stem}-", dir=path.parent) as directory:
        staged = Path(directory) / path.name
        yield staged
        if staged.is_dir():
            try:
                staged.rename(path)
            except OSError as exc:
                if exc.errno not in (errno.EEXIST, errno.ENOTEMPTY) or not path.is_dir():
                    raise
        else:
            try:
                os.link(staged, path)
            except FileExistsError:
                pass


def publish(path: Path, write) -> None:
    """Publish a completed fixture without sharing another writer's staging files.

    Args:
        path: Final fixture path.
        write: Callable writing a fixture to its staging path.
    """
    if path.exists():
        return
    with staged_fixture(path) as staged:
        write(staged)


# =============================================================================================
# GeoTIFF


class GeoTiff:
    """GeoTIFF fixtures (COG and stripped, by codec, type and layout) and their reads."""

    # name: (dtype, bands, size, compress, predictor, layout, interleave, block)
    FIXTURES = {
        "u8_none": ("uint8", 1, 4096, "NONE", 1, "cog", "BAND", 512),
        "u8_rgb_jpeg": ("uint8", 3, 4096, "JPEG", 1, "cog", "PIXEL", 512),
        "u8_rgb_webp": ("uint8", 3, 4096, "WEBP", 1, "cog", "PIXEL", 512),
        "u8_rgb_deflate_band": ("uint8", 3, 4096, "DEFLATE", 2, "cog", "BAND", 512),
        "u16_deflate_p2": ("uint16", 1, 4096, "DEFLATE", 2, "cog", "BAND", 512),
        "u16_lzw_p2_t256": ("uint16", 1, 4096, "LZW", 2, "cog", "BAND", 256),
        "u16_zstd_p2": ("uint16", 1, 4096, "ZSTD", 2, "cog", "BAND", 512),
        "u16_12b_deflate_pixel": ("uint16", 12, 2048, "DEFLATE", 2, "cog", "PIXEL", 256),
        "u16_12b_zstd_band": ("uint16", 12, 2048, "ZSTD", 2, "cog", "BAND", 256),
        "u16_deflate_strip": ("uint16", 1, 4096, "DEFLATE", 2, "strip", "BAND", 0),
        "i16_lzw_strip": ("int16", 1, 4096, "LZW", 2, "strip", "BAND", 0),
        "f32_none": ("float32", 1, 4096, "NONE", 1, "cog", "BAND", 512),
        "f32_deflate_p3": ("float32", 1, 4096, "DEFLATE", 3, "cog", "BAND", 512),
        "f32_lzw_p3": ("float32", 1, 4096, "LZW", 3, "cog", "BAND", 512),
        "f32_zstd_p3": ("float32", 1, 4096, "ZSTD", 3, "cog", "BAND", 512),
        "f32_lerc_zstd": ("float32", 1, 4096, "LERC_ZSTD", 1, "cog", "BAND", 512),
        "f64_deflate_p3": ("float64", 1, 4096, "DEFLATE", 3, "cog", "BAND", 512),
    }
    # One window off the tile grid, 16 scattered windows through one handle, the whole raster, the
    # whole raster decimated 8x (an overview), 4 threads reading 4 of the 16 windows each.
    ACCESS = ["window", "windows16", "full", "overview", "threads4"]
    EXT = ".tif"

    @staticmethod
    def axes(name: str) -> dict:
        """The values of the matrix's axes for a fixture.

        Args:
            name: The fixture's name.

        Returns:
            Its `codec`, `dtype` and `layout`.
        """
        dtype, bands, _, compress, pred, layout, interleave, _ = GeoTiff.FIXTURES[name]
        return {
            "codec": compress + (f"+p{pred}" if pred > 1 else ""),
            "dtype": dtype,
            "layout": f"{layout}/{interleave.lower()}/{bands}b",
        }

    @staticmethod
    def make(out: Path) -> None:
        """Writes the format's fixtures that are not there yet, and its warm-up file.

        Args:
            out: The directory of the format's fixtures.
        """
        import numpy as np
        from osgeo import gdal

        gdal.UseExceptions()
        warm = gdal.GetDriverByName("MEM").Create("", 256, 256, 1)
        warm.SetGeoTransform((0, 1, 0, 0, 0, -1))
        publish(out / "_warmup.tif", lambda tmp: gdal.Translate(str(tmp), warm, format="GTiff", creationOptions=["COMPRESS=DEFLATE", "TILED=YES"]))
        types = {"uint8": "Byte", "uint16": "UInt16", "int16": "Int16", "float32": "Float32", "float64": "Float64"}
        for name, (dtype, bands, size, compress, pred, layout, interleave, block) in GeoTiff.FIXTURES.items():
            path = out / f"{name}.tif"
            if path.exists():
                continue
            mem = gdal.GetDriverByName("MEM").Create("", size, size, bands, gdal.GetDataTypeByName(types[dtype]))
            mem.SetGeoTransform((500000, 10, 0, 5000000, 0, -10))
            mem.SetProjection("EPSG:32631")
            for b, v in enumerate(terrain(np, size, bands, dtype)):
                mem.GetRasterBand(b + 1).WriteArray(v)
            opts = [f"COMPRESS={compress}", f"INTERLEAVE={interleave}"] + ([f"PREDICTOR={pred}"] if pred > 1 else [])
            if layout == "cog":
                opts += [f"BLOCKSIZE={block}", "OVERVIEW_RESAMPLING=AVERAGE"]
            driver = "COG" if layout == "cog" else "GTiff"
            publish(path, lambda tmp: gdal.Translate(str(tmp), mem, format=driver, creationOptions=opts))
            print(f"{path.name}: {path.stat().st_size >> 20} MB", flush=True)

    @staticmethod
    def meta(path: Path, access: str) -> dict | None:
        """What a read of a fixture needs, from the fixture's own chunk tables.

        Args:
            path: The fixture.
            access: The access pattern.

        Returns:
            `need_bytes` (and what the reader needs), or `None` when the access does not apply.
        """
        from osgeo import gdal

        gdal.UseExceptions()
        ds = gdal.Open(str(path))
        size = ds.RasterXSize
        if access == "overview" and ds.GetRasterBand(1).GetOverviewCount() == 0:
            return None
        level = None
        if access == "overview":  # the overview GDAL reads a decimation by 8 from
            level = min(range(ds.GetRasterBand(1).GetOverviewCount()), key=lambda i: abs(ds.GetRasterBand(1).GetOverview(i).XSize - size // 8))
        wins = {"window": windows(size, 1), "windows16": windows(size, 16), "threads4": windows(size, 16)}.get(access, [(0, 0, size, size)])
        # The bytes of the chunks the read touches, each counted once, from the TIFF block tables.
        need, seen = 0, set()
        for b in range(1, ds.RasterCount + 1):
            band = ds.GetRasterBand(b) if level is None else ds.GetRasterBand(b).GetOverview(level)
            bw, bh = band.GetBlockSize()
            f = size / band.XSize
            for x, y, w, h in wins:
                x, y, w, h = int(x / f), int(y / f), math.ceil(w / f), math.ceil(h / f)
                for by in range(y // bh, (y + h - 1) // bh + 1):
                    for bx in range(x // bw, (x + w - 1) // bw + 1):
                        off = band.GetMetadataItem(f"BLOCK_OFFSET_{bx}_{by}", "TIFF")
                        if off is not None and off not in seen:
                            seen.add(off)
                            need += int(band.GetMetadataItem(f"BLOCK_SIZE_{bx}_{by}", "TIFF") or 0)
        return {"size": size, "need_bytes": need}

    @staticmethod
    def reader(tool: str, access: str, meta: dict):
        """How a tool reads a fixture in an access pattern.

        Args:
            tool: `terrio` or `gdal`.
            access: The access pattern.
            meta: What `meta` returned for the fixture.

        Returns:
            `(open, items, check, warm)`: `open(path)` opens the source, each item reads one part of
            it, `check` turns an item's result into `(sum, count)` after the timed read, and `warm` is the
            open and read of the warm-up file.
        """
        size = meta["size"]
        if tool == "terrio":
            import terrio.rio as rio
            from terrio.rio.windows import Window

            def open_(p):
                return rio.open(p)

            def win(ds, w):
                return ds.read(window=Window(*w))

            def whole(ds):
                return ds.read()

            def decimated(ds):
                return ds.read(out_shape=(ds.count, size // 8, size // 8))
        else:
            from osgeo import gdal

            gdal.UseExceptions()

            def open_(p):
                return gdal.Open(p if p.startswith("/") else "/vsicurl/" + p)

            def win(ds, w):
                return ds.ReadAsArray(*w)

            def whole(ds):
                return ds.ReadAsArray()

            def decimated(ds):
                return ds.ReadAsArray(buf_xsize=size // 8, buf_ysize=size // 8)

        n = {"window": 1, "windows16": 16, "threads4": 16}.get(access)
        if n:
            items = [lambda ds, w=w: win(ds, w) for w in windows(size, n)]
        else:
            items = [whole if access == "full" else decimated]
        warm = (open_, lambda ds: win(ds, (0, 0, 64, 64)))
        return open_, items, checksum, warm


# =============================================================================================
# Zarr


class Zarr:
    """Zarr v2 and v3 fixtures (by codec, type, chunking and sharding) and their reads."""

    # name: (zarr_format, dtype, bands, size, chunk, shard, compressor)
    # compressor: none, zlib, gzip, zstd, blosc-<cname>-<shuffle>
    FIXTURES = {
        "v2_zlib_c512": (2, "float32", 1, 4096, 512, None, "zlib"),
        "v2_blosc_lz4_shuffle": (2, "float32", 1, 4096, 512, None, "blosc-lz4-shuffle"),
        "v2_blosc_zstd_bitshuffle": (2, "float32", 1, 4096, 512, None, "blosc-zstd-bitshuffle"),
        "v2_none_c512": (2, "float32", 1, 4096, 512, None, "none"),
        "v3_zstd_c256": (3, "float32", 1, 4096, 256, None, "zstd"),
        "v3_zstd_c512": (3, "float32", 1, 4096, 512, None, "zstd"),
        "v3_zstd_c1024": (3, "float32", 1, 4096, 1024, None, "zstd"),
        "v3_gzip_c512": (3, "float32", 1, 4096, 512, None, "gzip"),
        "v3_blosc_lz4_shuffle": (3, "float32", 1, 4096, 512, None, "blosc-lz4-shuffle"),
        "v3_zstd_u16": (3, "uint16", 1, 4096, 512, None, "zstd"),
        "v3_zstd_u8": (3, "uint8", 1, 4096, 512, None, "zstd"),
        "v3_zstd_f64": (3, "float64", 1, 4096, 512, None, "zstd"),
        "v3_shard2048_c256": (3, "float32", 1, 4096, 256, 2048, "zstd"),
        "v3_shard1024_c128": (3, "float32", 1, 4096, 128, 1024, "zstd"),
        # Cubes: 12 bands chunked one band per chunk, or all bands in each chunk.
        "v3_cube12_band_chunks": (3, "uint16", 12, 2048, (1, 512, 512), None, "zstd"),
        "v3_cube12_all_bands": (3, "uint16", 12, 2048, (12, 256, 256), None, "zstd"),
    }
    # As GeoTIFF's, with `band` (one band of a cube) in place of an overview.
    ACCESS = ["window", "windows16", "full", "band", "threads4"]
    EXT = ".zarr"

    @staticmethod
    def axes(name: str) -> dict:
        """The values of the matrix's axes for a fixture.

        Args:
            name: The fixture's name.

        Returns:
            Its `codec`, `dtype` and `layout`.
        """
        fmt, dtype, bands, _, chunk, shard, comp = Zarr.FIXTURES[name]
        side = chunk if isinstance(chunk, int) else "x".join(map(str, chunk))
        return {
            "codec": f"v{fmt}/{comp}",
            "dtype": dtype,
            "layout": f"{'shard ' + str(shard) + '/' if shard else ''}chunk {side}/{bands}b",
        }

    @staticmethod
    def _codecs(fmt: int, comp: str):
        """The zarr-python compressor of a fixture.

        Args:
            fmt: The Zarr format, 2 or 3.
            comp: `none`, `zlib`, `gzip`, `zstd` or `blosc-<cname>-<shuffle>`.

        Returns:
            The codec, or `None` for `none`.
        """
        from zarr.codecs import BloscCodec, GzipCodec, ZstdCodec

        if comp == "none":
            return None
        if fmt == 2:
            import numcodecs

            if comp.startswith("blosc"):
                _, cname, shuffle = comp.split("-")
                return numcodecs.Blosc(cname=cname, clevel=5, shuffle={"shuffle": 1, "bitshuffle": 2}[shuffle])
            return {"zlib": numcodecs.Zlib(level=6), "gzip": numcodecs.GZip(level=6), "zstd": numcodecs.Zstd(level=3)}[comp]
        if comp.startswith("blosc"):
            _, cname, shuffle = comp.split("-")
            return BloscCodec(cname=cname, clevel=5, shuffle=shuffle)
        return {"gzip": GzipCodec(level=6), "zstd": ZstdCodec(level=3)}[comp]

    @staticmethod
    def make(out: Path) -> None:
        """Writes the format's fixtures that are not there yet, and its warm-up file.

        Args:
            out: The directory of the format's fixtures.
        """
        import numpy as np
        import zarr

        def warmup(path):
            """Write the tiny initialization fixture.

            Args:
                path: Isolated staging directory.
            """
            w = zarr.create_array(str(path), shape=(256, 256), chunks=(256, 256), dtype="uint8", zarr_format=3, overwrite=True)
            w[:] = 1

        publish(out / "_warmup.zarr", warmup)
        for name, (fmt, dtype, bands, size, chunk, shard, comp) in Zarr.FIXTURES.items():
            path = out / f"{name}.zarr"
            if path.exists():
                continue
            data = terrain(np, size, bands, dtype)
            if bands == 1:
                data = data[0]
            chunks = (chunk, chunk) if isinstance(chunk, int) else chunk
            shards = (shard, shard) if shard else None

            def write(tmp, data=data, chunks=chunks, shards=shards, fmt=fmt, dtype=dtype, comp=comp, bands=bands):
                z = zarr.create_array(
                    str(tmp), shape=data.shape, chunks=chunks, shards=shards, dtype=dtype, zarr_format=fmt,
                    compressors=Zarr._codecs(fmt, comp), fill_value=0,
                    dimension_names=None if fmt == 2 else (["band", "y", "x"] if bands > 1 else ["y", "x"]),
                )
                z[:] = data

            publish(path, write)
            mb = sum(f.stat().st_size for f in path.rglob("*") if f.is_file()) >> 20
            print(f"{path.name}: {mb} MB", flush=True)

    @staticmethod
    def meta(path: Path, access: str) -> dict | None:
        """What a read of a fixture needs, from the fixture's own chunk tables.

        Args:
            path: The fixture.
            access: The access pattern.

        Returns:
            `need_bytes` (and what the reader needs), or `None` when the access does not apply.
        """
        fmt, dtype, bands, size, chunk, shard, _ = Zarr.FIXTURES[path.stem]
        if access == "band" and bands == 1:
            return None
        cb, ch = (1, chunk) if isinstance(chunk, int) else (chunk[0], chunk[1])
        wins = {"window": windows(size, 1), "windows16": windows(size, 16), "threads4": windows(size, 16)}.get(access, [(0, 0, size, size)])
        band_sel = [5] if access == "band" else range(bands)
        need, seen = 0, set()

        def key(b, cy, cx):
            idx = ([b] if bands > 1 else []) + [cy, cx]
            return ".".join(map(str, idx)) if fmt == 2 else "c/" + "/".join(map(str, idx))

        for b in sorted({b // cb for b in band_sel}):
            for x, y, w, h in wins:
                for cy in range(y // ch, (y + h - 1) // ch + 1):
                    for cx in range(x // ch, (x + w - 1) // ch + 1):
                        if (b, cy, cx) in seen:
                            continue
                        seen.add((b, cy, cx))
                        if not shard:
                            f = path / key(b, cy, cx)
                            need += f.stat().st_size if f.exists() else 0
                            continue
                        # Sharded: the inner chunk's bytes, from the shard's index at its end:
                        # (offset, nbytes) little-endian uint64 pairs, then a CRC32C.
                        per = shard // ch
                        f = path / key(b, cy // per, cx // per)
                        n = per * per
                        with open(f, "rb") as fh:
                            fh.seek(-(16 * n + 4), 2)
                            index = struct.unpack(f"<{2 * n}Q", fh.read(16 * n))
                        i = (cy % per) * per + cx % per
                        need += index[2 * i + 1] if index[2 * i] != 2**64 - 1 else 0
                        if ("index", f) not in seen:
                            seen.add(("index", f))
                            need += 16 * n + 4
        return {"size": size, "bands": bands, "need_bytes": need}

    @staticmethod
    def reader(tool: str, access: str, meta: dict):
        """How a tool reads a fixture in an access pattern.

        Args:
            tool: `terrio` or `gdal`.
            access: The access pattern.
            meta: What `meta` returned for the fixture.

        Returns:
            `(open, items, check, warm)`: `open(path)` opens the source, each item reads one part of
            it, `check` turns an item's result into `(sum, count)` after the timed read, and `warm` is the
            open and read of the warm-up file.
        """
        size = meta["size"]
        if tool == "terrio":
            import terrio

            def open_(p):
                return terrio.open_raster(p, lazy=True)

            def win(ds, w):
                return ds.read(window=w)

            def whole(ds):
                return ds.read()

            def band(ds):
                return ds.read(6)
        else:
            from osgeo import gdal

            gdal.UseExceptions()

            def open_(p):
                return gdal.Open(p if p.startswith("/") else f'ZARR:"/vsicurl/{p}"')

            def win(ds, w):
                return ds.ReadAsArray(*w)

            def whole(ds):
                return ds.ReadAsArray()

            def band(ds):
                return ds.GetRasterBand(6).ReadAsArray()

        n = {"window": 1, "windows16": 16, "threads4": 16}.get(access)
        if n:
            items = [lambda ds, w=w: win(ds, w) for w in windows(size, n)]
        else:
            items = [whole if access == "full" else band]
        warm = (open_, lambda ds: win(ds, (0, 0, 64, 64)))
        return open_, items, checksum, warm


# =============================================================================================
# GeoParquet


class GeoParquet:
    """GeoParquet fixtures (by geometry, encoding, compression and row groups) and their reads."""

    # name: (geometry, rows, row_group, compression, encoding, covering bbox, sorted by bbox)
    FIXTURES = {
        "poly_wkb_rg64k_zstd_bbox_sorted": ("polygon", 400_000, 65536, "ZSTD", "WKB", True, True),
        "poly_wkb_rg64k_zstd_bbox_unsorted": ("polygon", 400_000, 65536, "ZSTD", "WKB", True, False),
        "poly_wkb_rg64k_zstd_nobbox_sorted": ("polygon", 400_000, 65536, "ZSTD", "WKB", False, True),
        "poly_wkb_rg64k_snappy_bbox_sorted": ("polygon", 400_000, 65536, "SNAPPY", "WKB", True, True),
        "poly_wkb_rg8k_zstd_bbox_sorted": ("polygon", 400_000, 8192, "ZSTD", "WKB", True, True),
        "poly_wkb_rg1m_zstd_bbox_sorted": ("polygon", 400_000, 1_000_000, "ZSTD", "WKB", True, True),
        "poly_geoarrow_rg64k_zstd_sorted": ("polygon", 400_000, 65536, "ZSTD", "GEOARROW", False, True),
        "point_wkb_rg64k_zstd_bbox_sorted": ("point", 2_000_000, 65536, "ZSTD", "WKB", True, True),
        "point_geoarrow_rg64k_zstd_sorted": ("point", 2_000_000, 65536, "ZSTD", "GEOARROW", False, True),
        "line_wkb_rg64k_zstd_bbox_sorted": ("line", 400_000, 65536, "ZSTD", "WKB", True, True),
    }
    # Every column; the geometry and two attributes; a bbox of 1% of the extent; 16 such bboxes
    # through one handle (a reader of tiles); 4 threads reading 4 of the 16 each; an attribute
    # filter `id < 4000`: 1% of polygons and lines, 0.2% of points.
    ACCESS = ["full", "columns", "bbox", "bbox16", "threads4", "where"]
    EXT = ".parquet"
    EXTENT = (0.0, 0.0, 100_000.0, 100_000.0)

    @staticmethod
    def axes(name: str) -> dict:
        """The values of the matrix's axes for a fixture.

        Args:
            name: The fixture's name.

        Returns:
            Its `codec`, `dtype` and `layout`.
        """
        geom, _, rg, comp, enc, cover, sort = GeoParquet.FIXTURES[name]
        return {
            "codec": f"{enc}/{comp}",
            "dtype": geom,
            "layout": f"rg {rg}/{'bbox' if cover else 'no bbox'}/{'sorted' if sort else 'unsorted'}",
        }

    @staticmethod
    def sources(out: Path) -> dict[str, Path]:
        """Writes the unindexed FlatGeobuf files the vector fixtures are converted from, one per
        geometry type, that are not there yet.

        Args:
            out: The directory of the format's fixtures.

        Returns:
            The path of each geometry type's source.
        """
        import numpy as np
        from osgeo import gdal, ogr, osr

        gdal.UseExceptions()
        x0, y0, x1, y1 = GeoParquet.EXTENT
        srs = osr.SpatialReference()
        srs.ImportFromEPSG(32631)
        sources = {}
        for geom in ("polygon", "point", "line"):
            n = max(r for g, r, *_ in GeoParquet.FIXTURES.values() if g == geom)
            src = out / f"_src_{geom}.fgb"
            sources[geom] = src
            if src.exists():
                continue
            with staged_fixture(src) as staged:
                rng = np.random.default_rng(2)
                # Clustered, like fields or buildings: centres drawn around 200 towns.
                towns = rng.uniform((x0, y0), (x1, y1), (200, 2))
                c = towns[rng.integers(0, 200, n)] + rng.normal(0, 2500, (n, 2))
                c = np.clip(c, (x0, y0), (x1, y1))
                ds = ogr.GetDriverByName("FlatGeobuf").CreateDataSource(str(staged))
                try:
                    kind = {"polygon": ogr.wkbPolygon, "point": ogr.wkbPoint, "line": ogr.wkbLineString}[geom]
                    lyr = ds.CreateLayer("data", srs, kind, ["SPATIAL_INDEX=NO"])
                    for fname, ftype in (("id", ogr.OFTInteger64), ("value", ogr.OFTReal), ("class", ogr.OFTString), ("year", ogr.OFTInteger)):
                        lyr.CreateField(ogr.FieldDefn(fname, ftype))
                    defn = lyr.GetLayerDefn()
                    classes = ["cropland", "grassland", "forest", "urban", "water", "wetland", "bare"]
                    values, years, cls = rng.gamma(2, 50, n), rng.integers(2000, 2026, n), rng.integers(0, 7, n)
                    nv = rng.integers(5, 24, n)
                    radius = rng.uniform(10, 80, n)
                    lyr.StartTransaction()
                    for i in range(n):
                        f = ogr.Feature(defn)
                        f["id"], f["value"], f["class"], f["year"] = i, float(values[i]), classes[cls[i]], int(years[i])
                        cx, cy = c[i]
                        if geom == "point":
                            g = ogr.CreateGeometryFromWkt(f"POINT ({cx:.2f} {cy:.2f})")
                        else:
                            a = np.sort(rng.uniform(0, 2 * np.pi, nv[i]))
                            r = radius[i] * rng.uniform(0.6, 1.0, nv[i])
                            xs, ys = cx + r * np.cos(a), cy + r * np.sin(a)
                            pts = ", ".join(f"{x:.2f} {y:.2f}" for x, y in zip(xs, ys, strict=True))
                            if geom == "polygon":
                                g = ogr.CreateGeometryFromWkt(f"POLYGON (({pts}, {xs[0]:.2f} {ys[0]:.2f}))")
                            else:
                                g = ogr.CreateGeometryFromWkt(f"LINESTRING ({pts})")
                        f.SetGeometry(g)
                        lyr.CreateFeature(f)
                    lyr.CommitTransaction()
                finally:
                    lyr = None
                    ds = None
        return sources

    @staticmethod
    def make(out: Path) -> None:
        """Writes the format's fixtures that are not there yet, and its warm-up file.

        Args:
            out: The directory of the format's fixtures.
        """
        from osgeo import gdal

        gdal.UseExceptions()
        sources = GeoParquet.sources(out)
        for name, (geom, rows, rg, comp, enc, cover, sort) in GeoParquet.FIXTURES.items():
            path = out / f"{name}.parquet"
            if path.exists():
                continue
            lco = [f"ROW_GROUP_SIZE={rg}", f"COMPRESSION={comp}", f"GEOMETRY_ENCODING={enc}", f"WRITE_COVERING_BBOX={'YES' if cover else 'NO'}", f"SORT_BY_BBOX={'YES' if sort else 'NO'}"]
            publish(path, lambda tmp: gdal.VectorTranslate(str(tmp), str(sources[geom]), format="Parquet", layerCreationOptions=lco, limit=rows))
            print(f"{path.name}: {path.stat().st_size >> 20} MB", flush=True)
        publish(out / "_warmup.parquet", lambda tmp: gdal.VectorTranslate(str(tmp), str(sources["point"]), format="Parquet", limit=100))

    @staticmethod
    def bboxes(n: int) -> list[tuple[float, float, float, float]]:
        """The n boxes of 1/10 x 1/10 of the extent (1% of its area), spread over it.

        Args:
            n: The number of boxes.

        Returns:
            `(min_x, min_y, max_x, max_y)` of each box.
        """
        x0, y0, x1, y1 = GeoParquet.EXTENT
        w, h = (x1 - x0) / 10, (y1 - y0) / 10
        corners = [(x0 + (x1 - x0 - w) * ((i * 0.37 + 0.45) % 1), y0 + (y1 - y0 - h) * ((i * 0.61 + 0.45) % 1)) for i in range(n)]
        return [(bx, by, bx + w, by + h) for bx, by in corners]

    COLUMNS = ["value", "class"]
    WHERE = "id < 4000"  # 1% of the polygons or lines, 0.2% of the points

    @staticmethod
    def meta(path: Path, access: str) -> dict | None:
        """What a read of a fixture needs, from the fixture's own chunk tables.

        Args:
            path: The fixture.
            access: The access pattern.

        Returns:
            `need_bytes` (and what the reader needs), or `None` when the access does not apply.
        """
        import pyarrow.parquet as pq

        pf = pq.ParquetFile(path)
        md = pf.metadata
        geo = json.loads(md.metadata[b"geo"])
        gcol = geo["primary_column"]
        names = [md.schema.column(i).path for i in range(md.num_columns)]
        # The covering bbox columns, as the file's metadata names them (GDAL: `<geometry>_bbox`).
        cover = geo["columns"][gcol].get("covering", {}).get("bbox")
        boxes = GeoParquet.bboxes({"bbox": 1, "bbox16": 16, "threads4": 16}.get(access, 0))
        need = md.serialized_size + 8  # the footer
        for g in range(md.num_row_groups):
            rg = md.row_group(g)
            cols = {names[i]: rg.column(i) for i in range(md.num_columns)}
            if boxes and cover:
                stats = {k: cols[".".join(cover[k])].statistics for k in ("xmin", "ymin", "xmax", "ymax")}
                if all(st is not None and st.has_min_max for st in stats.values()):
                    s = {k: (st.min, st.max) for k, st in stats.items()}
                    hits = sum(1 for bx0, by0, bx1, by1 in boxes if s["xmin"][0] <= bx1 and s["xmax"][1] >= bx0 and s["ymin"][0] <= by1 and s["ymax"][1] >= by0)
                    if not hits:
                        continue
            if access == "where":
                st = cols["id"].statistics
                if st is not None and st.has_min_max and st.min >= 4000:
                    continue
            keep = [n for n in names if n.split(".")[0] in (gcol, *GeoParquet.COLUMNS)] if access == "columns" else names
            need += sum(cols[n].total_compressed_size for n in keep)
        return {"need_bytes": need}

    @staticmethod
    def reader(tool: str, access: str, meta: dict):
        """How a tool reads a fixture in an access pattern.

        Args:
            tool: `terrio` or `gdal`.
            access: The access pattern.
            meta: What `meta` returned for the fixture.

        Returns:
            `(open, items, check, warm)`: `open(path)` opens the source, each item reads one part of
            it, `check` turns an item's result into `(sum, count)` after the timed read, and `warm` is the
            open and read of the warm-up file.
        """
        if tool == "terrio":
            import terrio.pyogrio as po

            def open_(p):
                return p  # a GeoParquet read opens the file: nothing is held between reads
        else:
            import pyogrio as po

            def open_(p):
                return p if p.startswith("/") else "/vsicurl/" + p

        def read(p, **kw):
            return po.read_arrow(p, **kw)

        boxes = GeoParquet.bboxes({"bbox": 1, "bbox16": 16, "threads4": 16}.get(access, 0))
        if boxes:
            items = [lambda p, b=b: read(p, bbox=b) for b in boxes]
        elif access == "columns":
            items = [lambda p: read(p, columns=GeoParquet.COLUMNS)]
        elif access == "where" and tool == "terrio":  # terrio.pyogrio has no `where`: terrio's SQL
            import pyarrow as pa
            import terrio

            def where(p):
                layer = p.rsplit("/", 1)[-1].removesuffix(".parquet")
                return pa.table(terrio.sql(p, f"SELECT * FROM {layer} WHERE {GeoParquet.WHERE}"))

            items = [where]
        elif access == "where":
            items = [lambda p: read(p, where=GeoParquet.WHERE)]
        else:
            items = [lambda p: read(p)]
        warm = (open_, lambda p: read(p))
        return open_, items, table_checksum, warm


# =============================================================================================
# FlatGeobuf


class FlatGeobuf:
    """FlatGeobuf fixtures (by geometry, with or without a spatial index) and their reads."""

    # name: (geometry, rows, spatial index); the features of GeoParquet's sources
    FIXTURES = {
        "poly_index": ("polygon", 400_000, True),
        "poly_noindex": ("polygon", 400_000, False),
        "point_index": ("point", 2_000_000, True),
        "point_noindex": ("point", 2_000_000, False),
        "line_index": ("line", 400_000, True),
        "line_noindex": ("line", 400_000, False),
    }
    # Every column; the geometry and two attributes (features are stored whole: the same bytes); a
    # bbox of 1% of the extent; 16 such bboxes. The boxes and columns are GeoParquet's.
    ACCESS = ["full", "columns", "bbox", "bbox16"]
    EXT = ".fgb"
    NODE_SIZE = 16  # of the packed R-tree GDAL writes (reference: GDAL 3.13.3)

    @staticmethod
    def axes(name: str) -> dict:
        """The values of the matrix's axes for a fixture.

        Args:
            name: The fixture's name.

        Returns:
            Its `codec`, `dtype` and `layout`.
        """
        geom, rows, index = FlatGeobuf.FIXTURES[name]
        return {"codec": "flatgeobuf", "dtype": geom, "layout": f"{'index' if index else 'no index'}/{rows}"}

    @staticmethod
    def make(out: Path) -> None:
        """Writes the format's fixtures that are not there yet, and its warm-up file.

        Args:
            out: The directory of the format's fixtures.
        """
        from osgeo import gdal

        gdal.UseExceptions()
        sources = GeoParquet.sources(out)
        for name, (geom, rows, index) in FlatGeobuf.FIXTURES.items():
            path = out / f"{name}.fgb"
            if path.exists():
                continue
            lco = [f"SPATIAL_INDEX={'YES' if index else 'NO'}"]
            publish(path, lambda tmp: gdal.VectorTranslate(str(tmp), str(sources[geom]), format="FlatGeobuf", layerCreationOptions=lco, limit=rows))
            print(f"{path.name}: {path.stat().st_size >> 20} MB", flush=True)
        publish(out / "_warmup.fgb", lambda tmp: gdal.VectorTranslate(str(tmp), str(sources["point"]), format="FlatGeobuf", limit=100))

    @staticmethod
    def levels(count: int) -> list[tuple[int, int]]:
        """The node ranges `[start, end)` of each level of a packed R-tree, the leaves first.

        Args:
            count: The number of features.

        Returns:
            The ranges; the root's is last, and first in the file.

        Raises:
            ValueError: If there are no features, or the header does not give their count (-1).
        """
        if count < 0:
            raise ValueError("a spatial index of an unknown feature count")
        if count == 0:
            raise ValueError("a spatial index of no features")
        counts, n = [count], count
        while True:  # at least two levels, as the reference implementation lays out one feature
            n = -(-n // FlatGeobuf.NODE_SIZE)
            counts.append(n)
            if n == 1:
                break
        out, end = [], sum(counts)
        for c in counts:
            out.append((end - c, end))
            end -= c
        return out

    @staticmethod
    def meta(path: Path, access: str) -> dict | None:
        """What a read of a fixture needs, from the fixture's own header, index and features.

        The header and every feature, for a whole read or a bbox without an index; for a bbox
        through the index, the header, the index nodes on the way and the features found. As in the
        other formats, a node or feature that several of `bbox16`'s boxes touch counts once.

        Args:
            path: The fixture.
            access: The access pattern.

        Returns:
            `need_bytes`.

        Raises:
            ValueError: If the index does not describe the file's features.
        """
        import numpy as np
        from osgeo import ogr

        _, _, index = FlatGeobuf.FIXTURES[path.stem]
        size = path.stat().st_size
        if not index:
            return {"need_bytes": size}
        ogr.UseExceptions()
        ds = ogr.Open(str(path))
        count = ds.GetLayer(0).GetFeatureCount()  # from the header
        ds = None
        levels = FlatGeobuf.levels(count)
        item = np.dtype([("x0", "<f8"), ("y0", "<f8"), ("x1", "<f8"), ("y1", "<f8"), ("off", "<u8")])
        # Only the bytes used are read: the runner's peak memory is the floor of each worker's.
        with open(path, "rb") as f:
            fd = f.fileno()
            header = 12 + struct.unpack("<I", os.pread(fd, 4, 8))[0]  # magic bytes, header length, header
            num_nodes = levels[0][1]  # the leaves end the index: every node precedes the data
            data = header + num_nodes * 40

            def nodes(a, b):
                """The index nodes `[a, b)`."""
                return np.frombuffer(os.pread(fd, (b - a) * 40, header + a * 40), item)

            def feature(off):
                """The size of the feature at `off` in the data section, its size prefix included."""
                return 4 + struct.unpack("<I", os.pread(fd, 4, data + off))[0]

            # The features follow the leaves: the first leaf's starts the data, the last's ends the file.
            first, last = int(nodes(levels[0][0], levels[0][0] + 1)["off"][0]), int(nodes(num_nodes - 1, num_nodes)["off"][0])
            if first != 0 or data + last + feature(last) != size:
                raise ValueError(f"{path.name}: the spatial index does not describe the features")
            boxes = GeoParquet.bboxes({"bbox": 1, "bbox16": 16}.get(access, 0))
            if not boxes:  # every feature, after the index
                return {"need_bytes": size - (data - header)}
            visited, hits = set(), set()
            for bx0, by0, bx1, by1 in boxes:
                current = [levels[-1]]
                for level in range(len(levels) - 1, -1, -1):
                    below = []
                    for a, b in current:
                        visited.add((a, b))
                        n = nodes(a, b)
                        found = n["off"][(n["x0"] <= bx1) & (n["y0"] <= by1) & (n["x1"] >= bx0) & (n["y1"] >= by0)]
                        if level == 0:
                            hits.update(found.tolist())
                        else:  # the index of the child node's first item
                            below += [(int(o), min(int(o) + FlatGeobuf.NODE_SIZE, levels[level - 1][1])) for o in found]
                    current = below
            return {"need_bytes": header + sum(b - a for a, b in visited) * 40 + sum(feature(o) for o in hits)}

    @staticmethod
    def reader(tool: str, access: str, meta: dict):
        """How a tool reads a fixture in an access pattern.

        terrio reads a bbox with `open_vector`, the reader that searches the spatial index, and
        every feature or two columns with `open_stream`, the reader that selects columns. GDAL reads
        all three with pyogrio's `read_arrow`.

        Args:
            tool: `terrio` or `gdal`.
            access: The access pattern.
            meta: What `meta` returned for the fixture.

        Returns:
            `(open, items, check, warm)`: `open(path)` opens the source, each item reads one part of
            it, `check` turns an item's result into `(sum, count)` after the timed read, and `warm` is the
            open and read of the warm-up file.
        """
        if tool == "terrio":
            import pyarrow as pa
            import terrio

            def open_(p):
                return p  # nothing is held between reads

            def read(p, bbox=None, columns=None):
                if bbox is not None:
                    return pa.table(terrio.open_vector(p, bbox=bbox).layers[0])
                return pa.table(terrio.open_stream(p, columns=columns))
        else:
            import pyogrio

            def open_(p):
                return p if p.startswith("/") else "/vsicurl/" + p

            def read(p, **kw):
                return pyogrio.read_arrow(p, **kw)

        boxes = GeoParquet.bboxes({"bbox": 1, "bbox16": 16}.get(access, 0))
        if boxes:
            items = [lambda p, b=b: read(p, bbox=b) for b in boxes]
        elif access == "columns":
            items = [lambda p: read(p, columns=GeoParquet.COLUMNS)]
        else:
            items = [lambda p: read(p)]
        warm = (open_, lambda p: read(p))
        return open_, items, table_checksum, warm


FORMATS = {"geotiff": GeoTiff, "zarr": Zarr, "geoparquet": GeoParquet, "flatgeobuf": FlatGeobuf}

# The source files each format's reads go through, as fnmatch patterns from the repository root
# (`*` crosses directories): a pull request that changes one is worth that format's matrix. The
# shared I/O, threading and dependency files count for every format.
_SHARED = [
    "Cargo.lock",
    "crates/terrio/src/http*",
    "crates/terrio/src/block_cache.rs",
    "crates/terrio/src/par.rs",
    "crates/terrio/src/vfs.rs",
    "crates/terrio/src/vsi*",
    "crates/terrio/src/memory*",
    "crates/terrio/src/zstandard.rs",
]
PATHS = {
    "geotiff": [
        "crates/terrio/src/geotiff.rs",
        "crates/terrio/src/cog.rs",
        "crates/terrio/src/tiff_*",
        "crates/terrio/src/float_read.rs",
        "crates/terrio/src/resampled_read.rs",
        "crates/terrio/src/sparse.rs",
        "crates/terrio/src/jpeg.rs",
        "crates/terrio/src/deflate_compress.rs",
        "crates/terrio/src/drivers/webp*",
        "crates/terrio/src/drivers/gdal_overview.rs",
        "python/python/terrio/rio/*",
        "python/src/pixel.rs",
        "python/src/raster.rs",
    ],
    "zarr": [
        "crates/terrio/src/drivers/zarr*",
        "crates/terrio/src/multidim/*",
        "python/src/multidim.rs",
        "python/src/raster.rs",
    ],
    "geoparquet": [
        "crates/terrio/src/drivers/vector/geoparquet*",
        "crates/terrio/src/drivers/vector/wkb.rs",
        "crates/terrio/src/parquet_metadata.rs",
        "crates/terrio/src/arrow.rs",
        "crates/terrio/src/stream/*",
        "python/src/stream.rs",
        "python/src/arrow.rs",
        "python/python/terrio/pyogrio/*",
    ],
    "flatgeobuf": [
        "crates/terrio/src/drivers/vector/flatgeobuf.rs",
        "crates/terrio/src/drivers/vector/mod.rs",
        "crates/terrio/src/arrow.rs",
        "crates/terrio/src/stream/*",
        "python/src/stream.rs",
        "python/src/vector.rs",
        "python/src/arrow.rs",
    ],
}


def touched(paths: list[str]) -> list[str]:
    """The formats whose reads go through any of `paths`.

    Args:
        paths: Changed files, relative to the repository root.

    Returns:
        The names of the formats, in the order of `FORMATS`.
    """
    import fnmatch

    hit = lambda patterns: any(fnmatch.fnmatch(p, pat) for p in paths for pat in patterns)  # noqa: E731
    return [f for f in FORMATS if hit(PATHS[f] + _SHARED)]


# =============================================================================================
# HTTP server with a simulated round-trip time, logging every request.

_lock = threading.Condition()
_log: list[tuple[float, float, int, str]] = []  # (start, end, bytes, path)
_busy = 0  # requests being served: the runner reads the log once none is


def _settled_log() -> list[tuple[float, float, int, str]]:
    """Wait until every received request has been recorded.

    Returns:
        The completed request log.

    Raises:
        TimeoutError: If handlers remain active after the accounting timeout.
    """
    with _lock:
        if not _lock.wait_for(lambda: _busy == 0, timeout=10):
            raise TimeoutError("HTTP handlers did not finish accounting")
        return list(_log)


class _Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    latency = 0.02

    def log_message(self, *args):
        pass

    def do_HEAD(self):
        self._serve(True)

    def do_GET(self):
        self._serve(False)

    def _serve(self, head: bool) -> None:
        """Answers a GET or HEAD request, with byte ranges, after the simulated round-trip time.

        Args:
            head: Whether the request is a HEAD request (no body).
        """
        global _busy
        t0 = time.perf_counter()
        sent = 0
        with _lock:
            _busy += 1
        try:
            time.sleep(self.latency)
            path = self.translate_path(self.path)
            if not os.path.isfile(path):
                self.send_error(404)
                return
            size = os.path.getsize(path)
            start, end, status = 0, size - 1, 200
            rng = self.headers.get("Range", "")
            if rng.startswith("bytes="):
                try:
                    a, sep, b = rng[6:].partition("-")
                    if not sep or (not a and not b) or (a and not a.isdigit()) or (b and not b.isdigit()):
                        raise ValueError("unsupported range")
                    start, end = (max(0, size - int(b)), size - 1) if a == "" else (int(a), min(size - 1, int(b)) if b else size - 1)
                    if start > end or start >= size:
                        raise ValueError("unsatisfiable range")
                except ValueError:
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                status = 206
            self.send_response(status)
            self.send_header("Content-Length", str(end - start + 1))
            self.send_header("Accept-Ranges", "bytes")
            if status == 206:
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.end_headers()
            if not head:
                with open(path, "rb") as f:
                    f.seek(start)
                    self.wfile.write(f.read(end - start + 1))
                    sent = end - start + 1
        finally:
            with _lock:
                _log.append((t0, time.perf_counter(), sent, self.path))
                _busy -= 1
                _lock.notify_all()


def rounds(log: list[tuple[float, float, int, str]]) -> int:
    """The sequential rounds of HTTP requests in a log.

    A request that starts after every request of the current round has ended waited for them, so it
    opens a new round.

    Args:
        log: `(start, end, bytes, path)` of each request.

    Returns:
        The number of rounds.
    """
    n, end = 0, -1.0
    for s, e, *_ in sorted(log):
        if s >= end:
            n += 1
            end = e
        else:
            end = max(end, e)
    return n


# =============================================================================================
# Worker: one fresh process per sample, so caches, pools and counters start empty.


PROC_IO = Path("/proc/self/io")  # Linux: bytes a process read, for local sources


def _rchar(local: bool) -> int:
    """The bytes this process has read so far (Linux's `rchar`).

    Args:
        local: Whether the source is a local path: an HTTP source counts 0, as the server counts its bytes.

    Returns:
        The bytes read.
    """
    if not local:  # over HTTP the runner counts the bytes the server sent
        return 0
    d = dict(line.split(": ") for line in PROC_IO.read_text().splitlines())
    return int(d["rchar"])


def worker(spec: dict) -> dict:
    """Reads one case in this process, timed: one sample of one tool.

    Args:
        spec: The format, tool, source, access pattern and fixture metadata of the case.

    Returns:
        The sample: init, open and read times, bytes, peak memory, and the values' sum and count.
    """
    from concurrent.futures import ThreadPoolExecutor

    fmt = FORMATS[spec["format"]]
    src, access = spec["src"], spec["access"]
    local = src.startswith("/")
    # `base` is another terrio build, found first on PYTHONPATH
    tool = "terrio" if spec["tool"] == "base" else spec["tool"]
    ti = time.perf_counter()
    open_, items, check, (warm_open, warm_read) = fmt.reader(tool, access, spec["meta"])
    # A process's first open and read pays one-time costs (library initialization, thread pools):
    # measured on a tiny file apart from the case, as `init`.
    warm_read(warm_open(src.rsplit("/", 1)[0] + "/_warmup" + fmt.EXT))
    r0 = _rchar(local)
    t0 = time.perf_counter()
    if access == "threads4":  # four handles, opened in the threads and timed as the read

        def job(k):
            d = open_(src)  # a handle per thread: GDAL datasets are not thread-safe
            return [(i, items[i](d)) for i in range(k, len(items), 4)]

        t1, r1 = t0, r0
        with ThreadPoolExecutor(4) as pool:  # a job's exception is raised here
            out = [res for _, res in sorted((x for part in pool.map(job, range(4)) for x in part), key=lambda x: x[0])]
    else:
        ds = open_(src)
        t1, r1 = time.perf_counter(), _rchar(local)
        out = [item(ds) for item in items]
    t2 = time.perf_counter()
    r2 = _rchar(local)
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / (1024**2 if sys.platform == "darwin" else 1024)
    sums = [check(x) for x in out]  # after the timed read and its peak memory
    return {
        "init_s": t0 - ti,
        "open_s": t1 - t0,
        "read_s": t2 - t1,
        "open_bytes": r1 - r0,
        "read_bytes": r2 - r1,
        "rss_mb": rss,
        "sum": sum(s for s, _ in sums),
        "n": sum(n for _, n in sums),
    }


# =============================================================================================
# Runner


def _read(path: str) -> str:
    """A text file's content, stripped.

    Args:
        path: The file's path.

    Returns:
        The content, or an empty string when the file cannot be read.
    """
    try:
        return Path(path).read_text().strip()
    except OSError:
        return ""


def machine() -> dict:
    """What the timings depend on: compare ratios across runs only on the same machine."""
    import platform

    cpu = next((line.split(":", 1)[1].strip() for line in _read("/proc/cpuinfo").splitlines() if line.startswith("model name")), platform.processor())
    cgroup = _read("/proc/self/cgroup").rpartition(":")[2]
    ac = [_read(f"{d}/online") for d in Path("/sys/class/power_supply").glob("*") if _read(f"{d}/type") == "Mains"]
    versions = {}
    for mod in ("terrio", "osgeo.gdal", "pyogrio", "zarr"):
        try:
            m = __import__(mod, fromlist=["_"])
            versions[mod] = getattr(m, "__version__", None) or m.VersionInfo("RELEASE_NAME")
        except Exception:  # noqa: BLE001 - absent for this format
            pass
    return {
        "machine": {
            "cpu": cpu,
            "cpus": len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count(),
            "cpu_quota": _read(f"/sys/fs/cgroup{cgroup}/cpu.max") or None,  # "max" or "<quota> <period>"
            "mem_gb": round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30, 1),
            "os": platform.platform(),
            "on_ac": None if not ac else "1" in ac,
            "load": os.getloadavg()[0],
            "github_runner": os.environ.get("RUNNER_NAME"),
            "versions": versions,
            "time": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        }
    }


def run(args: argparse.Namespace) -> None:
    """Measures every case of a format and prints the report.

    Args:
        args: The parsed `run` command line.
    """
    with contextlib.ExitStack() as cleanup:
        fmt = FORMATS[args.format]
        data = args.dir
        _Handler.latency = args.latency_ms / 1000
        server = ThreadingHTTPServer(("127.0.0.1", 0), lambda *a, **k: _Handler(*a, directory=str(data), **k))
        server.daemon_threads = True
        cleanup.callback(server.server_close)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        cleanup.callback(server.shutdown)
        base = f"http://127.0.0.1:{server.server_address[1]}/"

        env = dict(os.environ)
        env.update(
            RAYON_NUM_THREADS=str(args.threads),
            GDAL_NUM_THREADS=str(args.threads),
            GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR",
            GDAL_HTTP_MERGE_CONSECUTIVE_RANGES="YES",
            GDAL_CACHEMAX="512",
        )
        names = [f for f in fmt.FIXTURES if not args.only or any(o in f for o in args.only)]
        if not names:
            sys.exit("no fixtures match --only")
        samples = 1 if args.quick else args.samples
        results = args.out or data / f"results-{time.strftime('%Y%m%d-%H%M%S')}.jsonl"
        sources = args.source or ["local", "http"]
        if "local" in sources and not PROC_IO.exists():
            sys.exit(f"local byte counts need {PROC_IO} (Linux): use --source http")
        head = machine()
        tools = ["terrio", "gdal"]
        envs = {"terrio": env, "gdal": env}
        if args.base:
            base_dir = args.base.expanduser().resolve()
            envs["base"] = {**env, "PYTHONPATH": os.pathsep.join(filter(None, [str(base_dir), env.get("PYTHONPATH")]))}
            tools.append("base")
            version = subprocess.run([sys.executable, "-c", "import json, terrio; print(json.dumps([terrio.__file__, terrio.__version__]))"], env=envs["base"], capture_output=True, text=True, check=True)
            module_file, module_version = json.loads(version.stdout)
            if not Path(module_file).resolve().is_relative_to(base_dir):
                sys.exit(f"--base {base_dir}: terrio is imported from {version.stdout.strip()}, not from it")
            head["machine"]["base"] = f"{module_file} {module_version}"
        if head["machine"]["on_ac"] is False and not args.allow_battery:
            sys.exit("on battery, timings are slower and vary: plug in, or pass --allow-battery")
        try:
            out_file = open(results, "x")  # one run per file: report sums every row of a file
        except FileExistsError:
            sys.exit(f"{results} exists: choose another --out")
        cleanup.callback(out_file.close)
        out_file.write(json.dumps(head) + "\n")
        rows = []
        for case_index, (name, access, source) in enumerate(itertools.product(names, args.access or fmt.ACCESS, sources)):
            path = data / f"{name}{fmt.EXT}"
            label = "where/sql-vs-filter" if args.format == "geoparquet" and access == "where" else access
            try:
                meta = fmt.meta(path, access)
            except Exception as exc:
                row = {"format": args.format, "fixture": name, "access": label, "source": source,
                       "meta_error": f"{type(exc).__name__}: {exc}", **fmt.axes(name)}
                rows.append(row)
                print(fmt_row(row), flush=True)
                out_file.write(json.dumps(row) + "\n")
                out_file.flush()
                continue
            if meta is None:
                continue
            src = str(path) if source == "local" else base + path.name
            row = {"format": args.format, "fixture": name, "access": label, "source": source, "need_bytes": meta["need_bytes"], **fmt.axes(name)}
            for k in range(samples):
                start = (case_index + k) % len(tools)
                for tool in tools[start:] + tools[:start]:  # rotate even in one-sample runs
                    if f"{tool}_error" in row:  # failed or timed out once: not again
                        continue
                    try:
                        _settled_log()  # a killed worker's requests end before the log is cleared
                        with _lock:
                            _log.clear()
                        spec = {"format": args.format, "tool": tool, "src": src, "access": access, "meta": meta}
                        cmd = [sys.executable, __file__, "_worker", json.dumps(spec)]
                        p = subprocess.run(cmd, env=envs[tool], capture_output=True, text=True, timeout=args.timeout)
                        if p.returncode:
                            err = p.stderr.strip().splitlines()
                            row[f"{tool}_error"] = err[-1][:300] if err else f"exit {p.returncode}"
                            continue
                        r = json.loads(p.stdout.strip().splitlines()[-1])
                        # this case's requests, not the warm-up's
                        log = [x for x in _settled_log() if x[3].split("?")[0].startswith("/" + path.name)]
                        if source == "http":
                            r["requests"], r["rounds"] = len(log), rounds(log)
                            r["read_bytes"], r["open_bytes"] = sum(x[2] for x in log), 0  # transferred, open included
                    except subprocess.TimeoutExpired:
                        row[f"{tool}_error"] = f"timeout: over {args.timeout:.0f} s"
                        continue
                    except (TimeoutError, ValueError, IndexError) as exc:
                        row[f"{tool}_error"] = f"{type(exc).__name__}: {exc}"
                        continue
                    row.setdefault(tool, []).append(r)
            rows.append(summarize(row))
            print(fmt_row(rows[-1]), flush=True)
            out_file.write(json.dumps(rows[-1]) + "\n")
            out_file.flush()
        print(f"\n{results}")
        print(report([head, *rows]))


def summarize(row: dict) -> dict:
    """A case's medians per tool, its time ratios and whether its values match GDAL's.

    A time ratio only when every sample of terrio and GDAL succeeded (a tool
    stops at its first failure), and a ratio to the base only when the base's did too: a base that
    failed where this build succeeds is listed as such. `values` is `checksum_match` only when every
    sample's sum and count equal GDAL's first, else the first that differs.

    Args:
        row: A case's row, with the samples of each tool as lists and the errors as `*_error`.

    Returns:
        The row with each tool's medians in place of its samples, the ratios and `values`.
    """
    out = {k: v for k, v in row.items() if not isinstance(v, list)}
    tools = [t for t in ("terrio", "gdal", "base") if t in row]
    for tool in tools:
        rs = row.get(tool)
        if not rs:
            continue
        out[tool] = {k: statistics.median(r[k] for r in rs) for k in rs[0] if k not in ("sum", "n")}
        out[tool]["total_s"] = statistics.median(r["open_s"] + r["read_s"] for r in rs)
        out[f"{tool}_sum"], out[f"{tool}_n"] = rs[0]["sum"], rs[0]["n"]
    base_ok = "base" in out and "base_error" not in row
    if base_ok and "gdal" in out and "gdal_error" not in row:
        ref = row["gdal"][0]
        out["base_values_match"] = all(
            r["n"] == ref["n"] and math.isclose(r["sum"], ref["sum"], rel_tol=1e-6)
            for tool in ("base", "gdal") for r in row[tool]
        )
    if "terrio" in out and "gdal" in out and not [e for e in _errors(row) if not e.startswith("base ")]:
        out["time_ratio"] = out["terrio"]["total_s"] / out["gdal"]["total_s"]
        ref = row["gdal"][0]
        if base_ok:
            out["base_ratio"] = out["terrio"]["total_s"] / out["base"]["total_s"]
        else:
            tools = [t for t in tools if t != "base"]
        differ = next(((t, r) for t in tools for r in row[t] if r["n"] != ref["n"] or not math.isclose(r["sum"], ref["sum"], rel_tol=1e-6)), None)
        out["values"] = "checksum_match" if differ is None else f"DIFFER: {differ[0]} n {differ[1]['n']} sum {differ[1]['sum']:.6g} vs gdal n {ref['n']} sum {ref['sum']:.6g}"
    return out


def _errors(r: dict) -> list[str]:
    """The errors recorded in a row, each prefixed by what failed.

    Args:
        r: A case's row.

    Returns:
        `"<tool> <error>"` for each `*_error` key.
    """
    return [f"{k.removesuffix('_error')} {v}" for k, v in r.items() if k.endswith("_error")]


def _bytes(t: dict) -> int:
    """The bytes a tool read or transferred, open included.

    Args:
        t: A tool's medians in a row.

    Returns:
        The bytes.
    """
    return int(t["open_bytes"] + t["read_bytes"])


def fmt_row(r: dict) -> str:
    """One line of the report for a case.

    Args:
        r: The case's summarized row.

    Returns:
        The line.
    """
    t, g = r.get("terrio"), r.get("gdal")
    s = f"{r['fixture']:34} {r['access']:9} {r['source']:5}"
    if "time_ratio" not in r:
        return s + "  error: " + (" | ".join(_errors(r)) or "no sample")
    s += f" {t['total_s'] * 1000:8.1f} ms vs {g['total_s'] * 1000:8.1f} x{r['time_ratio']:5.2f}"
    s += f" | KiB {_bytes(t) >> 10:7} vs {_bytes(g) >> 10:7} need {r['need_bytes'] >> 10:7}"
    if "requests" in t:
        s += f" | req {t['requests']:4.0f}/{g['requests']:4.0f} rounds {t['rounds']:3.0f}/{g['rounds']:3.0f}"
    label = "checksum_match" if r["values"] in ("same", "checksum_match") else "DIFFER"
    s += f" | init {t['init_s'] * 1000:3.0f}/{g['init_s'] * 1000:3.0f} ms rss {t['rss_mb']:4.0f}/{g['rss_mb']:4.0f} MB {label}"
    if "base_ratio" in r:
        s += f" | base {r['base']['total_s'] * 1000:8.1f} ms x{r['base_ratio']:5.2f}"
    return s


def report(rows: list[dict]) -> str:
    """The report of a run: rankings, geometric means by axis, I/O and failures.

    Only cases whose values equal GDAL's are ranked; mismatches and failures are listed apart.

    Args:
        rows: Summarized rows, and the machine line of each results file.

    Returns:
        The report's text.
    """
    heads = [r["machine"] for r in rows if "machine" in r]
    rows = [r for r in rows if "machine" not in r]
    lines = []
    for h in heads:
        lines.append(f"machine: {h['cpu']}, {h['cpus']} CPUs (quota {h['cpu_quota']}), {h['mem_gb']} GB, {h['os']}, AC {h['on_ac']}, load {h['load']:.1f}, {h['versions']}")
    if len({(h["cpu"], h["cpus"], h["cpu_quota"], h["mem_gb"]) for h in heads}) > 1:
        lines.append("WARNING: these results come from different machines: compare ratios within one machine only")
    computed = [r for r in rows if "time_ratio" in r]
    ok = [r for r in computed if r["values"] in ("same", "checksum_match")]
    differ = [r for r in computed if r["values"] not in ("same", "checksum_match")]
    based = [r for r in ok if "base_ratio" in r]
    if based:
        gm = math.exp(statistics.fmean(math.log(r["base_ratio"]) for r in based))
        lines.append(f"\n## This build against the base (terrio time / base time; geometric mean x{gm:.3f}, {len(based)} cases)\n")
        for title, rs in (
            ("slower by 10% or more", sorted((r for r in based if r["base_ratio"] >= 1.1), key=lambda r: -r["base_ratio"])),
            ("faster by 10% or more", sorted((r for r in based if r["base_ratio"] <= 1 / 1.1), key=lambda r: r["base_ratio"])),
        ):
            lines.append(f"{title}: {len(rs)}")
            lines += ["  " + fmt_row(r) for r in rs[:20]]
    if any("base_error" in r or "base" in r for r in rows):
        fixed = [r for r in ok if "base_error" in r]
        broken = [r for r in rows if "terrio_error" in r and r.get("base_values_match") is True]
        lines.append(f"the base failed, this build succeeds: {len(fixed)}")
        lines += [f"  {fmt_row(r)} | base {r['base_error']}" for r in fixed]
        lines.append(f"this build fails, the base succeeded: {len(broken)}")
        lines += ["  " + fmt_row(r) for r in broken]
    lines.append("\n## Worst cases (terrio time / GDAL time)\n")
    lines += [fmt_row(r) for r in sorted(ok, key=lambda r: -r["time_ratio"])[:20]]
    lines.append("\n## Geometric mean of the time ratios by axis (>1: terrio slower; cases in brackets)\n")
    for axis in ("format", "access", "source", "codec", "dtype", "layout"):
        groups: dict[str, list[float]] = {}
        for r in ok:
            groups.setdefault(r[axis], []).append(r["time_ratio"])
        cells = sorted(((math.exp(statistics.fmean(map(math.log, v))), k, len(v)) for k, v in groups.items()), reverse=True)
        lines.append(f"{axis:7} " + "  ".join(f"{k} x{gm:.2f}({n})" for gm, k, n in cells))
    http = [r for r in ok if r["source"] == "http"]
    more_rounds = [r for r in http if r["terrio"]["rounds"] > r["gdal"]["rounds"]]
    more_bytes = [r for r in ok if _bytes(r["terrio"]) > 1.25 * _bytes(r["gdal"]) + 65536]
    over_need = [r for r in ok if _bytes(r["terrio"]) > 1.5 * r["need_bytes"] + 262144]
    lines.append(f"\n## I/O\n\nmore request rounds than GDAL: {len(more_rounds)}/{len(http)}")
    lines += ["  " + fmt_row(r) for r in more_rounds]
    lines.append(f"more bytes than GDAL (>1.25x + 64 KiB): {len(more_bytes)}/{len(ok)}")
    lines += ["  " + fmt_row(r) for r in more_bytes]
    lines.append(f"more than 1.5x the bytes needed (+ 256 KiB): {len(over_need)}/{len(ok)}")
    lines += ["  " + fmt_row(r) for r in over_need]
    errors = [r for r in rows if "time_ratio" not in r]
    lines.append(f"\nvalues differ from GDAL (not ranked): {len(differ)}" + "".join(f"\n  {r['fixture']}/{r['access']}/{r['source']} x{r['time_ratio']:.2f}: {r['values']}" for r in differ))
    lines.append(f"errors: {len(errors)}" + "".join("\n  " + fmt_row(r) for r in errors))
    return "\n".join(lines)


def main() -> None:
    """Runs the command line: `fixtures`, `run`, `touched`, `report` or a worker."""
    if len(sys.argv) > 2 and sys.argv[1] == "_worker":
        print(json.dumps(worker(json.loads(sys.argv[2]))))
        return
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("fixtures", "run"):
        s = sub.add_parser(name, allow_abbrev=False)
        s.add_argument("--format", choices=list(FORMATS), default="geotiff")
        s.add_argument("--dir", type=Path, default=Path("~/.cache/terrio-bench/matrix"), help="fixtures in DIR/FORMAT")
    r = sub.choices["run"]
    r.add_argument("--only", nargs="*", help="fixture name substrings")
    r.add_argument("--access", nargs="*", help="access patterns (default: all of the format's)")
    r.add_argument("--source", nargs="*", choices=["local", "http"])
    r.add_argument("--threads", type=int, default=4, help="RAYON_NUM_THREADS and GDAL_NUM_THREADS")
    r.add_argument("--latency-ms", type=float, default=20, help="simulated round-trip time of the HTTP source")
    r.add_argument("--samples", type=int, default=3, help="samples per tool, in alternating order; the median")
    r.add_argument("--quick", action="store_true", help="one sample per tool")
    r.add_argument("--timeout", type=float, default=120, help="seconds a read may take before it is reported as a timeout")
    r.add_argument("--out", type=Path, help="results JSONL, a new file (default: DIR/FORMAT/results-<time>.jsonl)")
    r.add_argument("--base", type=Path, help="another terrio build to time too: a directory with its terrio package")
    r.add_argument("--allow-battery", action="store_true", help="run on battery power (timings slower and noisier)")
    sub.add_parser("touched", help="the formats whose files the paths on standard input change, one a line")
    rep = sub.add_parser("report")
    rep.add_argument("results", type=Path, nargs="+", help="results files, one or several formats")
    a = p.parse_args()
    if a.cmd == "run":
        for name in ("samples", "threads", "timeout"):
            value = getattr(a, name)
            if not math.isfinite(value) or value <= 0:
                p.error(f"--{name} must be positive and finite")
    if a.cmd == "touched":
        print(" ".join(touched([line.strip() for line in sys.stdin if line.strip()])))
        return
    if a.cmd == "report":
        print(report([json.loads(line) for f in a.results for line in f.read_text().splitlines() if line.strip()]))
        return
    if a.cmd == "run" and a.access and (bad := set(a.access) - set(FORMATS[a.format].ACCESS)):
        p.error(f"--access {' '.join(sorted(bad))}: {a.format} reads are {' '.join(FORMATS[a.format].ACCESS)}")
    a.dir = a.dir.expanduser().resolve() / a.format  # absolute: the readers take a relative path as a URL
    a.dir.mkdir(parents=True, exist_ok=True)
    if a.cmd == "fixtures":
        FORMATS[a.format].make(a.dir)
    else:
        run(a)


if __name__ == "__main__":
    main()
