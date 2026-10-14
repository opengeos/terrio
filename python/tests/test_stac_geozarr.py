"""STAC items and mosaics, and GeoZarr stores, on small data made in the tests."""

import json

import numpy as np

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4400000.0, 0.0, -10.0)


def write_item(tmp_path, name, data, transform, datetime):
    """Writes a GeoTIFF and a STAC item describing it.

    Args:
        tmp_path: The directory to write to.
        name: The base name of the files.
        data: The pixels, `(rows, cols)`.
        transform: The GDAL geotransform.
        datetime: The item's datetime.

    Returns:
        dict: The item.
    """
    terrio.Raster.from_array(data, transform, crs="EPSG:32613", nodata=0).write(str(tmp_path / f"{name}.tif"))
    rows, cols = data.shape
    x0, y1 = transform[0], transform[3]
    return {
        "type": "Feature",
        "stac_version": "1.0.0",
        "stac_extensions": ["https://stac-extensions.github.io/projection/v1.1.0/schema.json"],
        "id": name,
        "properties": {"datetime": datetime, "proj:epsg": 32613},
        "geometry": {"type": "Point", "coordinates": [-105.0, 40.0]},
        "links": [],
        "assets": {
            "data": {
                "href": f"./{name}.tif",
                "type": "image/tiff; application=geotiff",
                "proj:shape": [rows, cols],
                "proj:transform": [10.0, 0.0, x0, 0.0, -10.0, y1],
            }
        },
    }


def test_stac_item_asset_and_mosaic(tmp_path):
    a = np.full((4, 6), 1, dtype="uint16")
    b = np.full((4, 6), 2, dtype="uint16")
    item_a = write_item(tmp_path, "a", a, TRANSFORM, "2023-07-01T00:00:00Z")
    item_b = write_item(tmp_path, "b", b, (500030.0, 10.0, 0.0, 4400000.0, 0.0, -10.0), "2023-07-02T00:00:00Z")
    item_path = tmp_path / "a.json"
    item_path.write_text(json.dumps(item_a))
    summary = terrio.info(str(item_path))
    assert [s["name"] for s in summary["subdatasets"]] == [f'STAC:"{item_path}":data']
    asset = terrio.open_raster(f'STAC:"{item_path}":data', lazy=True)
    assert (asset.width, asset.height) == (6, 4)
    np.testing.assert_array_equal(asset.read(1), a)

    items = tmp_path / "items.json"
    items.write_text(json.dumps({"type": "FeatureCollection", "features": [item_b, item_a]}))
    mosaic = terrio.open_raster(f'STACIT:"{items}":asset=data', lazy=True)
    assert (mosaic.width, mosaic.height) == (9, 4)
    row = mosaic.read(1)[0]
    np.testing.assert_array_equal(row, [1, 1, 1, 2, 2, 2, 2, 2, 2])
    footprints = terrio.open_vector(str(items))
    assert len(footprints.layers[0]) == 2
    assert "assets.data.href" in footprints.layers[0].fields


def test_geozarr_write_and_overviews(tmp_path):
    data = np.arange(600 * 300, dtype="float32").reshape(300, 600)
    r = terrio.Raster.from_array(data, (-110.0, 0.01, 0.0, 40.0, 0.0, -0.01), crs="EPSG:4326")
    path = str(tmp_path / "geo.zarr")
    r.write(path, options={"FORMAT": "ZARR_V3", "GEOREFERENCING_CONVENTION": "SPATIAL_PROJ", "OVERVIEW_LEVELS": "AUTO"})
    back = terrio.open_raster(path, lazy=True)
    assert back.overviews == [(300, 150), (150, 75)]
    np.testing.assert_array_equal(back.read(1), data)
    np.testing.assert_array_equal(back.read(1, overview=0), data[::2, ::2])
    attrs = json.loads((tmp_path / "geo.zarr" / "geo" / "zarr.json").read_text())["attributes"]
    assert attrs["proj:code"] == "EPSG:4326"
    assert attrs["spatial:transform"] == [0.01, 0.0, -110.0, 0.0, -0.01, 40.0]
