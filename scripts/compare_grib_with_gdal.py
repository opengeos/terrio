"""Compare terrio's GRIB1 and GRIB2 reads with GDAL's (rasterio) on real products.

Usage:
    python scripts/compare_grib_with_gdal.py OUT_DIR URL[#REGEX] [URL[#REGEX] ...]

Each URL is a GRIB file (`file://` URLs too). With a REGEX, the file has a wgrib2 `.idx` index
next to it (NOAA's GFS and HRRR buckets) and the messages whose index line matches REGEX are
fetched by HTTP range requests and concatenated into a local subset file in OUT_DIR; without
one, the whole file is fetched (and decompressed when it ends with `.gz`, as MRMS files do). For
each file, prints the grid and data representation templates of its fields, then compares terrio
with GDAL: band count, size,
geotransform, CRS, and per band the pixels (count of differing pixels, max |d|), nodata, mask,
description and GRIB metadata. Also reads the file through a local HTTP server (with range
requests), as a URL. A file GDAL cannot open must be refused by terrio too. Exits non-zero on any
difference. Needs rasterio and terrio.
"""

from __future__ import annotations

import functools
import gzip
import http.server
import io
import re
import sys
import threading
import urllib.request
from pathlib import Path

import numpy as np
import rasterio
from rasterio.crs import CRS as RioCRS

import terrio.rio as trio

TAGS = (
    "GRIB_COMMENT",
    "GRIB_DISCIPLINE",
    "GRIB_ELEMENT",
    "GRIB_FORECAST_SECONDS",
    "GRIB_IDS",
    "GRIB_PDS_PDTN",
    "GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES",
    "GRIB_PDS_TEMPLATE_NUMBERS",
    "GRIB_REF_TIME",
    "GRIB_SHORT_NAME",
    "GRIB_UNIT",
    "GRIB_VALID_TIME",
)


def fetch(url: str, start: int | None = None, end: int | None = None) -> bytes:
    req = urllib.request.Request(url)
    if start is not None:
        req.add_header("Range", f"bytes={start}-{'' if end is None else end}")
    with urllib.request.urlopen(req, timeout=120) as r:
        return r.read()


def subset(url: str, pattern: str | None, out: Path) -> Path:
    if pattern is None:
        data = fetch(url)
        if url.endswith(".gz"):
            data = gzip.decompress(data)
            out = out.with_suffix("")
        out.write_bytes(data)
        return out
    lines = fetch(url + ".idx").decode().splitlines()
    offsets = [int(line.split(":")[1]) for line in lines]
    data = b""
    for i, line in enumerate(lines):
        if pattern and not re.search(pattern, line):
            continue
        end = offsets[i + 1] - 1 if i + 1 < len(offsets) else None
        data += fetch(url, offsets[i], end)
    out.write_bytes(data)
    return out


def templates(path: Path) -> list[tuple]:
    """`(grid, product, data representation)` template numbers of each GRIB2 field, and
    `("GRIB1", "-", "-")` for each GRIB1 message; a message cut short ends the list."""
    b = path.read_bytes()
    out, pos = [], 0
    while (i := b.find(b"GRIB", pos)) >= 0 and i + 16 <= len(b):
        if b[i + 7] == 1:
            out.append(("GRIB1", "-", "-"))
            pos = i + max(int.from_bytes(b[i + 4 : i + 7], "big"), 8)
            continue
        if b[i + 7] != 2:
            pos = i + 4
            continue
        total = int.from_bytes(b[i + 8 : i + 16], "big")
        p, g, prod = i + 16, None, None
        while p + 5 <= len(b) and b[p : p + 4] != b"7777":
            length, num = int.from_bytes(b[p : p + 4], "big"), b[p + 4]
            if length < 5:
                break
            if num == 3:
                g = int.from_bytes(b[p + 12 : p + 14], "big")
            elif num == 4:
                prod = int.from_bytes(b[p + 7 : p + 9], "big")
            elif num == 5:
                out.append((g, prod, int.from_bytes(b[p + 9 : p + 11], "big")))
            p += length
        pos = i + max(total, 16)
    return out


def compare(path: str, url: str | None = None) -> int:
    try:
        rasterio.open(path).close()
    except rasterio.errors.RasterioIOError as e:
        # GDAL refuses the file: terrio must refuse it too.
        try:
            trio.open(url or path).close()
        except Exception:  # noqa: BLE001 - any refusal will do
            print(f"  both refuse ({str(e)[:100]})")
            return 0
        print(f"  terrio opens a file GDAL refuses ({str(e)[:100]})")
        return 1
    problems = 0
    with rasterio.open(path) as g, trio.open(url or path) as t:
        print(f"  {g.count} bands, {g.width} x {g.height}, terrio driver {t.driver}")
        if (t.count, t.width, t.height) != (g.count, g.width, g.height):
            print(f"  SIZE differs: terrio {(t.count, t.width, t.height)}")
            return 1
        gt, tt = np.array(g.transform.to_gdal()), np.array(t.transform.to_gdal())
        rel = np.max(np.abs(tt - gt) / np.maximum(np.abs(gt), 1e-300))
        print(f"  geotransform GDAL {list(gt)}; max relative difference {rel:.3g}")
        problems += not np.allclose(tt, gt, rtol=1e-9, atol=1e-12)
        same_crs = (g.crs is None and t.crs is None) or (
            g.crs is not None and t.crs is not None and RioCRS.from_wkt(t.crs.to_wkt()) == g.crs
        )
        print(f"  CRS equivalent: {same_crs} (GDAL EPSG {g.crs.to_epsg() if g.crs else None})")
        problems += not same_crs
        for b in g.indexes:
            a, o = g.read(b), t.read(b)
            same = np.array_equal(a, o, equal_nan=True)
            diff = np.abs(a - o)
            mask_same = np.array_equal(g.read_masks(b), t.read_masks(b))
            gtags = {k: v for k, v in g.tags(b).items() if k in TAGS}
            ttags = {k: v for k, v in t.tags(b).items() if k in TAGS}
            tag_diff = {k: (gtags.get(k), ttags.get(k)) for k in set(gtags) | set(ttags) if gtags.get(k) != ttags.get(k)}
            nd_same = g.nodatavals[b - 1] == t.nodatavals[b - 1]
            desc_same = g.descriptions[b - 1] == t.descriptions[b - 1]
            ok = same and mask_same and not tag_diff and nd_same and desc_same
            problems += not ok
            print(
                f"  band {b:3d} {gtags.get('GRIB_ELEMENT', '?'):>8} {gtags.get('GRIB_SHORT_NAME', ''):<22}"
                f" pixels {'identical' if same else f'{np.count_nonzero(diff)} differ, max |d| {np.nanmax(diff):.6g}'}"
                f", nodata {g.nodatavals[b - 1]} {'=' if nd_same else '!= ' + str(t.nodatavals[b - 1])}"
                f", masked {np.count_nonzero(g.read_masks(b) == 0)} {'=' if mask_same else '!='}"
                + ("" if desc_same else f", description {g.descriptions[b - 1]!r} != {t.descriptions[b - 1]!r}")
                + ("" if not tag_diff else f", tags differ {tag_diff}")
            )
    return problems


class RangeHandler(http.server.SimpleHTTPRequestHandler):
    """Serves files with single range requests (`bytes=a-b`), as object stores do."""

    def send_head(self):
        rng = self.headers.get("Range", "")
        m = re.fullmatch(r"bytes=(\d+)-(\d*)", rng.strip())
        path = Path(self.translate_path(self.path))
        if not m or not path.is_file():
            return super().send_head()
        data = path.read_bytes()
        a, b = int(m[1]), min(int(m[2]) if m[2] else len(data) - 1, len(data) - 1)
        body = data[a : b + 1]
        self.send_response(206)
        self.send_header("Content-Range", f"bytes {a}-{b}/{len(data)}")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()
        return io.BytesIO(body)

    def log_message(self, *args):
        pass


def serve(directory: Path) -> tuple[http.server.HTTPServer, int]:
    handler = functools.partial(RangeHandler, directory=str(directory))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def main() -> int:
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    problems = 0
    server, port = serve(out)
    try:
        for k, arg in enumerate(sys.argv[2:]):
            url, _, pattern = arg.partition("#")
            path = subset(url, pattern or None, out / f"subset{k}_{Path(url).name}")
            counts: dict[tuple, int] = {}
            for tpl in templates(path):
                counts[tpl] = counts.get(tpl, 0) + 1
            print(f"{url} ({pattern or 'all'}): {path.stat().st_size} bytes")
            listed = sorted(counts.items(), key=lambda kv: tuple(str(v) for v in kv[0]))
            print("  templates (3.grid, 4.product, 5.packing): " + ", ".join(f"3.{g}/4.{p}/5.{d} x{n}" for (g, p, d), n in listed))
            problems += compare(str(path))
            local_url = f"http://127.0.0.1:{port}/{path.name}"
            print(f"  read as a URL ({local_url}):")
            problems += compare(str(path), local_url)
    finally:
        server.shutdown()
    print(f"{problems} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
