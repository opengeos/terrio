# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Public CRS strings are definitions, never implicit requests to read another file."""

import json

import numpy as np
import pytest

import terrio
from terrio.rio.crs import CRS


@pytest.mark.parametrize("contents", ["EPSG:32633", "CONTROLLED_PRIVATE_CRS_MARKER"])
@pytest.mark.parametrize("constructor", ["from_user_input", "from_string", "from_wkt"])
def test_rio_crs_constructors_do_not_open_files(tmp_path, contents, constructor):
    """Reject a file name independently of whether its contents happen to be a valid CRS.

    Args:
        tmp_path: Temporary directory holding the controlled input file.
        contents: Valid CRS text or a private marker.
        constructor: Rasterio-compatible CRS constructor.
    """
    path = tmp_path / "definition.prj"
    path.write_text(contents)
    with pytest.raises(Exception) as raised:
        getattr(CRS, constructor)(str(path))
    assert "CONTROLLED_PRIVATE_CRS_MARKER" not in str(raised.value)


@pytest.mark.parametrize("contents", ["EPSG:32633", "CONTROLLED_PRIVATE_CRS_MARKER"])
@pytest.mark.parametrize("api", ["info", "points", "array", "warp", "bounds"])
def test_native_crs_inputs_are_literal(tmp_path, contents, api):
    """Refuse implicit file reads through native and facade operations.

    Args:
        tmp_path: Temporary directory holding the controlled input file.
        contents: Valid CRS text or a private marker.
        api: Entry point accepting a CRS string.
    """
    from terrio.rio.warp import transform_bounds

    path = tmp_path / "definition.prj"
    path.write_text(contents)
    pixels = np.ones((2, 2), dtype="uint8")
    source = terrio.Raster.from_array(pixels, (0, 1, 0, 2, 0, -1), crs="EPSG:4326")
    calls = {
        "info": lambda: terrio._terrio.crs_info(str(path)),
        "points": lambda: terrio.transform_points([0.0], [0.0], str(path), "EPSG:4326"),
        "array": lambda: terrio.Raster.from_array(pixels, (0, 1, 0, 2, 0, -1), crs=str(path)),
        "warp": lambda: terrio.warp(source, t_srs=str(path)),
        "bounds": lambda: transform_bounds(str(path), "EPSG:4326", 0, 0, 1, 1),
    }
    with pytest.raises(Exception) as raised:
        calls[api]()
    assert "CONTROLLED_PRIVATE_CRS_MARKER" not in str(raised.value)


@pytest.mark.parametrize("contents", ["EPSG:32633", "CONTROLLED_PRIVATE_CRS_MARKER"])
def test_cityjson_crs_text_cannot_read_a_file(tmp_path, contents):
    """Reject a stored filename as CRS metadata without disclosing its contents.

    Args:
        tmp_path: Temporary directory holding the controlled dataset and file.
        contents: Valid CRS text or a private marker.
    """
    secret = tmp_path / "settings.txt"
    secret.write_text(contents)
    path = tmp_path / "building.city.json"
    path.write_text(
        json.dumps(
            {
                "type": "CityJSON",
                "version": "2.0",
                "CityObjects": {},
                "vertices": [],
                "metadata": {"referenceSystem": str(secret)},
            }
        )
    )
    with pytest.raises(Exception) as raised:
        terrio.open_vector(str(path))
    assert "CONTROLLED_PRIVATE_CRS_MARKER" not in str(raised.value)


@pytest.mark.parametrize("contents", ["EPSG:32633", "CONTROLLED_PRIVATE_CRS_MARKER"])
@pytest.mark.parametrize("driver", ["csv", "parquet"])
def test_stored_crs_text_does_not_adopt_another_file(tmp_path, contents, driver):
    """Read stored CRS text literally while retaining its normal unsupported-metadata policy.

    Args:
        tmp_path: Temporary directory holding the controlled dataset and file.
        contents: Valid CRS text or a private marker.
        driver: CSV sidecar or GeoParquet metadata.
    """
    secret = tmp_path / "definition.txt"
    secret.write_text(contents)
    if driver == "csv":
        path = tmp_path / "points.csv"
        path.write_text('id,WKT\n1,"POINT (1 2)"\n')
        path.with_suffix(".prj").write_text(str(secret))
    else:
        arrow = pytest.importorskip("pyarrow")
        parquet = pytest.importorskip("pyarrow.parquet")
        path = tmp_path / "points.parquet"
        geo = {
            "version": "1.0.0",
            "primary_column": "geometry",
            "columns": {
                "geometry": {"encoding": "WKB", "geometry_types": ["Point"], "crs": str(secret)}
            },
        }
        point = bytes.fromhex("0101000000000000000000f03f0000000000000040")
        table = arrow.table({"id": [1], "geometry": [point]}).replace_schema_metadata(
            {"geo": json.dumps(geo)}
        )
        parquet.write_table(table, path)
    layer = terrio.open_vector(str(path)).layers[0]
    assert len(layer) == 1
    assert layer.crs is None or CRS.from_user_input(layer.crs).to_epsg() != 32633


@pytest.mark.parametrize("contents", ["EPSG:32633", "CONTROLLED_PRIVATE_CRS_MARKER"])
def test_xarray_crs_attribute_is_literal(tmp_path, contents):
    """Reject a CRS attribute naming a local file without reading its definition.

    Args:
        tmp_path: Temporary directory holding the controlled file.
        contents: Valid CRS text or a private marker.
    """
    xarray = pytest.importorskip("xarray")
    import terrio.xarray  # noqa: F401

    path = tmp_path / "definition.txt"
    path.write_text(contents)
    dataset = xarray.Dataset({"v": (("y", "x"), np.ones((2, 2)))}, attrs={"crs": str(path)})
    with pytest.raises(Exception) as raised:
        _ = dataset.terrio.crs
    assert "CONTROLLED_PRIVATE_CRS_MARKER" not in str(raised.value)
