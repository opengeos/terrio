"""The temporary file of a GeoTIFF writer larger than `TERRIO_RIO_IN_MEMORY_LIMIT`.

Such a writer keeps its pixels in a temporary file beside the output, mapped in memory. The file
has no name, so that it goes with the process however that ends; it is created without nodata
written to it; and closing the dataset reads back only the blocks written, the others being
nodata (or 0) without being read. Pixels never written read as nodata, as before.
"""

import json
import mmap
import os
import re
import stat
import subprocess
import sys
import textwrap

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.io import DatasetWriter
from terrio.rio.windows import Window

linux = pytest.mark.skipif(
    not os.path.isdir("/proc/self/fd"), reason="finds open files and the peak RSS in /proc"
)


def held(directory):
    """The regular files in `directory` this process holds open, named or not: `(name as /proc
    links it, size, allocated bytes)`, once each (a file can be open several times)."""
    found = {}
    for fd in os.listdir("/proc/self/fd"):
        link = f"/proc/self/fd/{fd}"
        try:
            name, st = os.readlink(link), os.stat(link)
        except OSError:
            continue
        if stat.S_ISREG(st.st_mode) and os.path.dirname(name.removesuffix(" (deleted)")) == str(
            directory
        ):
            found[st.st_dev, st.st_ino] = (name, st.st_size, st.st_blocks * 512)
    return list(found.values())


def mapped(name):
    """The bytes of the file `name` (as /proc links it) mapped in this process and resident."""
    total, inside = 0, False
    with open("/proc/self/smaps") as f:
        for line in f:
            if re.match(r"[0-9a-f]+-[0-9a-f]+ ", line):
                inside = line.rstrip("\n").split(maxsplit=5)[5:] == [name]
            elif inside and line.startswith("Rss:"):
                total += int(line.split()[1]) * 1024
    return total


@linux
def test_the_temporary_file_is_beside_the_output_without_a_name_or_nodata(tmp_path, monkeypatch):
    """Beside the output, not in the system's temporary directory; without a name, so that a
    killed writer leaves nothing; without nodata written to it when it is created; and closed
    with the dataset, even while something still references its array."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1000000")
    with rio.open(
        tmp_path / "out.tif",
        "w",
        driver="GTiff",
        width=4096,
        height=4096,
        count=1,
        dtype="uint16",
        nodata=7,
        tiled=True,
    ) as dst:
        data = dst._data
        assert isinstance(data, np.memmap)
        assert [p.name for p in tmp_path.iterdir()] == []
        files = held(tmp_path)
        assert len(files) == 1 and files[0][0].endswith(" (deleted)"), files
        assert files[0][1] == 4096 * 4096 * 2
        assert files[0][2] < 1 << 20, f"{files[0][2]:,} bytes allocated when created"
        dst.write(np.ones((256, 256), "uint16"), 1, window=Window(0, 0, 256, 256))
    assert held(tmp_path) == []
    assert [p.name for p in tmp_path.iterdir()] == ["out.tif"]


@pytest.mark.parametrize("whole", [False, True], ids=["in-rows", "whole"])
@pytest.mark.parametrize(
    "layout", [dict(tiled=True, blockxsize=64, blockysize=32), {}], ids=["tiles", "strips"]
)
def test_pixels_never_written_are_nodata(tmp_path, monkeypatch, layout, whole):
    """Also those of blocks a window covers in part, whichever of its sides cuts them, and in
    the mask (255): in the dataset (`w+`), and in the file, written in rows, or whole when it
    has overviews."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1000")
    expected = np.full((2, 200, 300), 7, "uint16")
    mask = np.full((200, 300), 255, "uint8")
    mask[150:170, 40:100] = 0
    path = tmp_path / "out.tif"
    profile = dict(driver="GTiff", width=300, height=200, count=2, dtype="uint16", nodata=7)
    with rio.open(path, "w+", compress="deflate", **profile, **layout) as dst:
        bh, bw = dst.block_shapes[0]
        # In a row of blocks each, windows that cut them at the top, bottom, left and right.
        for i, (y, x, h, w) in enumerate(
            [(bh // 2, 0, bh - bh // 2, bw), (0, 0, bh // 2, bw), (0, bw // 2, bh, bw - bw // 2)]
            + [(0, 0, bh, bw // 2)]
        ):
            dst.write(np.ones((2, h, w), "uint16"), window=Window(x, i * bh + y, w, h))
            expected[:, i * bh + y : i * bh + y + h, x : x + w] = 1
        dst.write(np.full((40, 70), 2, "uint16"), 2, window=Window(150, 150, 70, 40))
        expected[1, 150:190, 150:220] = 2
        dst.write_mask(False, window=Window(40, 150, 60, 20))
        assert isinstance(dst._data, np.memmap) and isinstance(dst._mask, np.memmap)
        np.testing.assert_array_equal(dst.read(), expected)
        np.testing.assert_array_equal(
            dst.read(2, window=Window(140, 140, 100, 60)), expected[1, 140:200, 140:240]
        )
        np.testing.assert_array_equal(dst.dataset_mask(), mask)
        if whole:
            dst.build_overviews([2])
    with rio.open(path) as src:
        np.testing.assert_array_equal(src.read(), expected)
        np.testing.assert_array_equal(src.dataset_mask(), mask)


@linux
def test_closing_reads_back_only_the_blocks_written(tmp_path):
    """16384 x 16384 uint16 (512 MiB), of which one 512 x 512 window is written: closing the
    dataset grows the peak RSS by less than a quarter of that, the pages of the temporary file
    never written being neither read nor mapped."""
    script = textwrap.dedent(
        """
        import json, sys
        import numpy as np
        import terrio.rio as rio
        from terrio.rio.windows import Window

        def peak():
            with open("/proc/self/status") as f:
                return next(int(line.split()[1]) for line in f if line.startswith("VmHWM:")) * 1024

        dst = rio.open(sys.argv[1], "w", driver="GTiff", width=16384, height=16384, count=1,
                       dtype="uint16", tiled=True, blockxsize=512, blockysize=512,
                       compress="deflate")
        dst.write(np.full((512, 512), 7, "uint16"), 1, window=Window(0, 0, 512, 512))
        base = peak()
        dst.close()
        print(json.dumps(peak() - base))
        """
    )
    out = subprocess.run(
        [sys.executable, "-c", script, str(tmp_path / "out.tif")],
        capture_output=True,
        text=True,
        check=True,
        env=dict(os.environ, TERRIO_RIO_IN_MEMORY_LIMIT="1000000"),
    )
    grown = json.loads(out.stdout.splitlines()[-1])
    assert grown < (512 << 20) // 4, f"the peak RSS grew by {grown:,} bytes"


@linux
@pytest.mark.parametrize(
    "layout",
    [dict(blockysize=16), dict(tiled=True, blockxsize=64, blockysize=32)],
    ids=["strips", "tiles"],
)
def test_a_window_allocates_and_maps_only_the_blocks_it_touches(tmp_path, monkeypatch, layout):
    """Blocks that are not square, strips 16 rows tall or tiles 32 rows by 64 columns, with
    nodata 7: one small window allocates in the temporary file only the pages of the output's
    blocks it touches (which take nodata where it covers them in part), and once `close()` has
    read back the rows, no other page of the file is mapped. The tests above measure pages with
    square tiles only, the same whichever of a block's sides is taken for its rows."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1000000")
    size, (y, x, h, w) = 2048, (40, 100, 20, 50)
    resident = []
    original = DatasetWriter._write_streaming

    def measured(self, *args, **kwargs):
        # Once the rows are written, before `close()` releases the file.
        original(self, *args, **kwargs)
        resident.append(mapped(name))

    monkeypatch.setattr(DatasetWriter, "_write_streaming", measured)
    with rio.open(
        tmp_path / "out.tif",
        "w",
        driver="GTiff",
        width=size,
        height=size,
        count=1,
        dtype="uint16",
        nodata=7,
        **layout,
    ) as dst:
        bh = dst.block_shapes[0][0]
        dst.write(np.ones((h, w), "uint16"), 1, window=Window(x, y, w, h))
        [(name, _, allocated)] = held(tmp_path)
    # The pages that hold the blocks touched are those of their rows: a row of the file (4 KiB)
    # holds a row of every block across it.
    top, bottom, row = y // bh * bh, min(-(-(y + h) // bh) * bh, size), size * 2
    blocks = (-(-bottom * row // mmap.PAGESIZE) - top * row // mmap.PAGESIZE) * mmap.PAGESIZE
    assert 0 < allocated <= blocks, f"{allocated:,} bytes allocated, {blocks:,} for the blocks"
    assert len(resident) == 1 and 0 < resident[0] <= blocks, f"{resident} bytes mapped at close"
