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

Formats: geotiff, zarr, geoparquet, flatgeobuf, geopackage; geotiff-write and geoparquet-write:
writes of the GeoTIFF and GeoParquet fixtures, each compared with GDAL's output; the operations
warp and terrain, which run on a GeoTIFF fixture and compare their output with GDAL's pixel by
pixel. Needs terrio's Python module (a release build: `maturin develop --release` in python/),
GDAL's Python bindings, pyogrio and PyArrow (GeoParquet, FlatGeobuf, GeoPackage) and zarr-python 3
(Zarr fixtures) in one environment. Fixtures and results stay in --dir, out of the repository.
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


def _arrow_canonical(t):
    """An Arrow type with the widest integer, float, string and binary types and nullable fields.

    Args:
        t: The type, with any extension type unwrapped.

    Returns:
        The type every tool's column of the same values casts to.
    """
    import pyarrow as pa

    if pa.types.is_integer(t):
        return pa.uint64() if t == pa.uint64() else pa.int64()  # uint64 past 2**63 does not fit int64
    if pa.types.is_floating(t):
        return pa.float64()
    if pa.types.is_string(t) or pa.types.is_large_string(t):
        return pa.large_string()
    if pa.types.is_binary(t) or pa.types.is_large_binary(t):
        return pa.large_binary()
    if pa.types.is_list(t) or pa.types.is_large_list(t) or pa.types.is_fixed_size_list(t):
        return pa.large_list(_arrow_canonical(t.value_type))
    if pa.types.is_struct(t):
        return pa.struct([pa.field(f.name, _arrow_canonical(f.type)) for f in t])
    return t


def digest(results) -> str:
    """A hash of every value a case read, in order, to compare two tools' reads exactly.

    A raster's hash covers its data type, its shape without leading 1s (a band read as `(1, h, w)`
    and as `(h, w)` hash the same) and its bytes in C order. A table's covers its columns by name,
    each cast to a canonical type (`int32` and `int64` columns of the same values hash the same, as
    do chunked and sliced copies; `uint64` stays apart, so its values past 2**63 hash) and
    serialized as Arrow IPC; top-level extension columns hash as their storage. Nested extension types and dictionary-encoded columns hash as they come, so equal
    values in different encodings only fall back to the sum and count. Read metadata (the
    `metadata` of a `(metadata, table)`, such as the CRS) is not hashed: only values are compared.

    Args:
        results: What each of a case's items returned: arrays, tables or `(metadata, table)`.

    Returns:
        The hex BLAKE2b digest.
    """
    import hashlib

    import numpy as np

    h = hashlib.blake2b(digest_size=16)
    for res in results:
        table = res[1] if isinstance(res, tuple) else res
        if hasattr(table, "column_names"):
            import pyarrow as pa

            names = sorted(table.column_names)
            cols = []
            for name in names:
                col = table.column(name)
                if isinstance(col.type, pa.BaseExtensionType):
                    col = pa.chunked_array([c.storage for c in col.chunks], col.type.storage_type)
                cols.append(col.cast(_arrow_canonical(col.type)).combine_chunks())
            batch = pa.RecordBatch.from_arrays(cols, names=names)
            # A serialized batch holds no schema: the names and canonical types are hashed apart.
            h.update(batch.schema.serialize())
            h.update(batch.serialize())
        else:
            a = np.ascontiguousarray(res)
            shape = a.shape
            while len(shape) > 2 and shape[0] == 1:  # a band's (1, h, w) hashes as (h, w)
                shape = shape[1:]
            h.update(repr((a.dtype.str, shape)).encode())
            h.update(memoryview(a).cast("B"))
    return h.hexdigest()


def gdal_read_arrow(path: str, **kw):
    """`pyogrio.read_arrow`'s table, its geometry column named `geometry` as terrio names it.

    The digest compares columns by name, and GDAL names the geometry column as the file does
    (`geom` in a GeoPackage) or, unnamed, pyogrio's `wkb_geometry` (FlatGeobuf); the rename copies
    nothing.

    Args:
        path: The dataset's path.
        **kw: `read_arrow`'s options.

    Returns:
        The table.
    """
    import pyogrio

    info, table = pyogrio.read_arrow(path, **kw)
    geometry = info.get("geometry_name") or "wkb_geometry"  # pyogrio's name for an unnamed column
    return table.rename_columns(["geometry" if c == geometry else c for c in table.column_names])


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
    # whole raster decimated 8x (an overview), N threads sharing the 16 windows (`THREAD_COUNTS`).
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
    # through one handle (a reader of tiles); N threads sharing the 16 (`THREAD_COUNTS`); an attribute
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
        """Writes the FlatGeobuf files the vector fixtures are translated from, if not there yet.

        Clustered features (fields or buildings around 200 towns) with `id`, `value`, `class` and
        `year`, in `id` order, as many of each geometry as GeoParquet's largest fixture of it has.

        Args:
            out: The directory of the format's fixtures.

        Returns:
            The file of each geometry: `polygon`, `point` and `line`.
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
        while True:  # the root is a level of its own, even for a few features
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
            def open_(p):
                return p if p.startswith("/") else "/vsicurl/" + p

            def read(p, **kw):
                return gdal_read_arrow(p, **kw)

        boxes = GeoParquet.bboxes({"bbox": 1, "bbox16": 16}.get(access, 0))
        if boxes:
            items = [lambda p, b=b: read(p, bbox=b) for b in boxes]
        elif access == "columns":
            items = [lambda p: read(p, columns=GeoParquet.COLUMNS)]
        else:
            items = [lambda p: read(p)]
        warm = (open_, lambda p: read(p))
        return open_, items, table_checksum, warm


# =============================================================================================
# Operations: an operation's output, in memory, against GDAL's, compared pixel by pixel.


def compare_raster(out, ref, tol: float) -> dict:
    """terrio's output of an operation against GDAL's, pixel by pixel.

    The rules are `scripts/compare_ops_with_gdal.py`'s, but stricter for nodata: that script
    compares only the pixels valid in GDAL's output, while here every pixel counts, nodata
    included: equal values (NaN with NaN) agree, and a pixel differs by `|terrio - GDAL|`
    (infinite where one is NaN). A different shape, type, CRS, geotransform
    (by more than 1e-6 pixel) or nodata value (in the output's type) puts every pixel beyond the
    tolerance.

    Args:
        out: terrio's output, a `terrio.Raster`.
        ref: GDAL's output, a `gdal.Dataset`.
        tol: The largest difference a pixel may have.

    Returns:
        `n` pixels, `differing` (not equal), `beyond_tol`, `max_diff` (finite differences),
        `tol` and `mismatch` (what else differs, or an empty string).
    """
    import numpy as np
    from osgeo import osr

    g = ref.ReadAsArray().reshape(ref.RasterCount, ref.RasterYSize, ref.RasterXSize)
    t = np.asarray(out.read())
    t = t.reshape(-1, *t.shape[-2:])
    nd, gnd = out.nodata, ref.GetRasterBand(1).GetNoDataValue()
    gt, ggt = out.transform, ref.GetGeoTransform()
    srs = osr.SpatialReference()
    if out.crs_wkt:
        srs.ImportFromWkt(out.crs_wkt)

    def held(v):  # a float nodata as the pixels hold it: 3.402823466e+38 is float32's largest
        return float(np.asarray(v).astype(g.dtype)) if v is not None and g.dtype.kind == "f" else v

    a, b = held(nd), held(gnd)
    same_nodata = a == b or (a is not None and b is not None and math.isnan(a) and math.isnan(b))
    mismatch = [
        what for what, bad in (
            (f"shape {t.shape} vs {g.shape}", t.shape != g.shape),
            (f"type {t.dtype} vs {g.dtype}", t.dtype != g.dtype),
            ("crs", not srs.IsSame(ref.GetSpatialRef() or osr.SpatialReference(), [
                "IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES", "CRITERION=EQUIVALENT_EXCEPT_AXIS_ORDER_GEOGCRS"])),
            (f"geotransform {gt} vs {ggt}", max(abs(a - b) for a, b in zip(gt, ggt, strict=True)) > 1e-6 * abs(ggt[1])),
            (f"nodata {nd} vs {gnd}", not same_nodata),
        ) if bad
    ]
    res = {"n": int(g.size), "tol": tol, "mismatch": "; ".join(mismatch)}
    if t.shape != g.shape:
        return {**res, "differing": res["n"], "beyond_tol": res["n"], "max_diff": math.inf}
    with np.errstate(invalid="ignore"):
        diff = np.abs(t.astype("float64") - g)
    diff[(t == g) | (np.isnan(t) & np.isnan(g))] = 0
    diff[np.isnan(diff)] = np.inf
    finite = diff[np.isfinite(diff)]
    return {**res, "differing": int(np.count_nonzero(diff)), "max_diff": float(finite.max()) if finite.size else 0.0,
            "beyond_tol": res["n"] if mismatch else int((diff > tol).sum())}


class _Operation:
    """An operation on the GeoTIFF fixtures of a DEM, timed from the path to the output in memory.

    Each access pattern is one operation with its options (`CASES`); `_t1` cases run it with one
    thread in both tools. A worker runs the operation on the warm-up file first, for its one-time
    costs (CRS database, thread pools). After the timing, a terrio sample computes GDAL's output in
    its process and compares the two pixel by pixel (`compare_raster`).
    """

    COMPARES_OUTPUT = True  # `check` compares the output with GDAL's: the worker hashes no values
    DIR = "geotiff"  # the inputs are GeoTIFF fixtures
    EXT = ".tif"
    SOURCES = ["local"]
    FIXTURES = {"f32_none": GeoTiff.FIXTURES["f32_none"]}  # a 4096 x 4096 float32 DEM, uncompressed
    CASES: dict = {}
    make = staticmethod(GeoTiff.make)
    axes = staticmethod(GeoTiff.axes)

    @staticmethod
    def meta(path: Path, access: str) -> dict:
        """What an operation reads: the whole of the fixture's full resolution.

        Args:
            path: The fixture.
            access: The operation.

        Returns:
            `need_bytes` and the fixture's `path`.
        """
        return {"need_bytes": GeoTiff.meta(path, "full")["need_bytes"], "path": str(path)}

    @classmethod
    def reader(cls, tool: str, access: str, meta: dict):
        """How a tool runs an operation on a fixture.

        Args:
            tool: `terrio` or `gdal`.
            access: The operation.
            meta: What `meta` returned for the fixture.

        Returns:
            `(open, items, check, warm)` as the formats' readers return them; `check` returns
            `(pixels beyond the tolerance, pixels, compare_raster's result)` for terrio.
        """
        if access.endswith("_t1"):  # before terrio's and GDAL's thread pools start
            os.environ["RAYON_NUM_THREADS"] = os.environ["GDAL_NUM_THREADS"] = "1"
        run = cls.op(tool, access)  # a terrio worker loads GDAL only for the check, after the timing

        def check(out):
            if tool == "gdal":
                return 0.0, out.RasterCount * out.RasterXSize * out.RasterYSize
            c = compare_raster(out, cls.op("gdal", access)(meta["path"]), cls.CASES[access][-1])
            return float(c["beyond_tol"]), c["n"], c

        return (lambda p: p), [run], check, ((lambda p: p), lambda p: run(p, warm=True))


class Warp(_Operation):
    """Reprojection of a UTM DEM to WGS 84 longitudes and latitudes, as `gdalwarp -t_srs EPSG:4326`.

    Reference: GDAL 3.13.3.
    """

    # access: (resampling, output width (0: GDAL's default, about the source's resolution), tolerance)
    # Tolerances as compare_ops_with_gdal.py's: nearest exact, interpolating resamplings 1e-2.
    CASES = {
        "near": ("near", 0, 0.0),
        "bilinear": ("bilinear", 0, 1e-2),
        "cubic": ("cubic", 0, 1e-2),
        "bilinear_t1": ("bilinear", 0, 1e-2),
        "bilinear_ts8192": ("bilinear", 8192, 1e-2),  # about 2x upsampled
        "bilinear_ts1024": ("bilinear", 1024, 1e-2),  # about 4x downsampled, from an overview
        "average_ts1024": ("average", 1024, 1e-2),  # GDAL weighs footprint-edge pixels differently (#703)
    }
    ACCESS = list(CASES)

    @staticmethod
    def op(tool: str, access: str):
        """An operation's call.

        terrio reproduces GDAL 3.13's kernels and chunk-dependent sampling (`kernel_version="3.13"`,
        `grid_invariant=False`): its default, grid-invariant sampling gives other values.

        Args:
            tool: `terrio` or `gdal`.
            access: The operation.

        Returns:
            `op(path, warm=False)`, which returns the output in memory; `warm` runs it on the
            warm-up file, which has no CRS, at its own size.
        """
        resampling, width, _ = Warp.CASES[access]
        threads = os.environ.get("GDAL_NUM_THREADS", "1")
        if tool == "terrio":
            import terrio

            def op(p, warm=False):
                size = {"ts": (width, 0)} if width and not warm else {}
                return terrio.warp(p, s_srs="EPSG:32631" if warm else None, t_srs="EPSG:4326", resampling=resampling,
                                   kernel_version="3.13", grid_invariant=False, **size)
        else:
            from osgeo import gdal

            gdal.UseExceptions()

            def op(p, warm=False):
                size = {"width": width, "height": 0} if width and not warm else {}
                return gdal.Warp("", p, format="MEM", srcSRS="EPSG:32631" if warm else None, dstSRS="EPSG:4326",
                                 resampleAlg=resampling, multithread=threads != "1",
                                 warpOptions=[f"NUM_THREADS={threads}"], **size)
        return op


class Terrain(_Operation):
    """Terrain products of a DEM, as `gdaldem`, and the raster analyses `gdal_proximity` and `gdal_calc`.

    Reference: GDAL 3.13.3.
    """

    FIXTURES = {**_Operation.FIXTURES, "u8_none": GeoTiff.FIXTURES["u8_none"]}
    # access: (input fixture, operation, its argument, tolerance)
    CASES = {
        "slope": ("f32_none", "slope", "3.13", 0.0),  # the argument: terrio's GDAL arithmetic
        "hillshade": ("f32_none", "hillshade", "3.13", 1.0),  # GDAL 3.13 rounds hillshade per CPU (SIMD): 1 DN
        # terrio has no GDAL 3.13 aspect: the 3.12 arithmetic differs by float32 rounding (one ulp is
        # 3.05e-5 degrees from 256 to 360)
        "aspect": ("f32_none", "aspect", "3.12", 1e-4),
        "slope_t1": ("f32_none", "slope", "3.13", 0.0),
        "hillshade_t1": ("f32_none", "hillshade", "3.13", 1.0),
        "aspect_t1": ("f32_none", "aspect", "3.12", 1e-4),
        # pixel distances to the 8-bit fixture's 255s, as `gdal_proximity.py -values 255`
        "proximity": ("u8_none", "proximity", "255", 0.0),
        "proximity_t1": ("u8_none", "proximity", "255", 0.0),
        "calc": ("f32_none", "calc", "sqrt(abs(A)) * 2 + A / 3", 0.0),  # as `gdal_calc.py --calc`
    }
    ACCESS = list(CASES)

    @classmethod
    def meta(cls, path: Path, access: str) -> dict | None:
        """What an operation reads, for its own input fixture only.

        Args:
            path: The fixture.
            access: The operation.

        Returns:
            As `_Operation.meta`, or `None` for another operation's input.
        """
        return _Operation.meta(path, access) if path.stem == cls.CASES[access][0] else None

    @staticmethod
    def op(tool: str, access: str):
        """An operation's call.

        Args:
            tool: `terrio` or `gdal`.
            access: The operation.

        Returns:
            `op(path, warm=False)`, which returns the output in memory.
        """
        _, what, arg, _ = Terrain.CASES[access]
        if tool == "terrio":
            import terrio

            if what == "proximity":
                return lambda p, warm=False: terrio.proximity(p, values=[int(arg)])
            if what == "calc":
                return lambda p, warm=False: terrio.calc(arg, A=p)
            return lambda p, warm=False: terrio.dem(p, what, kernel_version=arg)
        from osgeo import gdal

        gdal.UseExceptions()

        if what == "proximity":

            def proximity(p, warm=False):
                src = gdal.Open(p)
                out = gdal.GetDriverByName("MEM").Create("", src.RasterXSize, src.RasterYSize, 1, gdal.GDT_Float32)
                out.SetGeoTransform(src.GetGeoTransform())
                out.SetProjection(src.GetProjection())
                gdal.ComputeProximity(src.GetRasterBand(1), out.GetRasterBand(1), [f"VALUES={arg}"])
                return out

            return proximity
        if what == "calc":
            from osgeo_utils.gdal_calc import Calc

            return lambda p, warm=False: Calc(arg, A=p, outfile="", format="MEM", quiet=True)
        return lambda p, warm=False: gdal.DEMProcessing("", p, what, format="MEM")



# ======================================================================================# GeoPackage (vector)


def _varint(b: bytes, i: int) -> int:
    """An SQLite variable-length integer.

    Args:
        b: The bytes.
        i: Where the integer starts.

    Returns:
        Its value.
    """
    v = 0
    for k in range(8):
        v = (v << 7) | (b[i + k] & 0x7F)
        if b[i + k] < 0x80:
            return v
    return (v << 8) | b[i + 8]


class GeoPackage:
    """GeoPackage feature tables (by geometry, R-tree and spatial order of the rows) and their reads.

    terrio reads a local file a page at a time (`terrio.open_stream`) and downloads a URL whole
    first, where GDAL reads pages by range requests. Its `where` is `terrio.sql`, which reads the
    whole file through `terrio.open_vector`.
    """

    # name: (geometry, rows, R-tree, rows sorted along a Hilbert curve)
    FIXTURES = {
        "poly_rtree_sorted": ("polygon", 400_000, True, True),
        "poly_rtree_unsorted": ("polygon", 400_000, True, False),
        "poly_nortree_sorted": ("polygon", 400_000, False, True),
        "line_rtree_sorted": ("line", 400_000, True, True),
        "point_rtree_sorted": ("point", 2_000_000, True, True),
        "point_rtree_unsorted": ("point", 2_000_000, True, False),
    }
    # GeoParquet's: every column; the geometry and two attributes; a bbox of 1% of the extent; 16
    # bboxes; N threads sharing the 16 (`THREAD_COUNTS`); the attribute filter `id < 4000`.
    ACCESS = ["full", "columns", "bbox", "bbox16", "threads4", "where"]
    EXT = ".gpkg"
    LAYER = "data"

    @staticmethod
    def axes(name: str) -> dict:
        """The values of the matrix's axes for a fixture.

        Args:
            name: The fixture's name.

        Returns:
            Its `codec`, `dtype` and `layout`.
        """
        geom, rows, rtree, sort = GeoPackage.FIXTURES[name]
        return {
            "codec": "rtree" if rtree else "no rtree",
            "dtype": geom,
            "layout": f"{rows // 1000}k rows/{'sorted' if sort else 'unsorted'}",
        }

    @staticmethod
    def make(out: Path) -> None:
        """Writes the format's fixtures that are not there yet, and its warm-up file.

        Args:
            out: The directory of the format's fixtures.
        """
        from osgeo import gdal

        gdal.UseExceptions()
        sources = GeoParquet.sources(out)  # rows in `id` order: unsorted in space
        for name, (geom, rows, rtree, sort) in GeoPackage.FIXTURES.items():
            path = out / f"{name}.gpkg"
            if path.exists():
                continue

            def write(tmp, src=str(sources[geom]), rows=rows, rtree=rtree, sort=sort, name=name):
                lco = [f"SPATIAL_INDEX={'YES' if rtree else 'NO'}"]
                if not sort:
                    gdal.VectorTranslate(str(tmp), src, format="GPKG", layerName=GeoPackage.LAYER, layerCreationOptions=lco, limit=rows)
                    return
                # FlatGeobuf's spatial index orders the features along a Hilbert curve.
                hilbert = f"/vsimem/{name}.fgb"
                try:
                    gdal.VectorTranslate(hilbert, src, format="FlatGeobuf", layerCreationOptions=["SPATIAL_INDEX=YES"], limit=rows)
                    gdal.VectorTranslate(str(tmp), hilbert, format="GPKG", layerName=GeoPackage.LAYER, layerCreationOptions=lco)
                finally:
                    gdal.Unlink(hilbert)

            publish(path, write)
            print(f"{path.name}: {path.stat().st_size >> 20} MB", flush=True)
        publish(out / "_warmup.gpkg", lambda tmp: gdal.VectorTranslate(str(tmp), str(sources["point"]), format="GPKG", layerName=GeoPackage.LAYER, limit=100))

    @staticmethod
    def leaves(f, page_size: int, root: int) -> list[tuple[float, int, tuple[int, ...]]]:
        """The leaf pages of a table b-tree, in rowid order, from its interior pages.

        Args:
            f: The database file, opened in binary.
            page_size: Its page size.
            root: The table's root page.

        Returns:
            `(largest rowid it can hold, page, interior pages above it)` of each leaf.
        """
        out = []

        def walk(page: int, hi: float, up: tuple[int, ...]) -> None:
            f.seek((page - 1) * page_size)
            p = f.read(page_size)
            h = 100 if page == 1 else 0  # page 1 starts with the file header
            if p[h] == 0x0D:
                out.append((hi, page, up))
                return
            if p[h] != 0x05:
                raise ValueError(f"page {page} is not a table b-tree page")
            n, right = struct.unpack_from(">H", p, h + 3)[0], struct.unpack_from(">I", p, h + 8)[0]
            for i in range(n):
                cell = struct.unpack_from(">H", p, h + 12 + 2 * i)[0]
                # (left child, key): the child holds the rowids up to the key
                walk(struct.unpack_from(">I", p, cell)[0], _varint(p, cell + 4), (*up, page))
            walk(right, hi, (*up, page))

        walk(root, math.inf, ())
        return out

    @staticmethod
    def rtree(con, name: str, boxes) -> tuple[set[int], int]:
        """The rowids an R-tree gives for boxes, and the bytes of the nodes it visits for them.

        Args:
            con: The database's sqlite3 connection.
            name: The R-tree (`rtree_<table>_<column>`), read from its `_node` table.
            boxes: `(min_x, min_y, max_x, max_y)` of each box.

        Returns:
            The rowids whose boxes intersect any of them, and the visited nodes' bytes.
        """

        def node(n):
            return con.execute(f'SELECT data FROM "{name}_node" WHERE nodeno = ?', (n,)).fetchone()[0]

        root = node(1)
        ids, need, stack = set(), 0, [(root, struct.unpack_from(">H", root)[0])]  # (node, height)
        while stack:
            data, height = stack.pop()
            need += len(data)
            for i in range(struct.unpack_from(">H", data, 2)[0]):
                # (rowid or child node, min x, max x, min y, max y), big-endian float32 bounds
                key, x0, x1, y0, y1 = struct.unpack_from(">q4f", data, 4 + 24 * i)
                if any(x0 <= bx1 and x1 >= bx0 and y0 <= by1 and y1 >= by0 for bx0, by0, bx1, by1 in boxes):
                    if height:
                        stack.append((node(key), height - 1))
                    else:
                        ids.add(key)
        return ids, need

    @staticmethod
    def meta(path: Path, access: str) -> dict | None:
        """What a read of a fixture needs, from the fixture's own b-tree and R-tree pages.

        The schema's pages, then: a bbox read through an R-tree, the nodes it visits and the leaf
        pages (and the interior pages above them) of the rows it gives; any other read, the whole
        table, as the fixtures have no attribute index and a row's columns share its page.

        Args:
            path: The fixture.
            access: The access pattern.

        Returns:
            `{"need_bytes": ...}`: the bytes the access needs, from the schema, the R-tree and the
            leaf pages.
        """
        import bisect
        import sqlite3

        # Read only, never writing the fixture; the path as a URI, so a `#` or `?` in it stays a path.
        con = sqlite3.connect(f"{Path(path).resolve().as_uri()}?immutable=1", uri=True)
        try:
            page_size = con.execute("PRAGMA page_size").fetchone()[0]
            roots = dict(con.execute("SELECT name, rootpage FROM sqlite_master WHERE type = 'table'"))
            gcol = con.execute("SELECT column_name FROM gpkg_geometry_columns WHERE table_name = ?", (GeoPackage.LAYER,)).fetchone()[0]
            with open(path, "rb") as f:
                leaves = GeoPackage.leaves(f, page_size, roots[GeoPackage.LAYER])
                # The schema a read looks up: the GeoPackage tables present, every page of each.
                schema = sum(
                    len({p for _, leaf, up in GeoPackage.leaves(f, page_size, roots[t]) for p in (leaf, *up)})
                    for t in ("gpkg_contents", "gpkg_geometry_columns", "gpkg_spatial_ref_sys", "gpkg_extensions", "gpkg_ogr_contents")
                    if t in roots
                )
            boxes = GeoParquet.bboxes({"bbox": 1, "bbox16": 16, "threads4": 16}.get(access, 0))
            rtree = f"rtree_{GeoPackage.LAYER}_{gcol}"
            need = (1 + schema) * page_size  # page 1 (sqlite_master) and the schema tables' pages
            if boxes and f"{rtree}_node" in roots:
                ids, nodes = GeoPackage.rtree(con, rtree, boxes)
                keys = [hi for hi, _, _ in leaves]
                hit = {bisect.bisect_left(keys, i) for i in ids}
                pages = {leaves[k][1] for k in hit} | {p for k in hit for p in leaves[k][2]}
                need += nodes + page_size * len(pages)
            else:
                need += page_size * len({p for _, leaf, up in leaves for p in (leaf, *up)})
        finally:
            con.close()
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
        import pyarrow as pa

        if tool == "terrio":
            import terrio

            def open_(p):
                return p  # each read opens the file: nothing is held between reads

            def read(p, **kw):
                return pa.table(terrio.open_stream(p, **kw))
        else:
            def open_(p):
                return p if p.startswith("/") else "/vsicurl/" + p

            def read(p, **kw):
                return gdal_read_arrow(p, **kw)

        boxes = GeoParquet.bboxes({"bbox": 1, "bbox16": 16, "threads4": 16}.get(access, 0))
        if boxes:
            items = [lambda p, b=b: read(p, bbox=b) for b in boxes]
        elif access == "columns":
            items = [lambda p: read(p, columns=GeoParquet.COLUMNS)]
        elif access == "where" and tool == "terrio":  # terrio's stream has no `where`: terrio's SQL
            items = [lambda p: pa.table(terrio.sql(p, f"SELECT * FROM {GeoPackage.LAYER} WHERE {GeoParquet.WHERE}"))]
        elif access == "where":
            items = [lambda p: read(p, where=GeoParquet.WHERE)]
        else:
            items = [read]
        warm = (open_, read)
        return open_, items, table_checksum, warm


FORMATS = {"geotiff": GeoTiff, "zarr": Zarr, "geoparquet": GeoParquet, "flatgeobuf": FlatGeobuf, "geopackage": GeoPackage}

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
    "geopackage": [
        "crates/terrio/src/drivers/vector/gpkg_read.rs",
        "crates/terrio/src/drivers/vector/mod.rs",  # the eager read `terrio.sql` opens
        "crates/terrio/src/drivers/vector/wkb.rs",
        "crates/terrio/src/drivers/sqlite.rs",
        "crates/terrio/src/drivers/sqlite_repack.rs",
        "crates/terrio/src/stream/*",
        "crates/terrio/src/arrow.rs",
        "crates/terrio/src/ops/sql.rs",
        "crates/terrio/src/ops/sql/*",
        "python/src/stream.rs",
        "python/src/arrow.rs",
        "python/src/ops.rs",  # `terrio.sql`, the `where` read
    ],
}


# =============================================================================================
# GeoTIFF, COG and GeoParquet writes


def _writes_dir(parent: Path) -> Path:
    """This run's write outputs, emptied for each case and removed when the run ends.

    Args:
        parent: The fixture directory.

    Returns:
        The empty directory.
    """
    import atexit
    import shutil

    out = parent / f".writes-{os.getpid()}"
    if not out.exists():
        atexit.register(shutil.rmtree, out, True)
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir()
    return out


def _spawned(fn, *args):
    """Runs a function in a fresh process: a worker inherits the runner's peak memory as its own
    floor, which a reference write would raise.

    Args:
        fn: A module-level function.
        *args: Its arguments.

    Returns:
        What it returns.
    """
    import multiprocessing
    from concurrent.futures import ProcessPoolExecutor

    with ProcessPoolExecutor(1, mp_context=multiprocessing.get_context("spawn")) as pool:
        return pool.submit(fn, *args).result()


class GeoTiffWrite:
    """COG and GeoTIFF writes of the GeoTIFF fixtures, by codec, predictor, tile size, overviews and threads.

    A case writes a lossless fixture, loaded in memory before the timed write, to a new file on
    local disk: the time is the write call's. GDAL writes the reference output once, untimed, when
    the case starts. After each terrio sample, GDAL reads terrio's output and the reference and
    compares them: size, georeferencing, CRS, types, nodata, color interpretation, mask flags,
    overview count and every pixel of every band and overview level, exactly (JPEG: within
    `JPEG_TOL`). Each output is deleted after its sample.
    """

    DIR = "geotiff"  # the sources are the GeoTIFF fixtures
    SOURCES = ["local"]
    EXT = ".tif"
    # name: (source fixture, compress, predictor, block, interleave, photometric)
    FIXTURES = {
        "u8_none_t512": ("u8_none", "NONE", 1, 512, "BAND", None),
        "u8_rgb_deflate_p2_t512": ("u8_rgb_deflate_band", "DEFLATE", 2, 512, "PIXEL", None),
        "u8_rgb_jpeg_rgb_t512": ("u8_rgb_deflate_band", "JPEG", 1, 512, "PIXEL", "RGB"),  # GTiff only
        "u8_rgb_jpeg_ycbcr_t512": ("u8_rgb_deflate_band", "JPEG", 1, 512, "PIXEL", "YCBCR"),
        "u16_deflate_p2_t512": ("u16_deflate_p2", "DEFLATE", 2, 512, "BAND", None),
        "u16_deflate_p2_t256": ("u16_deflate_p2", "DEFLATE", 2, 256, "BAND", None),
        "u16_lzw_p2_t512": ("u16_deflate_p2", "LZW", 2, 512, "BAND", None),
        "u16_zstd_p2_t512": ("u16_deflate_p2", "ZSTD", 2, 512, "BAND", None),
        "u16_12b_zstd_p2_band_t256": ("u16_12b_zstd_band", "ZSTD", 2, 256, "BAND", None),
        "f32_deflate_p3_t512": ("f32_none", "DEFLATE", 3, 512, "BAND", None),
        "f32_lzw_p3_t512": ("f32_none", "LZW", 3, 512, "BAND", None),
        "f32_zstd_p3_t512": ("f32_none", "ZSTD", 3, 512, "BAND", None),
    }
    # A COG with average overviews; without overviews; a tiled GeoTIFF without overviews; the COG
    # with one thread (RAYON_NUM_THREADS and GDAL_NUM_THREADS 1).
    ACCESS = ["cog", "cog_no_overviews", "gtiff", "cog_threads1"]
    COMPARES_OUTPUT = True  # `check` compares the written file with GDAL's: the worker hashes no values
    # The largest difference of a JPEG value from GDAL's within tolerance: the largest measured
    # between terrio's and GDAL's RGB JPEG of `u8_rgb_deflate_band` (1.3% of the values differ,
    # by 0.07 on average; both outputs' mean error against the source is 6.43).
    JPEG_TOL = 24

    @staticmethod
    def axes(name: str) -> dict:
        """The values of the matrix's axes for a case.

        Args:
            name: The case's name.

        Returns:
            Its `codec`, `dtype` and `layout`.
        """
        src, comp, pred, block, interleave, photometric = GeoTiffWrite.FIXTURES[name]
        dtype, bands = GeoTiff.FIXTURES[src][:2]
        return {
            "codec": comp + (f"+p{pred}" if pred > 1 else "") + (f"/{photometric.lower()}" if photometric else ""),
            "dtype": dtype,
            "layout": f"tile {block}/{interleave.lower()}/{bands}b",
        }

    @staticmethod
    def make(out: Path) -> None:
        """Writes the GeoTIFF fixtures the writes start from.

        Args:
            out: The directory of the GeoTIFF fixtures.
        """
        GeoTiff.make(out)

    @staticmethod
    def options(name: str, access: str) -> tuple[str, dict]:
        """The driver and creation options of a case, the same for terrio and GDAL.

        Args:
            name: The case's name.
            access: The access pattern: the kind of output.

        Returns:
            `(driver, options)`.
        """
        _, comp, pred, block, interleave, photometric = GeoTiffWrite.FIXTURES[name]
        o = {"COMPRESS": comp, "INTERLEAVE": interleave}
        if pred > 1:
            o["PREDICTOR"] = str(pred)
        if photometric:
            o["PHOTOMETRIC"] = photometric
        if comp == "ZSTD":
            # Level 1: this build of terrio writes any level above 4 at level 4, which is not GDAL's
            # default 9; at level 1 both write files of the same size.
            o["ZSTD_LEVEL"] = "1"
        if access == "gtiff":
            return "GTiff", {**o, "TILED": "YES", "BLOCKXSIZE": str(block), "BLOCKYSIZE": str(block)}
        overviews = "NONE" if access == "cog_no_overviews" else "IGNORE_EXISTING"
        return "COG", {**o, "BLOCKSIZE": str(block), "OVERVIEW_RESAMPLING": "AVERAGE", "OVERVIEWS": overviews}

    @staticmethod
    def gdal_write(ds, dst: Path, driver: str, options: dict) -> None:
        """Writes a dataset with GDAL, with `GDAL_NUM_THREADS` threads.

        Args:
            ds: The GDAL dataset.
            dst: The output path.
            driver: `COG` or `GTiff`.
            options: The creation options.
        """
        from osgeo import gdal

        # GDAL's COG driver has no PHOTOMETRIC option: it writes 3-band JPEG as YCbCr itself. Its
        # ZSTD level is LEVEL.
        cog = {"PHOTOMETRIC": None, "ZSTD_LEVEL": "LEVEL"} if driver == "COG" else {}
        co = [f"{cog.get(k, k)}={v}" for k, v in options.items() if cog.get(k, k)]
        gdal.Translate(str(dst), ds, format=driver, creationOptions=[*co, f"NUM_THREADS={os.environ.get('GDAL_NUM_THREADS', '1')}"])

    @staticmethod
    def reference(src: str, ref: str, driver: str, options: dict) -> tuple[int, int]:
        """Writes GDAL's reference output of a case.

        Args:
            src: The source fixture.
            ref: The output path.
            driver: `COG` or `GTiff`.
            options: The creation options.

        Returns:
            `(n, raw)`: the output's values, every overview level included, and the source's
            uncompressed size in bytes.
        """
        from osgeo import gdal

        gdal.UseExceptions()
        source = gdal.Open(src)
        GeoTiffWrite.gdal_write(source, Path(ref), driver, options)
        r = gdal.Open(ref)
        n = sum(
            o.XSize * o.YSize
            for b in range(1, r.RasterCount + 1)
            for o in [r.GetRasterBand(b), *(r.GetRasterBand(b).GetOverview(i) for i in range(r.GetRasterBand(b).GetOverviewCount()))]
        )
        b1 = source.GetRasterBand(1)
        return n, source.RasterXSize * source.RasterYSize * source.RasterCount * gdal.GetDataTypeSize(b1.DataType) // 8

    @staticmethod
    def meta(path: Path, access: str) -> dict | None:
        """Writes GDAL's reference output of a case, untimed and in a fresh process, and what the samples need.

        Args:
            path: The case's path in the fixture directory (not a file).
            access: The access pattern.

        Returns:
            The source, driver, options, output directory and reference output; `need_bytes`, the
            source's uncompressed size; or `None` when the case does not apply (RGB JPEG is
            GeoTIFF only: GDAL's COG driver writes YCbCr JPEG).
        """
        src, comp, *_, photometric = GeoTiffWrite.FIXTURES[path.stem]
        if photometric == "RGB" and access != "gtiff":
            return None
        out = _writes_dir(path.parent)
        driver, options = GeoTiffWrite.options(path.stem, access)
        ref = out / "gdal-reference.tif"
        n, raw = _spawned(GeoTiffWrite.reference, str(path.with_name(src + ".tif")), str(ref), driver, options)
        return {
            "src": str(path.with_name(src + ".tif")), "out": str(out), "ref": str(ref), "driver": driver, "options": options,
            "n": n, "need_bytes": raw, "gdal_out_bytes": ref.stat().st_size, "tol": GeoTiffWrite.JPEG_TOL if comp == "JPEG" else 0,
        }

    @staticmethod
    def compare(path: Path, ref: Path, tol: float) -> dict:
        """Compares an output with GDAL's, both read by GDAL.

        Args:
            path: terrio's output.
            ref: GDAL's output.
            tol: The largest absolute difference of a pixel within tolerance (0: exact).

        Returns:
            `tol`, `n` (values compared), `differing`, `beyond_tol`, `max_diff`, `sum_diff` (of the
            finite differences) and `mismatch`: what differs other than pixels, or `None`. NaNs
            equal NaNs. Pixels are compared unless the grids or overview sizes differ.
        """
        import numpy as np
        from benchmark_validation import same_crs, same_number
        from osgeo import gdal

        gdal.UseExceptions()
        a, b = gdal.Open(str(path)), gdal.Open(str(ref))
        c = {"tol": tol, "n": 0, "differing": 0, "beyond_tol": 0, "max_diff": 0.0, "sum_diff": 0.0, "mismatch": None}

        def levels(band):
            return [band, *(band.GetOverview(i) for i in range(band.GetOverviewCount()))]

        def grid() -> str | None:
            if (a.RasterXSize, a.RasterYSize, a.RasterCount) != (b.RasterXSize, b.RasterYSize, b.RasterCount):
                return f"size {a.RasterXSize}x{a.RasterYSize}x{a.RasterCount} vs GDAL's {b.RasterXSize}x{b.RasterYSize}x{b.RasterCount}"
            for i in range(1, a.RasterCount + 1):
                x, y = ([(o.XSize, o.YSize) for o in levels(ds.GetRasterBand(i))] for ds in (a, b))
                if x != y:
                    return f"band {i} overview sizes {x} vs GDAL's {y}"
            return None

        def metadata() -> str | None:
            if not np.allclose(a.GetGeoTransform(), b.GetGeoTransform(), rtol=1e-12, atol=1e-8):
                return f"geotransform {a.GetGeoTransform()} vs GDAL's {b.GetGeoTransform()}"
            if not same_crs(a.GetSpatialRef(), b.GetSpatialRef()):
                return "CRS differs"
            for i in range(1, a.RasterCount + 1):
                x, y = a.GetRasterBand(i), b.GetRasterBand(i)
                for what, f in (
                    ("type", lambda z: gdal.GetDataTypeName(z.DataType)),
                    ("color interpretation", lambda z: gdal.GetColorInterpretationName(z.GetColorInterpretation())),
                    ("mask flags", lambda z: z.GetMaskFlags()),
                ):
                    if f(x) != f(y):
                        return f"band {i} {what} {f(x)} vs GDAL's {f(y)}"
                if not same_number(x.GetNoDataValue(), y.GetNoDataValue()):
                    return f"band {i} nodata {x.GetNoDataValue()} vs GDAL's {y.GetNoDataValue()}"
            return None

        c["mismatch"] = grid()
        if c["mismatch"]:
            return c  # no pixel to compare
        c["mismatch"] = metadata()  # the pixels are compared all the same
        for i in range(1, a.RasterCount + 1):
            for x, y in zip(levels(a.GetRasterBand(i)), levels(b.GetRasterBand(i)), strict=True):
                xa, ya = x.ReadAsArray().astype("float64"), y.ReadAsArray().astype("float64")
                d = np.abs(xa - ya)
                d[(xa == ya) | (np.isnan(xa) & np.isnan(ya))] = 0  # equal values, infinities included
                d[np.isnan(d)] = np.inf  # NaN against a number
                c["n"] += d.size
                c["differing"] += int(np.count_nonzero(d))
                c["beyond_tol"] += int(np.count_nonzero(d > tol))
                c["max_diff"] = max(c["max_diff"], float(d.max()))
                c["sum_diff"] += float(d[np.isfinite(d)].sum())
        return c

    @staticmethod
    def reader(tool: str, access: str, meta: dict):
        """How a tool writes a case, in the shape of a read: the write is the timed item.

        The warm-up loads the source in memory, untimed, and writes a 256 x 256 crop of it with the
        case's options; `open` returns the loaded source.

        Args:
            tool: `terrio` or `gdal`.
            access: The access pattern.
            meta: What `meta` returned for the case.

        Returns:
            `(open, items, check, warm)`, as a format's `reader`. `check` returns `(values beyond
            tolerance, n, comparison)` for terrio's output, with its size and GDAL's, and `(0, n)`
            for GDAL's; it deletes the output.
        """
        if access == "cog_threads1":  # before either library starts its threads
            os.environ["RAYON_NUM_THREADS"] = os.environ["GDAL_NUM_THREADS"] = "1"
        out, driver, options = Path(meta["out"]), meta["driver"], meta["options"]
        if tool == "terrio":
            import terrio

            def load(p):
                return terrio.open_raster(p)

            def crop(r):
                return terrio.translate(r, srcwin=(0, 0, 256, 256))

            def write(r, dst):
                terrio.write_raster(r, str(dst), driver=driver, options=options)
        else:
            from osgeo import gdal

            gdal.UseExceptions()

            def load(p):
                return gdal.Translate("", p, format="MEM")

            def crop(ds):
                return gdal.Translate("", ds, format="MEM", srcWin=[0, 0, 256, 256])

            def write(ds, dst):
                GeoTiffWrite.gdal_write(ds, dst, driver, options)

        held = {}

        def warm_open(_):
            held["src"] = load(meta["src"])
            return crop(held["src"])

        def warm_write(small):
            dst = out / f"warm-{os.getpid()}.tif"
            write(small, dst)
            dst.unlink()

        def item(src):
            dst = out / f"{tool}-{os.getpid()}.tif"
            write(src, dst)
            return dst

        def check(dst):
            try:
                if tool == "gdal":
                    return 0, meta["n"]
                c = GeoTiffWrite.compare(dst, Path(meta["ref"]), meta["tol"])
                c.update(out_bytes=dst.stat().st_size, gdal_out_bytes=meta["gdal_out_bytes"])
                return c["beyond_tol"] + bool(c["mismatch"]), meta["n"], c
            finally:
                dst.unlink()

        return lambda _: held["src"], [item], check, (warm_open, warm_write)


FORMATS["geotiff-write"] = GeoTiffWrite
PATHS["geotiff-write"] = [
    "crates/terrio/src/drivers/gtiff_write.rs",
    "crates/terrio/src/drivers/tiff_lzw.rs",
    "crates/terrio/src/cog.rs",
    "crates/terrio/src/tiff_*",
    "crates/terrio/src/deflate_compress.rs",
    "crates/terrio/src/jpeg.rs",
    "crates/terrio/src/drivers/gdal_overview.rs",
    "python/src/ops.rs",
    "python/src/raster.rs",
]


class GeoParquetWrite:
    """GeoParquet writes of the GeoParquet fixtures, one per geometry type.

    terrio's GeoParquet writer takes no options: it writes WKB with a covering bbox column,
    Snappy compression and row groups of 65536 rows, in the features' order. GDAL is given the
    same (`gdal.VectorTranslate` with `layerCreationOptions`). The time is the write call's, from
    the source loaded in memory beforehand (terrio's `open_vector`, a GDAL `MEM` copy). After each terrio sample, the
    output is compared with GDAL's reference: every feature's attributes and normalized geometry,
    field names and types and the CRS (`benchmark_validation.compare_vectors`), then the row and
    row group counts, columns, compression, geometry encoding and covering. An output whose bytes
    were compared already is not compared again.
    """

    DIR = "geoparquet"  # the sources are the GeoParquet fixtures
    SOURCES = ["local"]
    EXT = ".parquet"
    FIXTURES = {  # name: source fixture
        "polygon_wkb_snappy_rg64k_bbox": "poly_wkb_rg64k_zstd_bbox_sorted",
        "point_wkb_snappy_rg64k_bbox": "point_wkb_rg64k_zstd_bbox_sorted",
        "line_wkb_snappy_rg64k_bbox": "line_wkb_rg64k_zstd_bbox_sorted",
    }
    ACCESS = ["write"]
    COMPARES_OUTPUT = True  # `check` compares the written file with GDAL's: the worker hashes no values
    LCO = ["GEOMETRY_ENCODING=WKB", "WRITE_COVERING_BBOX=YES", "COMPRESSION=SNAPPY", "ROW_GROUP_SIZE=65536", "SORT_BY_BBOX=NO"]

    @staticmethod
    def axes(name: str) -> dict:
        """The values of the matrix's axes for a case.

        Args:
            name: The case's name.

        Returns:
            Its `codec`, `dtype` and `layout`.
        """
        return {"codec": "WKB/SNAPPY", "dtype": GeoParquet.FIXTURES[GeoParquetWrite.FIXTURES[name]][0], "layout": "rg 65536/bbox/source order"}

    @staticmethod
    def make(out: Path) -> None:
        """Writes the GeoParquet fixtures the writes start from.

        Args:
            out: The directory of the GeoParquet fixtures.
        """
        GeoParquet.make(out)

    @staticmethod
    def gdal_write(ds, dst: Path) -> None:
        """Writes a vector dataset as GeoParquet with GDAL, as terrio writes it.

        Args:
            ds: The GDAL dataset, or its path.
            dst: The output path.
        """
        from osgeo import gdal

        gdal.UseExceptions()
        gdal.VectorTranslate(str(dst), ds, format="Parquet", layerCreationOptions=GeoParquetWrite.LCO)

    @staticmethod
    def meta(path: Path, access: str) -> dict:
        """Writes GDAL's reference output of a case, untimed and in a fresh process, and what the samples need.

        Args:
            path: The case's path in the fixture directory (not a file).
            access: The access pattern.

        Returns:
            The source, output directory and reference; `n`, the rows; `need_bytes`, the
            source's uncompressed size.
        """
        import pyarrow.parquet as pq

        src = path.with_name(GeoParquetWrite.FIXTURES[path.stem] + ".parquet")
        out = _writes_dir(path.parent)
        ref = out / "gdal-reference.parquet"
        _spawned(GeoParquetWrite.gdal_write, str(src), ref)
        md = pq.ParquetFile(src).metadata
        return {
            "src": str(src), "out": str(out), "ref": str(ref), "n": md.num_rows, "gdal_out_bytes": ref.stat().st_size,
            "need_bytes": sum(md.row_group(g).total_byte_size for g in range(md.num_row_groups)),
        }

    @staticmethod
    def compare(path: Path, ref: Path) -> dict:
        """Compares an output with GDAL's.

        Args:
            path: terrio's output.
            ref: GDAL's output.

        Returns:
            The comparison, as `GeoTiffWrite.compare`'s: `n` features, and `mismatch`, the first
            difference, or `None`.
        """
        import pyarrow.parquet as pq
        from benchmark_validation import compare_vectors
        from osgeo import ogr

        a, b = pq.ParquetFile(path).metadata, pq.ParquetFile(ref).metadata
        mismatch = next(iter(compare_vectors(ogr.Open(str(path)), ogr.Open(str(ref)))), None)
        if mismatch and mismatch.startswith("vector validation "):  # the runner's GEOS or sort, not the output
            raise RuntimeError(mismatch)

        def geo(md):
            # A file without GeoParquet metadata differs from GDAL's rather than failing the check.
            return json.loads((md.metadata or {}).get(b"geo", b"{}")).get("columns", {}).get("geometry", {})

        for what, f in (
            ("rows", lambda md: md.num_rows),
            ("row groups", lambda md: md.num_row_groups),
            ("columns", lambda md: [md.schema.column(i).path for i in range(md.num_columns)]),
            ("compression", lambda md: sorted({md.row_group(0).column(i).compression for i in range(md.num_columns)})),
            ("geometry encoding", lambda md: geo(md).get("encoding")),
            ("covering", lambda md: geo(md).get("covering")),
        ):
            if mismatch is None and f(a) != f(b):
                mismatch = f"{what} {f(a)} vs GDAL's {f(b)}"
        return {"tol": 0, "n": a.num_rows, "differing": 0, "beyond_tol": 0, "max_diff": 0.0, "mismatch": mismatch}

    @staticmethod
    def reader(tool: str, access: str, meta: dict):
        """How a tool writes a case, in the shape of a read, as `GeoTiffWrite.reader`.

        Args:
            tool: `terrio` or `gdal`.
            access: The access pattern.
            meta: What `meta` returned for the case.

        Returns:
            `(open, items, check, warm)`; the warm-up writes the format's warm-up file.
        """
        import hashlib

        out = Path(meta["out"])
        if tool == "terrio":
            import terrio

            def load(p):
                return terrio.open_vector(p)

            def write(v, dst):
                terrio.write_vector(v, str(dst), driver="Parquet")
        else:
            from osgeo import gdal

            gdal.UseExceptions()

            def load(p):
                return gdal.VectorTranslate("", p, format="MEM")

            write = GeoParquetWrite.gdal_write

        held = {}

        def warm_open(p):
            held["src"] = load(meta["src"])
            return load(p)

        def warm_write(small):
            dst = out / f"warm-{os.getpid()}.parquet"
            write(small, dst)
            dst.unlink()

        def item(src):
            dst = out / f"{tool}-{os.getpid()}.parquet"
            write(src, dst)
            return dst

        def check(dst):
            try:
                if tool == "gdal":
                    return 0, meta["n"]
                # the same bytes compare the same: once per case
                done = out / f"compared-{hashlib.sha256(dst.read_bytes()).hexdigest()}.json"
                if done.exists():
                    c = json.loads(done.read_text())
                else:
                    c = GeoParquetWrite.compare(dst, Path(meta["ref"]))
                    done.write_text(json.dumps(c))
                c.update(out_bytes=dst.stat().st_size, gdal_out_bytes=meta["gdal_out_bytes"])
                return bool(c["mismatch"]), meta["n"], c
            finally:
                dst.unlink()

        return lambda _: held["src"], [item], check, (warm_open, warm_write)


FORMATS["geoparquet-write"] = GeoParquetWrite
PATHS["geoparquet-write"] = [
    "crates/terrio/src/drivers/vector/geoparquet.rs",
    "crates/terrio/src/drivers/vector/wkb.rs",
    "crates/terrio/src/drivers/vector/projjson.rs",
    "crates/terrio/src/parquet_metadata.rs",
    "python/src/ops.rs",
    "python/src/vector.rs",
]


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


FORMATS["warp"], FORMATS["terrain"] = Warp, Terrain
PATHS["warp"] = [
    "crates/terrio/src/ops/warp*",
    "crates/terrio/src/ops/mod.rs",
    "crates/terrio/src/crs.rs",
    "crates/terrio-crs/src/*",
    "python/src/ops.rs",
]
PATHS["terrain"] = [
    "crates/terrio/src/ops/dem.rs",
    "crates/terrio/src/ops/window.rs",
    "crates/terrio/src/ops/mod.rs",
    "crates/terrio/src/ops/proximity.rs",
    "crates/terrio/src/ops/calc/*",
    "python/src/terrain.rs",
]


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
                # In 1 MiB pieces: a worker inherits the runner's peak RSS (`ru_maxrss` survives
                # exec), so whole-file answers held in memory here would inflate every worker's.
                with open(path, "rb") as f:
                    f.seek(start)
                    while sent < end - start + 1 and (piece := f.read(min(1 << 20, end - start + 1 - sent))):
                        self.wfile.write(piece)
                        sent += len(piece)
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


# A format's `threads4` access is measured with each of these numbers of reading threads, as
# `threads1` to `threads8`: each thread opens its own handle and reads every Nth of the 16 parts.
THREAD_COUNTS = (1, 2, 4, 8)


def accesses(fmt) -> list[str]:
    """A format's access patterns, with its `threads4` expanded to one per `THREAD_COUNTS`.

    Args:
        fmt: The format's class.

    Returns:
        The names of the access patterns `run` measures by default and accepts.
    """
    return [a for x in fmt.ACCESS for a in ([f"threads{n}" for n in THREAD_COUNTS] if x == "threads4" else [x])]


def threads(access: str) -> int:
    """The number of reading threads of an access pattern.

    Args:
        access: The access pattern, such as `threads8`.

    Returns:
        N for `threadsN`, 0 for a read in the calling thread.
    """
    return int(access.removeprefix("threads")) if access.startswith("threads") else 0


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
    return _proc_io()["rchar"]


def _proc_io() -> dict[str, int]:
    """This process's I/O counters (Linux).

    Returns:
        The counters by name: `rchar` counts the bytes the process read, `read_bytes` those it
        read from a block device.
    """
    return {k: int(v) for k, v in (line.split(": ") for line in PROC_IO.read_text().splitlines())}


def _files(path: Path) -> list[Path]:
    """A fixture's files.

    Args:
        path: The fixture: a file or a directory store such as Zarr's.

    Returns:
        The fixture itself, or every file under the directory.
    """
    return [path] if path.is_file() else sorted(f for f in path.rglob("*") if f.is_file())


def _resident_pages(fd: int, size: int) -> int:
    """How many pages of an open file are in the OS page cache (Linux's `mincore`).

    Args:
        fd: The file's descriptor.
        size: The file's size in bytes, more than 0.

    Returns:
        The number of its pages in the page cache.
    """
    import ctypes

    libc = ctypes.CDLL(None, use_errno=True)
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
    # Mapping a file reads none of it; mincore then tells which of its pages are cached.
    addr = libc.mmap(None, size, 1, 1, fd, 0)  # PROT_READ, MAP_SHARED
    if addr in (None, ctypes.c_void_p(-1).value):
        raise OSError(ctypes.get_errno(), "mmap failed")
    try:
        vec = (ctypes.c_ubyte * -(-size // resource.getpagesize()))()
        if libc.mincore(ctypes.c_void_p(addr), size, vec):
            raise OSError(ctypes.get_errno(), "mincore failed")
        return sum(v & 1 for v in vec)
    finally:
        libc.munmap(ctypes.c_void_p(addr), size)


def evict(path: Path) -> dict:
    """Drops a fixture's files from the OS page cache, without root, and counts what stayed.

    Each file is flushed first, as `POSIX_FADV_DONTNEED` leaves dirty pages, such as those of a
    file just written. It also leaves the pages another process maps.

    Args:
        path: The fixture: a file or a directory store.

    Returns:
        `cache_pages`, the files' pages, and `resident_before` and `resident_after`, those of them
        in the page cache before and after the eviction.
    """
    pages = before = after = 0
    for f in _files(path):
        fd = os.open(f, os.O_RDONLY)
        try:
            size = os.fstat(fd).st_size
            if not size:
                continue
            os.fsync(fd)
            pages += -(-size // resource.getpagesize())
            before += _resident_pages(fd, size)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            after += _resident_pages(fd, size)
        finally:
            os.close(fd)
    return {"cache_pages": pages, "resident_before": before, "resident_after": after}


def rewarm(path: Path) -> None:
    """Reads a fixture's files through, into the page cache: a warm read in a run with cold ones.

    Args:
        path: The fixture: a file or a directory store.
    """
    for f in _files(path):
        with open(f, "rb") as h:
            while h.read(1 << 24):
                pass


def worker(spec: dict) -> dict:
    """Reads one case in this process, timed: one sample of one tool.

    Args:
        spec: The format, tool, source, access pattern and fixture metadata of the case.

    Returns:
        The sample: init, open and read times, bytes, peak memory, and the values' sum and count;
        for a cold read, also the fixture's pages in the page cache before and after the
        eviction and the bytes read from a block device.
    """
    from concurrent.futures import ThreadPoolExecutor

    fmt = FORMATS[spec["format"]]
    src, access = spec["src"], spec["access"]
    local = src.startswith("/")
    # `base` is another terrio build, found first on PYTHONPATH
    tool = "terrio" if spec["tool"] == "base" else spec["tool"]
    n_threads = threads(access)
    ti = time.perf_counter()
    # A format reads its 16 parts for `threads4`; any thread count reads the same parts.
    open_, items, check, (warm_open, warm_read) = fmt.reader(tool, "threads4" if n_threads else access, spec["meta"])
    # A process's first open and read pays one-time costs (library initialization, thread pools):
    # measured on a tiny file apart from the case, as `init`.
    warm_read(warm_open(src.rsplit("/", 1)[0] + "/_warmup" + fmt.EXT))
    # A cold read: the fixture leaves the page cache after the warm-up, so the read starts from storage.
    te = time.perf_counter()  # the eviction is neither `init` nor the read
    cold = {**evict(Path(src)), "disk_bytes": -_proc_io()["read_bytes"]} if spec.get("cold") else {}
    r0 = _rchar(local)
    t0 = time.perf_counter()
    if n_threads:  # a handle per thread, opened in the threads and timed as the read

        # Each job waits for the others to start, so N jobs run on N threads: an idle pool thread
        # would otherwise take a second job.
        started = threading.Barrier(n_threads)

        def job(k):
            started.wait()
            d = open_(src)  # a handle per thread: GDAL datasets are not thread-safe
            return [(i, items[i](d)) for i in range(k, len(items), n_threads)]

        t1, r1 = t0, r0
        with ThreadPoolExecutor(n_threads) as pool:  # a job's exception is raised here
            out = [res for _, res in sorted((x for part in pool.map(job, range(n_threads)) for x in part), key=lambda x: x[0])]
    else:
        ds = open_(src)
        t1, r1 = time.perf_counter(), _rchar(local)
        out = [item(ds) for item in items]
    t2 = time.perf_counter()
    r2 = _rchar(local)
    if cold:  # the bytes read from storage, open included
        cold["disk_bytes"] += _proc_io()["read_bytes"]
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / (1024**2 if sys.platform == "darwin" else 1024)
    sums = [check(x) for x in out]  # after the timed read and its peak memory, as the digest
    compares = [s[2] for s in sums if len(s) > 2]  # (sum, n, compare): an output compared with GDAL's
    return {
        **({"compare": max(compares, key=_badness)} if compares else {}),
        **cold,
        "init_s": te - ti,
        "open_s": t1 - t0,
        "read_s": t2 - t1,
        "open_bytes": r1 - r0,
        "read_bytes": r2 - r1,
        "rss_mb": rss,
        "sum": sum(s[0] for s in sums),
        "n": sum(s[1] for s in sums),
        # A write's or an operation's output is compared by `check`: only read values are hashed.
        "digest": None if getattr(fmt, "COMPARES_OUTPUT", False) else digest(out),
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
        server = ThreadingHTTPServer(("127.0.0.1", 0), lambda *a, **k: _Handler(*a, directory=str(data), **k), bind_and_activate=False)
        cleanup.callback(server.server_close)  # the socket exists from here, even if binding fails
        # Eight reading threads connect at once: with socketserver's backlog of 5, a dropped
        # connection waits a 1 s SYN retransmission, which the case would time as its read.
        server.request_queue_size = 128
        server.server_bind()
        server.server_activate()
        server.daemon_threads = True
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
        sources = args.source or getattr(fmt, "SOURCES", ["local", "http"])
        if {"local", "cold"} & set(sources) and not PROC_IO.exists():
            sys.exit(f"local byte counts need {PROC_IO} (Linux): use --source http")
        if "cold" in sources and not hasattr(os, "posix_fadvise"):
            sys.exit("cold reads need posix_fadvise and mincore (Linux)")
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
        head["machine"]["threads"] = args.threads
        out_file.write(json.dumps(head) + "\n")
        rows = []
        for case_index, (name, access, source) in enumerate(itertools.product(names, args.access or accesses(fmt), sources)):
            path = data / f"{name}{fmt.EXT}"
            label = "where/sql-vs-filter" if args.format in ("geoparquet", "geopackage") and access == "where" else access
            try:
                meta = fmt.meta(path, "threads4" if threads(access) else access)
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
            src = base + path.name if source == "http" else str(path)
            if source != "cold" and "cold" in sources:  # a warm read after a cold one, or first: cache it
                rewarm(path)
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
                        spec = {"format": args.format, "tool": tool, "src": src, "access": access, "meta": meta, "cold": source == "cold"}
                        cmd = [sys.executable, __file__, "_worker", json.dumps(spec)]
                        p = subprocess.run(cmd, env=envs[tool], capture_output=True, text=True, timeout=args.timeout)
                        if p.returncode:
                            err = p.stderr.strip().splitlines()
                            row[f"{tool}_error"] = err[-1][:300] if err else f"exit {p.returncode}"
                            continue
                        r = json.loads(p.stdout.strip().splitlines()[-1])
                        if r.get("resident_after"):  # still cached: not a cold read
                            row[f"{tool}_error"] = f"not evicted: {r['resident_after']} of {r['cache_pages']} pages stayed in the page cache"
                            continue
                        if source == "cold" and not r["disk_bytes"]:  # NFS, FUSE: no block device
                            row[f"{tool}_error"] = "not cold: the read took no bytes from a block device"
                            continue
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
    failed where this build succeeds is listed as such. `values` is `exact` when every sample's
    digest equals GDAL's first, else `checksum_match` when every sample's sum and count do, else the
    first that differs.

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
        out[tool] = {k: statistics.median(r[k] for r in rs) for k in rs[0] if k not in ("sum", "n", "compare", "digest")}
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
        exact = ref.get("digest") is not None and all(r.get("digest") == ref["digest"] for t in tools for r in row[t])
        # Equal digests are exact first: a NaN read sums to NaN, which no sum check accepts.
        out["values"] = "exact" if exact else "checksum_match" if differ is None else f"DIFFER: {differ[0]} n {differ[1]['n']} sum {differ[1]['sum']:.6g} vs gdal n {ref['n']} sum {ref['sum']:.6g}"
        # terrio's outputs label the case; the base build's comparisons stay in its own samples
        compares = [r["compare"] for r in row["terrio"] if "compare" in r]
        if compares:  # outputs compared with GDAL's, pixel by pixel: the worst sample labels the case
            c = out["compare"] = max(compares, key=_badness)
            out["values"] = (
                f"DIFFER: {c['mismatch']}" if c["mismatch"]
                else f"DIFFER: {c['beyond_tol']} of {c['n']} values beyond {c['tol']:g} (max {c['max_diff']:.6g})" if c["beyond_tol"]
                else "within_tol" if c["differing"] else "exact"
            )
        # A base build whose output differs from GDAL's beyond the tolerance is not ranked against.
        if any(r["compare"]["mismatch"] or r["compare"]["beyond_tol"] for r in row.get("base", []) if "compare" in r):
            out.pop("base_ratio", None)
    return out


def _badness(c: dict) -> tuple:
    """How far an output is from GDAL's, to keep a case's worst comparison.

    Args:
        c: A comparison: `mismatch`, `beyond_tol`, `differing` and `max_diff`.

    Returns:
        A key that sorts a worse comparison higher.
    """
    return (bool(c["mismatch"]), c["beyond_tol"], c["differing"], c["max_diff"])


# The `values` of a case whose values agree with GDAL's: `exact` (the same bytes, or an output
# equal to GDAL's), `checksum_match` (the same count and sum within 1e-6; `same` in older
# results), or `within_tol` (an output within its tolerance of GDAL's).
MATCH = ("exact", "checksum_match", "same", "within_tol")


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


def _cold_section(ok: list[dict], run_of: dict[int, int]) -> list[str]:
    """The report's section comparing cold reads with warm local ones, when a run has both.

    Args:
        ok: The ranked cases.
        run_of: The results file of each row, by the row's `id`: a cold case pairs within its run.

    Returns:
        The section's lines, none without a pair.
    """
    by_case: dict[tuple, dict[str, dict]] = {}
    for r in ok:
        if r["source"] in ("local", "cold"):
            by_case.setdefault((r["format"], r["fixture"], r["access"], run_of.get(id(r))), {})[r["source"]] = r
    pairs = [(c["local"], c["cold"]) for c in by_case.values() if len(c) == 2]
    if not pairs:
        return []
    gm = statistics.geometric_mean
    lines = ["\n## Cold page cache (geometric means over the cases read both ways; cases in brackets)\n"]
    for fmt in sorted({warm["format"] for warm, _ in pairs}):
        ps = [(w, c) for w, c in pairs if w["format"] == fmt]
        slow = {t: gm(c[t]["total_s"] / w[t]["total_s"] for w, c in ps) for t in ("terrio", "gdal")}
        lines.append(f"{fmt} cold time / warm time: terrio x{slow['terrio']:.2f}  gdal x{slow['gdal']:.2f}  "
                     f"terrio/gdal time warm x{gm(w['time_ratio'] for w, _ in ps):.2f} cold x{gm(c['time_ratio'] for _, c in ps):.2f}  ({len(ps)})")
    return lines


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
    if "disk_bytes" in t:
        s += f" | disk KiB {int(t['disk_bytes']) >> 10:7} vs {int(g['disk_bytes']) >> 10:7}"
    label = r["values"] if r["values"] in MATCH else "DIFFER"
    if "out_bytes" in r.get("compare", {}):
        s += f" | out KiB {r['compare']['out_bytes'] >> 10:7} vs {r['compare']['gdal_out_bytes'] >> 10:7}"
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
    # The results file of each row (its machine line comes first), so thread counts pair within a run.
    run_of, run = {}, 0
    for r in rows:
        run += "machine" in r
        run_of[id(r)] = run
    rows = [r for r in rows if "machine" not in r]
    lines = []
    for h in heads:
        lines.append(f"machine: {h['cpu']}, {h['cpus']} CPUs (quota {h['cpu_quota']}), {h['mem_gb']} GB, {h['os']}, AC {h['on_ac']}, load {h['load']:.1f}, {h['versions']}")
    if len({(h["cpu"], h["cpus"], h["cpu_quota"], h["mem_gb"]) for h in heads}) > 1:
        lines.append("WARNING: these results come from different machines: compare ratios within one machine only")
    pools = {h.get("threads") for h in heads}
    if len(pools) > 1:
        lines.append(f"WARNING: these results mix --threads {sorted(pools, key=str)}: the thread scaling is left out")
    computed = [r for r in rows if "time_ratio" in r]
    ok = [r for r in computed if r["values"] in MATCH]
    differ = [r for r in computed if r["values"] not in MATCH]
    inexact = [r for r in ok if r["values"] not in ("exact", "within_tol")]
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
    by_case: dict[tuple, dict[int, dict]] = {}
    for r in ok:
        if threads(r["access"]):
            by_case.setdefault((r["format"], r["fixture"], r["source"], run_of.get(id(r))), {})[threads(r["access"])] = r
    scaling = []
    for fmt, source in sorted({(f, s) for f, _, s, _ in by_case}):
        for n in sorted({n for c in by_case.values() for n in c if n > 1}):
            pairs = [(c[1], c[n]) for (f, _, s, _), c in by_case.items() if (f, s) == (fmt, source) and 1 in c and n in c]
            if pairs:
                speedup = {t: math.exp(statistics.fmean(math.log(one[t]["total_s"] / r[t]["total_s"]) for one, r in pairs)) for t in ("terrio", "gdal")}
                ratio = math.exp(statistics.fmean(math.log(r["time_ratio"]) for _, r in pairs))
                scaling.append(f"{fmt} {source:5} threads{n:<2} terrio x{speedup['terrio']:.2f}  gdal x{speedup['gdal']:.2f}  terrio/gdal time x{ratio:.2f}  ({len(pairs)})")
    if scaling and len(pools) <= 1:
        lines.append("\n## Thread scaling (time with 1 reading thread / time with N; geometric means, cases in brackets)\n")
        lines += scaling
    lines += _cold_section(ok, run_of)
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
    within = [r for r in ok if r["values"] == "within_tol"]
    if within:
        lines.append(f"\noutputs within tolerance of GDAL's (ranked): {len(within)}" + "".join(
            f"\n  {r['fixture']}/{r['access']}: {c['differing']} of {c['n']} values differ, max {c['max_diff']:.6g} <= {c['tol']:g}"
            for r in within for c in [r["compare"]]))
    lines.append(f"\nvalues not byte-identical to GDAL's (sum within 1e-6 and count equal; ranked): {len(inexact)}")
    lines += ["  " + fmt_row(r) for r in inexact]
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
    r.add_argument("--source", nargs="*", choices=["local", "http", "cold"],
                   help="local (from the page cache after the first read), http, or cold: the local file dropped from the page cache before each sample (Linux)")
    r.add_argument("--threads", type=int, default=4, help="RAYON_NUM_THREADS and GDAL_NUM_THREADS, for every access")
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
    if a.cmd == "run" and a.access and (bad := set(a.access) - set(accesses(FORMATS[a.format]))):
        p.error(f"--access {' '.join(sorted(bad))}: {a.format} reads are {' '.join(accesses(FORMATS[a.format]))}")
    sources = getattr(FORMATS[a.format], "SOURCES", ["local", "http", "cold"])  # cold on request only
    if a.cmd == "run" and a.source and (bad := set(a.source) - set(sources)):
        p.error(f"--source {' '.join(sorted(bad))}: {a.format} runs from {' '.join(sources)}")
    # absolute: the readers take a relative path as a URL
    a.dir = a.dir.expanduser().resolve() / getattr(FORMATS[a.format], "DIR", a.format)
    a.dir.mkdir(parents=True, exist_ok=True)
    if a.cmd == "fixtures":
        FORMATS[a.format].make(a.dir)
    else:
        run(a)


if __name__ == "__main__":
    main()
