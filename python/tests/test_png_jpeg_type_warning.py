"""PNG and JPEG writes of a type the format cannot hold warn, naming both types.

PNG holds Byte and UInt16 bands and terrio's JPEG writer Byte bands: other types are rounded and
saturated to them. GDAL 3.13.3's PNG and JPEG drivers warn before they convert a type they cannot
write (`CreateCopy` without `strict`); GDAL writes UInt16 JPEGs as 12-bit with no type
warning, so terrio's UInt16 warning has no GDAL counterpart. terrio wrote the converted file
without a word (issue #1342).
"""

import json
import warnings

import numpy as np
import pytest

import terrio
import terrio.rio as rio


def _write(path, driver, data):
    with rio.open(
        path,
        "w",
        driver=driver,
        width=data.shape[1],
        height=data.shape[0],
        count=1,
        dtype=data.dtype,
    ) as dst:
        dst.write(data, 1)


@pytest.mark.parametrize(
    ("driver", "dtype", "declared", "written"),
    [
        ("PNG", "float32", "Float32", "UInt16"),
        ("PNG", "int32", "Int32", "UInt16"),
        ("PNG", "int8", "Int8", "Byte"),
        ("JPEG", "float32", "Float32", "Byte"),
        ("JPEG", "uint16", "UInt16", "Byte"),
    ],
)
def test_a_type_the_format_cannot_hold_is_written_with_a_warning(
    tmp_path, driver, dtype, declared, written
):
    path = tmp_path / f"out.{driver.lower()}"
    message = f"{driver} driver doesn't support data type {declared}: written as {written}"
    with pytest.warns(UserWarning, match=message):
        _write(path, driver, np.full((16, 16), 7, dtype))
    with rio.open(path) as src:
        assert src.dtypes[0] == np.dtype(written.lower().replace("byte", "uint8")).name
        assert src.read(1)[0, 0] == 7


@pytest.mark.parametrize(
    ("driver", "dtype"), [("PNG", "uint8"), ("PNG", "uint16"), ("JPEG", "uint8")]
)
def test_a_type_the_format_holds_is_written_without_one(tmp_path, driver, dtype):
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        _write(tmp_path / f"out.{driver.lower()}", driver, np.full((16, 16), 7, dtype))
    assert [str(w.message) for w in caught if "driver doesn't support" in str(w.message)] == []


def test_an_operation_writing_its_output_warns_too(tmp_path):
    """`rasterize` writes its `dst` (Float64 by default) to PNG as UInt16, with the warning."""
    shapes = tmp_path / "square.geojson"
    ring = [[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]
    feature = {
        "type": "Feature",
        "properties": {},
        "geometry": {"type": "Polygon", "coordinates": [ring]},
    }
    shapes.write_text(json.dumps({"type": "FeatureCollection", "features": [feature]}))
    with pytest.warns(
        UserWarning, match="PNG driver doesn't support data type Float64: written as UInt16"
    ):
        terrio.rasterize(
            str(shapes), str(tmp_path / "burn.png"), burn=7, ts=(16, 16), te=(0, 0, 10, 10)
        )


def test_a_pipeline_writing_its_output_warns_too(tmp_path):
    source = terrio.Raster.from_array(
        np.full((16, 16), 7.5, "float32"), [0, 1, 0, 16, 0, -1], "EPSG:4326"
    )
    path = tmp_path / "out.jpg"
    with pytest.warns(
        UserWarning, match="JPEG driver doesn't support data type Float32: written as Byte"
    ):
        terrio.pipeline(f"read image ! write '{path}'", {"image": source}, write=True)
    assert path.exists()


def test_bytes_of_a_type_the_format_cannot_hold_come_with_the_warning():
    source = terrio.Raster.from_array(
        np.full((16, 16), 300.5, "float32"), [0, 1, 0, 16, 0, -1], "EPSG:4326"
    )
    with pytest.warns(
        UserWarning, match="JPEG driver doesn't support data type Float32: written as Byte"
    ):
        assert source.to_bytes("out.jpg")
    result = terrio.pipeline("read image ! write out.png", {"image": source})
    with pytest.warns(
        UserWarning, match="PNG driver doesn't support data type Float32: written as UInt16"
    ):
        assert result.output_bytes()["out.png"]
