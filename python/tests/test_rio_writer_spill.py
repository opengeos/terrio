"""The temporary file of a GeoTIFF writer larger than `TERRIO_RIO_IN_MEMORY_LIMIT`.

Such a writer keeps its pixels in a temporary file beside the output, mapped in memory. The file
has no name, so that it goes with the process however that ends.
"""

import os
import stat

import numpy as np
import pytest

import terrio.rio as rio
from terrio.rio.windows import Window

linux = pytest.mark.skipif(not os.path.isdir("/proc/self/fd"), reason="finds open files in /proc")


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


@linux
def test_the_temporary_file_is_beside_the_output_without_a_name(tmp_path, monkeypatch):
    """Beside the output, not in the system's temporary directory; without a name, so that a
    killed writer leaves nothing; and closed with the dataset, even while something still
    references its array."""
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
        dst.write(np.ones((256, 256), "uint16"), 1, window=Window(0, 0, 256, 256))
    assert held(tmp_path) == []
    assert [p.name for p in tmp_path.iterdir()] == ["out.tif"]
