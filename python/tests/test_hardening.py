"""Hardening through Python: zip members read alone, and buffers sized from file headers
refused over their limits or allocated fallibly.

Rust ends the process when an allocation fails, where GDAL and numpy raise. Each allocation test
reads a crafted file in a child process whose address space (``RLIMIT_AS``) leaves 1.5 GiB above
what the interpreter uses, so that the size the file declares cannot be allocated: the child must
raise a catchable exception. A regression aborts the child (SIGABRT), which these tests see as an
exit code, rather than taking the test runner down with it. The Rust unit tests cannot see this:
an abort there ends the test binary.
"""

import http.server
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import threading
import zipfile
import zlib

from pathlib import Path

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.errors import UnsupportedError
from terrio.rio.errors import RasterioIOError

# Inputs the fuzz targets found to crash, kept so that each stays fixed.
REGRESSIONS = Path(__file__).resolve().parents[2] / "fuzz" / "regressions"

# Room above the interpreter's address space: less than every size the crafted files declare.
HEADROOM = 3 << 29

CHILD = r"""
import resource, sys
import terrio, terrio.rio as rio
from terrio.rio.windows import Window

with open("/proc/self/status") as f:
    used = next(int(line.split()[1]) * 1024 for line in f if line.startswith("VmSize:"))
limit = used + int(sys.argv[1])
resource.setrlimit(resource.RLIMIT_AS, (limit, limit))
action, path = sys.argv[2], sys.argv[3]
try:
    if action == "window":
        with rio.open(path) as ds:
            ds.read(1, window=Window(0, 0, 16, 16))
    elif action == "read":
        with rio.open(path) as ds:
            ds.read()
    elif action == "vsi_read":
        terrio.vsi_read(path)
    elif action == "tiles":
        terrio.open_raster(path, lazy=True).write(path + ".pmtiles")
    else:
        terrio.open_raster(path)
    print("OPENED")
except Exception as e:
    print(f"RAISED {type(e).__name__}: {e}")
"""


def read_limited(action, path, env=None, headroom=HEADROOM):
    """Runs `action` on `path` in a child process limited to `headroom` more address space.

    Few malloc arenas and threads: each arena reserves 64 MiB of address space, which would
    otherwise measure the thread count rather than the sizes the files declare.
    """
    env = dict(os.environ if env is None else env, MALLOC_ARENA_MAX="2", RAYON_NUM_THREADS="4", OPENBLAS_NUM_THREADS="1")
    return subprocess.run(
        [sys.executable, "-c", CHILD, str(headroom), action, str(path)],
        capture_output=True,
        text=True,
        timeout=300,
        env=env,
    )


# A size refused before its allocation, or by it: the TIFF decoding limit (TiffReadLimits), a window
# too large for the allocator, or a fallible allocation.
REFUSALS = ("exceeds decoded-byte limit", "too large to allocate", "cannot allocate")


def assert_raises_allocation_error(p):
    assert p.returncode == 0, f"the reader died (exit code {p.returncode}): {p.stderr[-600:]}"
    assert p.stdout.startswith("RAISED") and any(r in p.stdout for r in REFUSALS), p.stdout + p.stderr[-600:]


def tiled_tiff(width, height, tile, tiles=1, tile_bytes=None):
    """A little-endian TIFF of one deflated 8-bit band in `tile` x `tile` tiles: `tiles` entries
    in its tile arrays, all pointing to the same compressed tile."""
    data = tile_bytes if tile_bytes is not None else zlib.compress(b"\0" * 64)
    entries = [
        (256, 4, 1, width),
        (257, 4, 1, height),
        (258, 3, 1, 8),
        (259, 3, 1, 8),
        (262, 3, 1, 1),
        (277, 3, 1, 1),
        (284, 3, 1, 1),
        (322, 4, 1, tile),
        (323, 4, 1, tile),
        (324, 4, tiles, None),
        (325, 4, tiles, None),
    ]
    offsets_at = 8 + 2 + 12 * len(entries) + 4
    counts_at = offsets_at + 4 * tiles
    data_at = counts_at + 4 * tiles if tiles > 1 else offsets_at
    out = bytearray(b"II*\0" + struct.pack("<I", 8) + struct.pack("<H", len(entries)))
    for tag, typ, count, value in entries:
        if tag == 324:
            value = offsets_at if tiles > 1 else data_at
        elif tag == 325:
            value = counts_at if tiles > 1 else len(data)
        packed = struct.pack("<H", value) + b"\0\0" if typ == 3 else struct.pack("<I", value)
        out += struct.pack("<HHI", tag, typ, count) + packed
    out += struct.pack("<I", 0)
    if tiles > 1:
        out += struct.pack(f"<{tiles}I", *[data_at] * tiles)
        out += struct.pack(f"<{tiles}I", *[len(data)] * tiles)
    return bytes(out + data)


def png_declaring(width, height):
    """A 16-bit RGBA PNG whose header declares `width` x `height` pixels, with one row of data."""

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    ihdr = struct.pack(">IIBBBBB", width, height, 16, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(b"\0" * 64)) + chunk(b"IEND", b"")


def test_a_tiff_chunk_too_large_to_allocate_raises(tmp_path):
    """TIFF chunk: one 65,536 x 65,536 tile (4 GiB) for a 16 x 16 window."""
    path = tmp_path / "chunk.tif"
    path.write_bytes(tiled_tiff(65536, 65536, 65536))
    assert_raises_allocation_error(read_limited("window", path))


def test_a_raster_too_large_to_allocate_raises(tmp_path):
    """Raster buffer: a 200,000 x 200,000 image read whole, by window (terrio.rio, the window
    buffer) and eagerly (terrio.open_raster, the raster buffer after the first chunk)."""
    path = tmp_path / "raster.tif"
    tiles = (200_000 // 1024 + 1) ** 2
    path.write_bytes(tiled_tiff(200_000, 200_000, 1024, tiles, zlib.compress(b"\0" * (1 << 20))))
    assert_raises_allocation_error(read_limited("read", path))
    assert_raises_allocation_error(read_limited("eager", path))


def test_a_png_too_large_to_allocate_raises(tmp_path):
    """PNG: a header declaring 50,000 x 50,000 16-bit RGBA pixels (20 GB)."""
    path = tmp_path / "big.png"
    path.write_bytes(png_declaring(50_000, 50_000))
    assert_raises_allocation_error(read_limited("read", path))


def test_a_jpeg_too_large_to_allocate_raises(tmp_path):
    """JPEG: a frame of 65,535 x 32,768 samples (2 GiB, within the decoder's size limit); the
    allocation failure must not be retried with `jpeg-decoder`, which allocates infallibly."""
    path = tmp_path / "big.jpg"
    path.write_bytes(bytes.fromhex("FFD8 FFC0 000B 08 8000 FFFF 01 01 11 00 FFD9"))
    assert_raises_allocation_error(read_limited("read", path))


def test_a_jpeg2000_too_large_to_allocate_raises(tmp_path):
    """JPEG 2000: a codestream whose header declares 60,000 x 60,000 pixels of 3 bands in
    1,024-pixel tiles (10.8 GB), read whole."""
    rasterio = pytest.importorskip("rasterio")
    small = tmp_path / "small.j2k"
    profile = dict(driver="JP2OpenJPEG", width=1024, height=1024, count=3, dtype="uint8", CODEC="J2K", BLOCKXSIZE=1024, BLOCKYSIZE=1024)
    with rasterio.open(small, "w", **profile) as dst:
        dst.write(np.zeros((3, 1024, 1024), "uint8"))
    data = bytearray(small.read_bytes())
    assert data[:4] == b"\xff\x4f\xff\x51", "a J2K codestream starts with SOC and SIZ"
    data[8:16] = struct.pack(">II", 60_000, 60_000)
    path = tmp_path / "big.j2k"
    path.write_bytes(bytes(data))
    assert_raises_allocation_error(read_limited("read", path))


def test_a_zip_member_declaring_more_than_memory_is_streamed(tmp_path):
    """Zip member: a member declaring 4 GiB (the limits raised to allow it) is copied as it is
    decompressed, never allocated whole; its short data is then an error."""
    path = tmp_path / "member.zip"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("big.tif", b"II*\0" + b"\0" * 60)
    data = bytearray(path.read_bytes())
    for signature, at in ((b"PK\x03\x04", 22), (b"PK\x01\x02", 24)):
        i = data.index(signature)
        data[i + at : i + at + 4] = struct.pack("<I", 0xFFFF_FFFE)
    path.write_bytes(bytes(data))
    env = dict(os.environ, TERRIO_ZIP_MAX_RATIO=str(1 << 40), TERRIO_ZIP_MAX_SIZE=str(1 << 40))
    p = read_limited("read", f"/vsizip/{path}/big.tif", env=env)
    assert p.returncode == 0, f"the reader died (exit code {p.returncode}): {p.stderr[-600:]}"
    assert p.stdout.startswith("RAISED"), p.stdout


def test_vsi_read_refuses_a_zip_member_declaring_more_than_memory(tmp_path):
    """vsi_read: a member declaring 4 GiB (the limits raised to allow it) is refused before its
    buffer is allocated, in an address space that cannot hold it, not aborted on."""
    path = tmp_path / "member.zip"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("big.bin", b"\0" * 64)
    data = bytearray(path.read_bytes())
    for signature, at in ((b"PK\x03\x04", 22), (b"PK\x01\x02", 24)):
        i = data.index(signature)
        data[i + at : i + at + 4] = struct.pack("<I", 0xFFFF_FFFE)
    path.write_bytes(bytes(data))
    env = dict(os.environ, TERRIO_ZIP_MAX_RATIO=str(1 << 40), TERRIO_ZIP_MAX_SIZE=str(1 << 40))
    assert_raises_allocation_error(read_limited("vsi_read", f"/vsizip/{path}/big.bin", env=env))


def test_vsi_functions_keep_the_zip_limits_of_a_raster_open(tmp_path, monkeypatch):
    """vsi_read, vsi_copy and vsi_sync of a zip member over TERRIO_ZIP_MAX_SIZE are refused with
    rio.open's message before the member is read; describing it reads no member."""
    path = tmp_path / "z.zip"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("z.bin", bytes(2_000_000))
    member = f"/vsizip/{path}/z.bin"
    out = tmp_path / "out.bin"
    monkeypatch.setenv("TERRIO_ZIP_MAX_SIZE", "1000000")
    message = re.escape("zip: 'z.bin' and its files hold more than the 1000000 bytes allowed (TERRIO_ZIP_MAX_SIZE)")
    with pytest.raises(RasterioIOError, match=message):
        rio.open(member)
    with pytest.raises(UnsupportedError, match=f"^{message}$"):
        terrio.vsi_read(member)
    with pytest.raises(UnsupportedError, match=f"^{message}$"):
        terrio.vsi_copy(member, str(out))
    with pytest.raises(UnsupportedError, match=f"^{message}$"):
        terrio.vsi_sync(member, str(out))
    assert not out.exists()
    assert terrio.vsi_stat(member)["size"] == 2_000_000
    monkeypatch.setenv("TERRIO_ZIP_MAX_SIZE", "2000000")
    assert terrio.vsi_read(member) == bytes(2_000_000)


@pytest.fixture
def zeros_server():
    """A loopback HTTP server honouring Range that serves 2 GiB of zeros, generated as they are sent.

    Returns:
        str: The file's URL.
    """
    size = 2 << 30

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            start, end, status = 0, size - 1, 200
            rng = self.headers.get("Range")
            if rng and rng.startswith("bytes="):
                a, _, b = rng[6:].partition("-")
                start, status = int(a), 206
                end = min(int(b), size - 1) if b else size - 1
            self.send_response(status)
            self.send_header("Content-Length", str(end - start + 1))
            if status == 206:
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.end_headers()
            left, chunk = end - start + 1, bytes(1 << 20)
            try:
                while left:
                    n = min(left, len(chunk))
                    self.wfile.write(chunk[:n])
                    left -= n
            except (BrokenPipeError, ConnectionResetError):
                pass

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{httpd.server_address[1]}/zeros.bin"
    httpd.shutdown()


def test_vsi_read_of_a_file_larger_than_memory_raises(zeros_server):
    """vsi_read: a remote file of 2 GiB, which its first response declares, in an address space
    with 512 MiB left: refused before its buffer is reserved, or its reservation fails; an error,
    not an abort."""
    assert_raises_allocation_error(read_limited("vsi_read", zeros_server, headroom=1 << 29))


@pytest.fixture
def counting_server(tmp_path):
    """A loopback HTTP server with range support, serving `tmp_path`, that counts the requests
    and the body bytes it sends."""
    stats = {"requests": 0, "bytes": 0, "ranges": []}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            data = (tmp_path / self.path.lstrip("/")).read_bytes()
            start, end, status = 0, len(data) - 1, 200
            rng = self.headers.get("Range")
            if rng and rng.startswith("bytes="):
                a, _, b = rng[6:].partition("-")
                start, status = int(a), 206
                end = min(int(b), len(data) - 1) if b else len(data) - 1
            body = data[start : end + 1]
            stats["requests"] += 1
            stats["bytes"] += len(body)
            stats["ranges"].append(rng)
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Accept-Ranges", "bytes")
            if status == 206:
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
            self.end_headers()
            self.wfile.write(body)

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{httpd.server_address[1]}", stats
    httpd.shutdown()


def test_a_remote_zip_member_is_read_by_range(tmp_path, counting_server):
    """Finding 7, remote: a GeoTIFF stored after 4 MiB of another member is read from the
    central directory and its own bytes, not by downloading the archive."""
    url, stats = counting_server
    pixels = np.arange(64 * 64, dtype="uint16").reshape(1, 64, 64)
    tif = tmp_path / "a.tif"
    profile = dict(driver="GTiff", width=64, height=64, count=1, dtype="uint16", crs="EPSG:32631")
    with rio.open(tif, "w", transform=rio.transform.from_origin(0, 64, 1, 1), **profile) as dst:
        dst.write(pixels)
    archive = tmp_path / "a.zip"
    with zipfile.ZipFile(archive, "w") as z:
        z.writestr(zipfile.ZipInfo("pad.bin"), np.random.default_rng(1).bytes(4 << 20))
        z.write(tif, "dir/a.tif", compress_type=zipfile.ZIP_DEFLATED)
    size = archive.stat().st_size
    with rio.open(f"/vsizip//vsicurl/{url}/a.zip/dir/a.tif") as ds:
        np.testing.assert_array_equal(ds.read(), pixels)
    # The first block (where the file's length is learned), the end of the archive with its
    # central directory, and the member: about 3 x 64 KiB of the 4.1 MiB archive.
    assert stats["requests"] <= 4, stats
    assert stats["bytes"] < size // 10, (stats, size)


@pytest.mark.parametrize("path", sorted((REGRESSIONS / "tiff").glob("*.tif")), ids=lambda p: p.name)
def test_fuzz_regressions_raise_exceptions_not_panics(path):
    """Each GeoTIFF the `tiff` fuzz target crashed on opens and reads, or raises an `Exception`.
    A Rust panic surfaces as `PanicException`, a `BaseException` that `except Exception` misses:
    pytest then reports it as an error. `geokey-ascii-multibyte.tif`: a citation GeoKey whose
    offset falls inside a multi-byte character of the GeoAsciiParams."""
    try:
        with rio.open(path) as ds:
            ds.read()
    except RasterioIOError:
        pass
    try:
        terrio.open_raster(str(path))
    except terrio.TerrioError:
        pass


ALLOW_LIST_CHILD = r"""
import sys
import terrio, terrio.rio as rio
terrio.set_allowed_drivers(["GTiff"])
for path in sys.argv[1:]:
    try:
        with rio.open(path) as ds:
            print("OPENED", ds.read().ravel()[:4].tolist())
    except Exception as e:
        print(f"REFUSED {e}")
"""


def test_the_allow_list_sees_inside_zip_members(tmp_path):
    """A3 control: with only GTiff allowed, a VRT named `x.tif` is refused for its content, before
    anything is opened, outside an archive and inside one alike. The VRT points to a local GeoTIFF
    (no network). In a child process: the allow-list is process-wide and cannot be lifted."""
    src = tmp_path / "secret.tif"
    profile = dict(driver="GTiff", width=4, height=4, count=1, dtype="uint8", crs="EPSG:4326")
    with rio.open(src, "w", transform=rio.transform.from_origin(0, 4, 1, 1), **profile) as dst:
        dst.write(np.full((1, 4, 4), 7, "uint8"))
    vrt = (
        '<VRTDataset rasterXSize="4" rasterYSize="4"><VRTRasterBand dataType="Byte" band="1"><SimpleSource>'
        f'<SourceFilename relativeToVRT="0">{src}</SourceFilename><SourceBand>1</SourceBand>'
        "</SimpleSource></VRTRasterBand></VRTDataset>"
    )
    (tmp_path / "x.tif").write_text(vrt)
    with zipfile.ZipFile(tmp_path / "a.zip", "w") as z:
        z.writestr("x.tif", vrt)
    paths = [str(tmp_path / "x.tif"), f"/vsizip/{tmp_path / 'a.zip'}/x.tif"]
    p = subprocess.run([sys.executable, "-c", ALLOW_LIST_CHILD, *paths], capture_output=True, text=True, timeout=120)
    assert p.returncode == 0, p.stderr[-600:]
    outside, inside = p.stdout.splitlines()
    assert "the VRT driver is not allowed" in outside, outside
    assert "the VRT driver is not allowed" in inside, inside


TIFF_TAGS_CHILD = r"""
import sys
import terrio
terrio.set_allowed_drivers(sys.argv[2].split(","), kind="raster")
try:
    print("READ", len(terrio._terrio.tiff_tags(sys.argv[1])))
except Exception as e:
    print(f"REFUSED {type(e).__name__}: {e}")
"""


def test_tiff_tags_reads_a_tiff_only_with_gtiff_allowed(tmp_path):
    """`tiff_tags` reads a TIFF's tags with a reader of the binding's own: under a raster list
    without GTiff it refuses the TIFF before reading them, as terrio's GeoTIFF reader does. In a
    child process: the allow-list is process-wide and cannot be lifted."""
    tif = tmp_path / "x.tif"
    profile = dict(driver="GTiff", width=4, height=4, count=1, dtype="uint8", crs="EPSG:4326")
    with rio.open(tif, "w", transform=rio.transform.from_origin(0, 4, 1, 1), **profile) as dst:
        dst.write(np.full((1, 4, 4), 7, "uint8"))
    assert len(terrio._terrio.tiff_tags(str(tif))) == 1

    def child(drivers):
        p = subprocess.run([sys.executable, "-c", TIFF_TAGS_CHILD, str(tif), drivers], capture_output=True, text=True, timeout=120)
        assert p.returncode == 0, p.stderr[-600:]
        return p.stdout.strip()

    refusal = f"'{tif}' holds GTiff data, and the GTiff driver is not allowed (allowed: PNG)"
    assert child("PNG") == f"REFUSED UnsupportedError: {refusal}"
    assert child("PNG,GTiff") == "READ 1"


def tiff_file(entries, data):
    """A little-endian TIFF of one IFD: `entries` are `(tag, type, values)` (3 SHORT, 4 LONG),
    sorted by tag; `TileOffsets` and `TileByteCounts`, added here, point to `data`."""
    entries = sorted([*entries, (324, 4, [0]), (325, 4, [len(data)])])
    at = 8 + 2 + 12 * len(entries) + 4
    arrays, ifd = bytearray(), bytearray(struct.pack("<H", len(entries)))
    for tag, typ, values in entries:
        packed = struct.pack(f"<{len(values)}{'H' if typ == 3 else 'I'}", *values)
        if len(packed) > 4:
            ifd += struct.pack("<HHII", tag, typ, len(values), at + len(arrays))
            arrays += packed
        else:
            ifd += struct.pack("<HHI", tag, typ, len(values)) + packed.ljust(4, b"\0")
    data_at = at + len(arrays)
    ifd = ifd.replace(struct.pack("<HHII", 324, 4, 1, 0), struct.pack("<HHII", 324, 4, 1, data_at))
    return b"II*\0" + struct.pack("<I", 8) + bytes(ifd) + struct.pack("<I", 0) + bytes(arrays) + data


def webp_declaring(width, height):
    """A lossless WebP (VP8L) whose header declares `width` x `height` pixels without alpha, and
    no image data."""
    vp8l = b"\x2f" + struct.pack("<I", (width - 1) | (height - 1) << 14) + b"\0" * 16
    chunk = b"VP8L" + struct.pack("<I", len(vp8l)) + vp8l
    return b"RIFF" + struct.pack("<I", 4 + len(chunk)) + b"WEBP" + chunk


def test_a_webp_chunk_too_large_to_allocate_raises(tmp_path):
    """TIFF chunk, WebP (the path for RGB with an unspecified extra sample): a 16,368 x 16,368
    tile whose WebP header declares as much (768 MiB of RGB) under 512 MiB of headroom."""
    side = 16368
    entries = [
        (256, 4, [side]), (257, 4, [side]), (258, 3, [8, 8, 8, 8]), (259, 3, [50001]), (262, 3, [2]),
        (277, 3, [4]), (284, 3, [1]), (322, 4, [side]), (323, 4, [side]), (338, 3, [0]),
    ]
    path = tmp_path / "webp.tif"
    path.write_bytes(tiff_file(entries, webp_declaring(side, side)))
    assert_raises_allocation_error(read_limited("window", path, headroom=1 << 29))


ZARR_CHILD = r"""
import json, resource, sys
import terrio.rio as rio
from terrio.rio.windows import Window

with open("/proc/self/status") as f:
    used = next(int(line.split()[1]) * 1024 for line in f if line.startswith("VmSize:"))
limit = used + int(sys.argv[1])
resource.setrlimit(resource.RLIMIT_AS, (limit, limit))
out = {}
with rio.open(sys.argv[2]) as ds:
    out["shape"] = list(ds.shape)
    out["window"] = ds.read(1, window=Window(0, 0, 4, 4)).tolist()
    out["decimated"] = ds.read(out_shape=(1, 8, 8)).tolist()
    for stage in ("dataset_mask", "read"):
        try:
            getattr(ds, stage)()
            out[stage] = "READ"
        except Exception as e:
            out[stage] = f"RAISED {type(e).__name__}: {e}"
print(json.dumps(out))
"""


def zarr_declaring(path, shape, chunks):
    """A Zarr v2 store of UInt16 declaring `shape` in uncompressed `chunks` (fill value 0): its
    first chunk, and the chunk holding the first pixel a decimated read of 8 x 8 picks, of values
    1 to n."""
    path.mkdir()
    zarray = {
        "zarr_format": 2,
        "shape": shape,
        "chunks": chunks,
        "dtype": "<u2",
        "compressor": None,
        "fill_value": 0,
        "order": "C",
        "filters": None,
    }
    (path / ".zarray").write_text(json.dumps(zarray))
    (path / ".zattrs").write_text(json.dumps({"_ARRAY_DIMENSIONS": ["y", "x"]}))
    values = np.arange(1, chunks[0] * chunks[1] + 1, dtype="<u2").tobytes()
    picked = int(0.5 * shape[1] / 8) // chunks[1]
    for key in ("0.0", f"0.{picked}"):
        (path / key).write_bytes(values)


def test_a_zarr_store_declaring_a_huge_shape_is_read_decimated_and_refused_whole(tmp_path):
    """A Zarr store declaring 2^31 - 1 columns (a few hundred bytes of metadata): a 4 x 4 window
    reads; a decimated read (`out_shape`), the read every tile server makes, reads the chunks
    holding the pixels it picks, GDAL's values, rather than allocating the 275 GB window spanning
    them (which ended the process: "memory allocation of 270582939776 bytes failed"); its mask
    and its whole read are errors before anything is allocated. Under an address space of 1.5 GiB
    above the interpreter's."""
    rasterio = pytest.importorskip("rasterio")
    path = tmp_path / "wide.zarr"
    zarr_declaring(path, [64, 2**31 - 1], [32, 32])
    env = dict(os.environ, MALLOC_ARENA_MAX="2", RAYON_NUM_THREADS="4", OPENBLAS_NUM_THREADS="1")
    p = subprocess.run(
        [sys.executable, "-c", ZARR_CHILD, str(HEADROOM), str(path)],
        capture_output=True,
        text=True,
        timeout=300,
        env=env,
    )
    assert p.returncode == 0, f"the reader died (exit code {p.returncode}): {p.stderr[-600:]}"
    got = json.loads(p.stdout)
    with rasterio.open(path) as ds:
        assert got["shape"] == list(ds.shape) == [64, 2**31 - 1]
        assert got["window"] == ds.read(1, window=rasterio.windows.Window(0, 0, 4, 4)).tolist()
        want = ds.read(out_shape=(1, 8, 8))
    assert got["decimated"] == want.tolist() and want.any(), (got["decimated"], want)
    refused = (
        r"RAISED RasterioIOError: Read failed: .*a window of 2147483647x64 pixels in 1 band\(s\) "
        r"is too large: \d+ bytes, more than the \d+ bytes of memory the process can use"
    )
    for stage in ("dataset_mask", "read"):
        assert re.match(refused, got[stage]), got[stage]


ZARR_STAGES = r"""
import json, resource, sys
import terrio.rio as rio
from terrio.rio.windows import Window

with open("/proc/self/status") as f:
    used = next(int(line.split()[1]) * 1024 for line in f if line.startswith("VmSize:"))
limit = used + int(sys.argv[1])
resource.setrlimit(resource.RLIMIT_AS, (limit, limit))
out = {}
with rio.open(sys.argv[2]) as ds:
    for stage, read in (
        ("window", lambda: ds.read(1, window=Window(32000, 5, 1, 1))),
        ("decimated", lambda: ds.read(out_shape=(1, 64, 64))),
    ):
        try:
            a = read()
            out[stage] = [list(a.shape), int(a.min()), int(a.max())]
        except Exception as e:
            out[stage] = f"RAISED {type(e).__name__}: {e}"
print(json.dumps(out))
"""


@pytest.fixture(scope="module")
def zarr_1_gib_chunk(tmp_path_factory):
    """A Zarr v2 store of one 32,768 x 32,768 uint8 chunk of zeros (1 GiB, the most GDAL's rule
    allows), zlib-coded in about 1 MB, its fill value 7."""
    path = tmp_path_factory.mktemp("zarr") / "one.zarr"
    path.mkdir()
    zarray = {
        "zarr_format": 2,
        "shape": [32768, 32768],
        "chunks": [32768, 32768],
        "dtype": "|u1",
        "compressor": {"id": "zlib", "level": 1},
        "fill_value": 7,
        "order": "C",
        "filters": None,
    }
    (path / ".zarray").write_text(json.dumps(zarray))
    z = zlib.compressobj(1)
    block = bytes(1 << 26)
    (path / "0.0").write_bytes(b"".join([z.compress(block) for _ in range(16)] + [z.flush()]))
    return path


@pytest.mark.parametrize(
    "headroom, want",
    [(HEADROOM, "values"), (1 << 29, "refused")],
    ids=["1.5 GiB", "512 MiB"],
)
def test_a_zarr_chunk_of_1_gib_is_read_or_refused_never_aborted(zarr_1_gib_chunk, headroom, want):
    """One honest 1 GiB chunk (1 MB stored): with 1.5 GiB of address space above the interpreter's,
    a 1 x 1 window and the decimated read return the chunk's values, the chunk held once, where
    terrio held it twice ("memory allocation of 1073741824 bytes failed", the process aborted);
    with 512 MiB, both reads are catchable errors, the chunk's buffer allocated fallibly."""
    env = dict(os.environ, MALLOC_ARENA_MAX="2", RAYON_NUM_THREADS="4", OPENBLAS_NUM_THREADS="1")
    p = subprocess.run(
        [sys.executable, "-c", ZARR_STAGES, str(headroom), str(zarr_1_gib_chunk)],
        capture_output=True,
        text=True,
        timeout=300,
        env=env,
    )
    assert p.returncode == 0, f"the reader died (exit code {p.returncode}): {p.stderr[-600:]}"
    got = json.loads(p.stdout)
    if want == "values":
        assert got == {"window": [[1, 1], 0, 0], "decimated": [[1, 64, 64], 0, 0]}, got
    else:
        for stage in ("window", "decimated"):
            refused = got[stage]
            assert refused.startswith("RAISED RasterioIOError"), got
            assert "cannot allocate" in refused, got

def test_a_rust_panic_is_an_internal_error():
    """A Rust panic reaches Python as `terrio.errors.InternalError`, an `Exception`,
    with "panic" and the panic's text in its message (a fuzz classifier can count them so).
    Uncaught, PyO3 raises `PanicException`, a `BaseException` that `except Exception` misses."""
    from terrio.errors import InternalError, TerrioError

    with pytest.raises(InternalError, match="Rust panic in terrio: on purpose") as info:
        terrio._terrio._panic_for_tests("on purpose")
    assert isinstance(info.value, TerrioError) and isinstance(info.value, Exception)


def test_terrio_rio_raises_a_rasterio_error_caused_by_the_internal_error(tmp_path, monkeypatch):
    """`terrio.rio` callers get `RasterioIOError`, its cause the `InternalError`."""
    from terrio.errors import InternalError

    path = tmp_path / "x.tif"
    path.write_bytes(b"II*\0")

    def panicking_open(p, lazy=False):
        terrio._terrio._panic_for_tests(f"opening {p}")

    monkeypatch.setattr(terrio._terrio, "open_raster", panicking_open)
    with pytest.raises(RasterioIOError, match="Rust panic in terrio") as info:
        rio.open(path)
    assert isinstance(info.value.__cause__, InternalError)


# Comments, string and character literals of Rust source, blanked before it is searched.
RUST_NON_CODE = re.compile(
    r"//[^\n]*"  # line comments, doc comments included
    r"|/\*.*?\*/"  # block comments
    r'|r(#*)".*?"\1'  # raw strings
    r"|b?'(?:\\.|[^'\\\n])'"  # character literals
    r'|b?"(?:\\.|[^"\\])*"',  # strings
    re.S,
)

# Every way Rust code can release the GIL: PyO3's `detach` (`allow_threads` before PyO3 0.26) as
# a method, an associated function (`Python::detach(py, ..)`, `P::detach` through an alias) or a
# value, its guard type, and CPython's own calls.
GIL_RELEASES = re.compile(
    r"\b(detach|allow_threads|SuspendGIL|PyEval_SaveThread|Py_BEGIN_ALLOW_THREADS)\b"
)


def gil_releases(text):
    """The lines of Rust source `text` that release the GIL, its comments and literals aside."""
    code = RUST_NON_CODE.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)
    return [code.count("\n", 0, m.start()) + 1 for m in GIL_RELEASES.finditer(code)]


def test_the_gil_release_scan_sees_every_spelling():
    """The scan below sees each way of releasing the GIL (`Python::detach(py, ..)` among them,
    which a search for `.detach(` missed), and nothing in comments, strings or the guard's own
    `detach_result` and `detach_value`."""
    released = [
        "let out = py.detach(|| f());",
        "let out = Python::detach(py, || f());",
        "let out = pyo3::Python::detach(py, || f());",
        "let out = P::detach(py, f);",
        "let out = py.allow_threads(|| f());",
        "let out = Python::allow_threads(py, f);",
        "let run = Python::detach; run(py, f)",
        "let _unlocked = unsafe { pyo3::gil::SuspendGIL::new() };",
        "let state = unsafe { pyo3::ffi::PyEval_SaveThread() };",
    ]
    for line in released:
        assert gil_releases(f"fn f(py: Python<'_>) {{\n    {line}\n}}\n") == [2], line
    quiet = [
        "py.detach_result(|| f()).py()",
        "py.detach_value(|| f())",
        "// py.detach(|| f()) would let a panic through",
        "/* Python::detach(py, f) */ f()",
        'let s = "py.detach(|| f())";',
        'let s = r#"Python::detach(py, f)"#;',
        "let c = '\"'; let t = \"detach\";",
        "let detached = 1; let detaching = 2;",
    ]
    for line in quiet:
        assert gil_releases(f"fn f(py: Python<'_>) {{\n    {line}\n}}\n") == [], line


def test_the_bindings_release_the_gil_only_through_the_panic_guard():
    """Every binding that releases the GIL goes through `Detach` (python/src/lib.rs), which turns a
    panic into `InternalError`. Any other release would let `PanicException` through again, unseen
    until something panics there: no functional test covers every binding."""
    src = Path(__file__).resolve().parents[1] / "src"
    if not (src / "lib.rs").is_file():
        pytest.skip("the bindings' sources are not here")
    sites = []
    for f in sorted(src.rglob("*.rs")):
        text = f.read_text()
        if f.name == "lib.rs" and f.parent == src:
            start = text.index("impl Detach for Python")
            end = text.index("\n}\n", start)
            text = text[:start] + re.sub(r"[^\n]", " ", text[start:end]) + text[end:]
        sites += [f"{f.relative_to(src)}:{line}" for line in gil_releases(text)]
    assert not sites, f"the GIL released outside the Detach guard: {sites}"


def test_validate_cog_releases_the_gil_through_the_guard(tmp_path, counting_server, monkeypatch):
    """`validate_cog` runs in the panic guard (`with_warnings`), which releases the GIL while the
    file is read: a COG served by an HTTP server running as a thread of this process is validated,
    the server answering while the call waits for it. The bindings release the GIL only through
    the guard (the scan above), so the release seen here is the guard's. Called with the GIL held,
    the binding would starve the server, and its request would time out (after 10 s, not retried)
    rather than the test hang."""
    url, stats = counting_server
    data = (np.arange(200 * 300, dtype="float32").reshape(200, 300) % 1000).astype("float32")
    transform = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)
    cog = terrio.Raster.from_array(data, transform, "EPSG:32610", -9999.0)
    cog.write(str(tmp_path / "served.tif"), driver="COG")
    monkeypatch.setenv("TERRIO_HTTP_TIMEOUT", "10")
    monkeypatch.setenv("TERRIO_HTTP_MAX_RETRY", "0")
    assert terrio.validate_cog(f"{url}/served.tif") == (True, [], [])
    assert stats["requests"] >= 1, stats


# `dem` from a path to a path, with a color file that is a named pipe written by another thread.
DEM_WITH_A_PIPED_COLOR_FILE = r"""
import json, sys, threading
import terrio

src, colors, dst = sys.argv[1:]


def write_colors():
    with open(colors, "w") as f:  # opened once the reader opens the pipe
        f.write("0 0 0 0\n100 200 100 50\n")


threading.Thread(target=write_colors, daemon=True).start()
relief = terrio.dem(src, "color-relief", dst=dst, color_file=colors)
print(json.dumps(relief.read().tolist()))
"""


def test_dem_path_to_path_releases_the_gil_through_the_guard(tmp_path):
    """`dem` from a path to a path runs in the panic guard (`with_warnings`), which releases the
    GIL while the files are read and written: its color file, a named pipe that another Python
    thread of the same process writes once the call opens it, is read, and the relief written. As
    above, the release seen is the guard's.

    A concurrency check, chosen over extending the scan of the sources to every binding doing
    file or network I/O, and made deterministic rather than given margins: the call needs the
    other thread to run, so no timing threshold decides. With the GIL held, that thread could
    never write and the call would wait for it forever: it runs in a child process, killed after
    120 s. The scan would have to follow I/O through the helpers the bindings share and allow the
    sites that read files with the GIL held today, and it would pin where the work is written
    rather than that it runs without the GIL."""
    if not hasattr(os, "mkfifo"):
        pytest.skip("no named pipes")
    src = tmp_path / "dem.tif"
    dem = np.array([[0, 100, 50], [100, 0, 100]], dtype="float32")
    terrio.Raster.from_array(dem, (0.0, 1.0, 0.0, 2.0, 0.0, -1.0), crs="EPSG:32631").write(str(src))
    colors = tmp_path / "colors.txt"
    os.mkfifo(colors)
    out = tmp_path / "relief.tif"
    p = subprocess.run(
        [sys.executable, "-c", DEM_WITH_A_PIPED_COLOR_FILE, str(src), str(colors), str(out)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert p.returncode == 0, p.stderr[-600:]
    red = [[0, 200, 100], [200, 0, 200]]
    green = [[0, 100, 50], [100, 0, 100]]
    blue = [[0, 50, 25], [50, 0, 50]]
    assert json.loads(p.stdout) == [red, green, blue]


# Half the width of the Web Mercator world, in meters (EPSG:3857).
MAX_GM = 20037508.342789244


def geotiff_on_the_tile_grid(width, height, zoom, tile=256):
    """A little-endian GeoTIFF of one deflated 8-bit band in EPSG:3857, on zoom level `zoom`'s
    pixel grid (its left edge the world's, its top the equator), whose tiles all point to one
    compressed tile of zeros: a raster a tile archive is written from a strip of rows at a time.
    `entries` are `(tag, type, values)`: 3 SHORT, 4 LONG, 12 DOUBLE."""
    res = 2 * MAX_GM / 256 / 2**zoom
    tiles = -(-width // tile) * -(-height // tile)
    data = zlib.compress(b"\0" * tile * tile)
    geokeys = [1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, 3857]
    entries = [
        (256, 4, [width]),
        (257, 4, [height]),
        (258, 3, [8]),
        (259, 3, [8]),
        (262, 3, [1]),
        (277, 3, [1]),
        (284, 3, [1]),
        (322, 4, [tile]),
        (323, 4, [tile]),
        (324, 4, [0] * tiles),
        (325, 4, [len(data)] * tiles),
        (33550, 12, [res, res, 0.0]),
        (33922, 12, [0.0, 0.0, 0.0, -MAX_GM, 0.0, 0.0]),
        (34735, 3, geokeys),
    ]
    code = {3: "H", 4: "I", 12: "d"}
    at = 8 + 2 + 12 * len(entries) + 4
    packed = [struct.pack(f"<{len(v)}{code[t]}", *v) for _, t, v in entries]
    data_at = at + sum(len(p) for p in packed if len(p) > 4)
    packed[9] = struct.pack(f"<{tiles}I", *[data_at] * tiles)
    arrays, ifd = bytearray(), bytearray(struct.pack("<H", len(entries)))
    for (tag, typ, values), p in zip(entries, packed):
        if len(p) > 4:
            ifd += struct.pack("<HHII", tag, typ, len(values), at + len(arrays))
            arrays += p
        else:
            ifd += struct.pack("<HHI", tag, typ, len(values)) + p.ljust(4, b"\0")
    return b"II*\0" + struct.pack("<I", 8) + bytes(ifd) + struct.pack("<I", 0) + bytes(arrays) + data


def test_a_tile_pyramid_too_wide_to_allocate_raises(tmp_path):
    """A tile archive written from a raster on its zoom level's pixel grid (`Raster.write` of a
    lazy raster to PMTiles) reads it a strip of 256 rows at a time, into rows sized from its
    header: a GeoTIFF of 2^23 x 256 pixels (the world's width at zoom level 15, 260 KiB on disk)
    whose strip and its alpha take 4 GiB, more than the address space left, raises; written whole,
    as before the strips, the 2 GiB of its pixels raise alike."""
    path = tmp_path / "wide.tif"
    path.write_bytes(geotiff_on_the_tile_grid(1 << 23, 256, 15))
    with rio.open(path) as ds:
        assert (ds.width, ds.height, ds.crs.to_epsg()) == (1 << 23, 256, 3857)
    assert_raises_allocation_error(read_limited("tiles", path))
    assert not list(tmp_path.glob("*.pmtiles")) and not list(tmp_path.glob(".*.tmp"))


def test_tile_archives_of_a_lazy_raster_release_the_gil_through_the_guard(tmp_path, counting_server, monkeypatch):
    """A tile archive written from a raster read by window (`Raster.write` of a lazy raster, and
    `convert`, both built a tile row at a time) is written in the panic guard, which releases the
    GIL while the raster is read: a COG served by an HTTP server running as a thread of this
    process is read during the write, the server answering while the call waits for it, as for
    `validate_cog` above. Called with the GIL held, the write would starve the server, and its
    request would time out (after 10 s, not retried)."""
    url, stats = counting_server
    # 1024 x 1024 pixels of RGB noise on zoom level 12's pixel grid: more than an open reads.
    res = 2 * MAX_GM / 256 / 2**12
    transform = (-MAX_GM + 1000 * 256 * res, res, 0.0, MAX_GM - 700 * 256 * res, 0.0, -res)
    noise = np.random.default_rng(7).integers(0, 256, size=(3, 1024, 1024), dtype="uint8")
    terrio.Raster.from_array(noise, transform, crs="EPSG:3857").write(str(tmp_path / "served.tif"), driver="COG")
    # A copy for `convert`: what the first write read is cached.
    shutil.copyfile(tmp_path / "served.tif", tmp_path / "copy.tif")
    monkeypatch.setenv("TERRIO_HTTP_TIMEOUT", "10")
    monkeypatch.setenv("TERRIO_HTTP_MAX_RETRY", "0")
    lazy = terrio.open_raster(f"{url}/served.tif", lazy=True)
    opened = stats["requests"]
    lazy.write(str(tmp_path / "lazy.pmtiles"), options={"MAXZOOM": "12"})
    written = stats["requests"]
    assert written > opened, stats
    terrio.convert(f"{url}/copy.tif", str(tmp_path / "converted.mbtiles"))
    assert stats["requests"] > written, stats
    for name in ("lazy.pmtiles", "converted.mbtiles"):
        assert terrio.info(str(tmp_path / name))["driver"] in ("PMTiles", "MBTiles"), name


TILES_CHILD = r"""
import sys
import terrio

src = sys.argv[1]
terrio.set_allowed_drivers(sys.argv[2].split(","), kind="raster")
for ext in ("pmtiles", "mbtiles"):
    calls = (
        ("convert", lambda: terrio.convert(src, f"{src}.{ext}")),
        ("write", lambda: terrio.open_raster(src, lazy=True).write(f"{src}.lazy.{ext}")),
    )
    for name, call in calls:
        try:
            call()
            print(name, "WROTE")
        except Exception as e:
            print(name, f"REFUSED {type(e).__name__}: {e}")
"""


def test_tile_archives_are_written_only_from_rasters_with_their_driver_allowed(tmp_path):
    """`convert` to a tile archive, and `Raster.write` of a lazy raster to one, open the raster
    with its driver judged: under a raster list without GTiff a GeoTIFF is refused, and nothing
    is written; with GTiff alone it is written (the archive's own driver is not judged, as no
    writer's is). In a child process: the allow-list is process-wide and cannot be lifted."""
    tif = tmp_path / "x.tif"
    res = 2 * MAX_GM / 256 / 2**10
    transform = (-MAX_GM + 1000 * 256 * res, res, 0.0, MAX_GM - 700 * 256 * res, 0.0, -res)
    terrio.Raster.from_array(np.full((1, 300, 200), 7, "uint8"), transform, crs="EPSG:3857").write(str(tif))

    def child(drivers):
        p = subprocess.run([sys.executable, "-c", TILES_CHILD, str(tif), drivers], capture_output=True, text=True, timeout=120)
        assert p.returncode == 0, p.stderr[-600:]
        return p.stdout.splitlines()

    # The lazy open judges the raster list; `convert`, refused by its streamed path's lazy open,
    # opens the file as it did before the streamed path, judging both kinds' lists.
    refused = f"REFUSED UnsupportedError: '{tif}': the GTIFF/COG driver is not allowed (allowed: "
    out = child("PNG")
    assert [line.split(" ", 1)[0] for line in out] == ["convert", "write"] * 2, out
    assert all(line.split(" ", 1)[1].startswith(refused) for line in out), out
    assert [line for line in out if line.startswith("write")] == [f"write {refused}PNG)"] * 2
    assert sorted(p.name for p in tmp_path.iterdir()) == ["x.tif"]
    assert child("GTiff") == ["convert WROTE", "write WROTE"] * 2
    names = sorted(p.name for p in tmp_path.iterdir())
    assert names == ["x.tif", "x.tif.lazy.mbtiles", "x.tif.lazy.pmtiles", "x.tif.mbtiles", "x.tif.pmtiles"]


def test_identification_and_content_checks_release_the_gil_through_the_guard(tmp_path, counting_server, monkeypatch):
    """`identify` and `check_dataset` read in the panic guard
    (`with_warnings`), which releases the GIL while they read: a COG served by an HTTP server
    running as a thread of this process is identified from its first bytes, and a copy is read
    whole by the check, the server answering while each call waits, as for `validate_cog` above.
    Called with the GIL held, a call would starve the server, and its request would time out
    (after 10 s, not retried). Each call reads its own file: what one reads is cached."""
    url, stats = counting_server
    noise = np.random.default_rng(7).integers(0, 256, size=(1, 512, 512), dtype="uint8")
    transform = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)
    terrio.Raster.from_array(noise, transform, crs="EPSG:32610").write(str(tmp_path / "a.tif"), driver="COG")
    shutil.copyfile(tmp_path / "a.tif", tmp_path / "b.tif")
    monkeypatch.setenv("TERRIO_HTTP_TIMEOUT", "10")
    monkeypatch.setenv("TERRIO_HTTP_MAX_RETRY", "0")
    found = terrio.identify(f"{url}/a.tif", report_failures=True)
    assert [record.get("driver") for record in found] == ["GTiff"], found
    assert stats["requests"] == 1, stats
    summary = terrio.check_dataset(f"{url}/b.tif")
    assert summary["raster_samples"] == 512 * 512, summary
    assert stats["requests"] > 2, stats


INSPECTION_CHILD = r"""
import json, sys
import terrio

path, drivers = sys.argv[1], sys.argv[2]
terrio.set_allowed_drivers(drivers.split(","))
data = open(path, "rb").read()
name = path.rsplit("/", 1)[-1]
out = {
    "identify": [r.get("driver") for r in terrio.identify(path, report_failures=True)],
    "identify bytes": [r.get("driver") for r in terrio.identify(data, filename=name, report_failures=True)],
}
for label, call in (("check", lambda: terrio.check_dataset(path)), ("check bytes", lambda: terrio.check_dataset(data, filename=name))):
    try:
        out[label] = call()["raster_samples"]
    except Exception as e:
        out[label] = f"{type(e).__name__}: {e}"
print(json.dumps(out))
"""


def test_identification_and_content_checks_see_only_allowed_drivers(tmp_path):
    """Under a list without GTiff, a GeoTIFF is not identified (as GDAL identifies a dataset only
    with its registered drivers) and its content check refuses it, from a path and from bytes;
    with GTiff it is identified and read. In a child process: the allow-list is process-wide and
    cannot be lifted."""
    tif = tmp_path / "x.tif"
    terrio.Raster.from_array(np.full((1, 3, 5), 7, "uint8"), (0.0, 1.0, 0.0, 3.0, 0.0, -1.0), crs="EPSG:4326").write(str(tif))

    def child(drivers):
        p = subprocess.run([sys.executable, "-c", INSPECTION_CHILD, str(tif), drivers], capture_output=True, text=True, timeout=120)
        assert p.returncode == 0, p.stderr[-600:]
        return json.loads(p.stdout)

    refused = child("PNG")
    assert refused["identify"] == [None] and refused["identify bytes"] == [None], refused
    for label in ("check", "check bytes"):
        assert "UnsupportedError" in refused[label] and "not allowed" in refused[label], refused
    assert child("PNG,GTiff") == {"identify": ["GTiff"], "identify bytes": ["GTiff"], "check": 15, "check bytes": 15}


def test_identify_drivers_is_the_allow_lists_view_and_identify_goes_by_content(tmp_path):
    """`identify_drivers(path) -> (by_name, by_content)` is the allow-list's view of a file by its
    name and first bytes, beside the records of `terrio.identify` (format identification), which
    go by the content."""
    tif = tmp_path / "x.tif"
    terrio.Raster.from_array(np.zeros((1, 4, 4), "uint8"), (0.0, 1.0, 0.0, 4.0, 0.0, -1.0), crs="EPSG:4326").write(str(tif))
    shutil.copyfile(tif, tmp_path / "disguised.png")
    assert terrio.identify_drivers(str(tmp_path / "disguised.png")) == (["PNG"], "GTIFF")
    assert [r["driver"] for r in terrio.identify(str(tmp_path / "disguised.png"))] == ["GTiff"]
    assert "identify_drivers" in terrio.__all__ and terrio.__all__.count("identify") == 1


def test_identify_drivers_releases_the_gil_through_the_guard(
    tmp_path, counting_server, monkeypatch
):
    """`identify_drivers` reads a remote file's first bytes in the panic guard (`detach_result`),
    which releases the GIL while it reads: a GeoTIFF whose name points to no driver, served by an
    HTTP server running as a thread of this process, is identified by its first bytes, the server
    answering while the call waits, as for `validate_cog` above. Called with the GIL held, the
    call would starve the server, its request would time out (after 10 s, not retried) and the
    content would go unseen."""
    url, stats = counting_server
    tif = tmp_path / "x.tif"
    data = np.zeros((1, 4, 4), "uint8")
    terrio.Raster.from_array(data, (0.0, 1.0, 0.0, 4.0, 0.0, -1.0), crs="EPSG:4326").write(str(tif))
    shutil.copyfile(tif, tmp_path / "served")
    monkeypatch.setenv("TERRIO_HTTP_TIMEOUT", "10")
    monkeypatch.setenv("TERRIO_HTTP_MAX_RETRY", "0")
    assert terrio.identify_drivers(f"{url}/served") == ([], "GTIFF")
    assert stats["requests"] == 1, stats


def test_content_checks_of_uploads_release_the_gil_through_the_guard(tmp_path):
    """`check_dataset` of uploaded bytes decodes them in the panic guard (`with_warnings`), which
    releases the GIL: a thread that wakes every millisecond to count runs while the check decodes
    an 8192 x 8192 Deflate GeoTIFF from memory. With a switch interval of 5 s, the thread gets the
    GIL during the call only if the call releases it: held, the count could not move until the
    call returned."""
    tif = tmp_path / "big.tif"
    profile = dict(driver="GTiff", width=8192, height=8192, count=1, dtype="uint8", tiled=True,
                   blockxsize=512, blockysize=512, compress="deflate")
    with rio.open(tif, "w", **profile) as dst:
        dst.write((np.arange(8192 * 8192, dtype="uint32") % 251).astype("uint8").reshape(1, 8192, 8192))
    upload = tif.read_bytes()
    counts, stop = [0], threading.Event()

    def count():
        while not stop.wait(0.001):
            counts[0] += 1

    interval = sys.getswitchinterval()
    sys.setswitchinterval(5.0)
    worker = threading.Thread(target=count, daemon=True)
    try:
        worker.start()
        stop.wait(0.05)
        before = counts[0]
        summary = terrio.check_dataset(upload, filename="big.tif")
        during = counts[0] - before
    finally:
        stop.set()
        worker.join()
        sys.setswitchinterval(interval)
    assert summary["raster_samples"] == 8192 * 8192, summary
    assert during > 0, during
