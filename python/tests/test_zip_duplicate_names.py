"""Ambiguous ZIP members cannot silently replace another dataset or workbook part."""

import io
import json
import warnings
import zipfile

import numpy as np
import pytest

import terrio


def archive(entries):
    """Build an archive that deliberately retains duplicate member names.

    Args:
        entries: Pairs of member names and contents.

    Returns:
        bytes: Complete ZIP archive.
    """
    buffer = io.BytesIO()
    with warnings.catch_warnings():
        warnings.filterwarnings("ignore", message="Duplicate name:", category=UserWarning)
        with zipfile.ZipFile(buffer, "w") as output:
            for name, contents in entries:
                output.writestr(name, contents)
    return buffer.getvalue()


def collection(name):
    """Create a named single-feature GeoJSON dataset.

    Args:
        name: Dataset name and property value.

    Returns:
        str: GeoJSON feature collection.
    """
    return json.dumps(
        {
            "type": "FeatureCollection",
            "name": name,
            "features": [
                {
                    "type": "Feature",
                    "properties": {"source": name},
                    "geometry": {"type": "Point", "coordinates": [1, 2]},
                }
            ],
        }
    )


@pytest.mark.parametrize("mode", ["path", "bytes", "member"])
def test_duplicate_vector_names_are_invalid(tmp_path, mode):
    """Every archive entry point refuses conflicting dataset members.

    Args:
        tmp_path: Temporary directory.
        mode: Public archive opening interface.
    """
    data = archive([("data.geojson", collection("first")), ("data.geojson", collection("second"))])
    path = tmp_path / "vector.zip"
    path.write_bytes(data)
    with pytest.raises(terrio.InvalidDataError, match="several entries with the same name"):
        if mode == "bytes":
            terrio.open_bytes(data, "vector.zip")
        elif mode == "member":
            terrio.open_vector(f"/vsizip/{path}/data.geojson")
        else:
            terrio.open_vector(str(path))


def test_duplicate_raster_member_names_are_invalid(tmp_path):
    """A later ZIP member cannot replace a raster under a /vsizip/ path.

    Args:
        tmp_path: Temporary directory.
    """
    rasters = []
    for value in [1, 2]:
        path = tmp_path / f"{value}.tif"
        terrio.Raster.from_array(np.full((4, 4), value, "uint8"), (0, 1, 0, 4, 0, -1)).write(
            str(path)
        )
        rasters.append(path.read_bytes())
    path = tmp_path / "raster.zip"
    path.write_bytes(archive([("dem.tif", data) for data in rasters]))
    with pytest.raises(terrio.InvalidDataError, match="several entries with the same name"):
        terrio.open_raster(f"/vsizip/{path}/dem.tif")


@pytest.mark.parametrize("duplicate", ["xl/sharedStrings.xml", "unused.txt"])
@pytest.mark.parametrize("mode", ["path", "bytes"])
def test_duplicate_workbook_parts_are_invalid(tmp_path, duplicate, mode):
    """Workbooks refuse duplicate names, including unused parts, before reading cells.

    Args:
        tmp_path: Temporary directory.
        duplicate: Workbook part or unrelated member to duplicate.
        mode: Public opening interface.
    """
    path = tmp_path / "points.xlsx"
    terrio.Layer.from_geojson(collection("first"), name="points").write(str(path))
    with zipfile.ZipFile(path) as workbook:
        entries = [(name, workbook.read(name)) for name in workbook.namelist()]
    if duplicate not in dict(entries):
        entries.append((duplicate, b"first"))
    entries.append((duplicate, dict(entries)[duplicate].replace(b"first", b"secnd")))
    data = archive(entries)
    path.write_bytes(data)
    with pytest.raises(terrio.InvalidDataError, match="several entries with the same name"):
        if mode == "bytes":
            terrio.open_bytes(data, "points.xlsx")
        else:
            terrio.open_vector(str(path))


def test_distinct_archive_names_still_open(tmp_path):
    """A unique dataset and sidecar retain ordinary archive behavior.

    Args:
        tmp_path: Temporary directory.
    """
    path = tmp_path / "vector.zip"
    data = archive([("data.geojson", collection("first")), ("data.txt", "sidecar")])
    path.write_bytes(data)
    assert len(terrio.open_vector(str(path))[0]) == 1
    assert len(terrio.open_bytes(data, "vector.zip")[0]) == 1
