"""PixelIsPoint GeoTIFFs and COGs written through `terrio.rio` (rasterio not needed).

A GeoTIFF records whether a pixel value stands for the area of its cell (PixelIsArea) or for the
point at its centre (PixelIsPoint), in its key GTRasterTypeGeoKey (1025): 1 or 2. A PixelIsPoint
file's tie point is the centre of pixel (0, 0). terrio reads the key as the `AREA_OR_POINT` tag, and
its GeoTIFF writer writes `AREA_OR_POINT=Point` as RasterPixelIsPoint, given a CRS.

`terrio.rio` dropped the tag from what it gave the writer, so every copy, write or update of a
PixelIsPoint raster came out PixelIsArea. These tests read the key and the tie point from the file.
Without a CRS the key cannot be written: such a raster is still written PixelIsArea, now with a
warning.
"""

import warnings

import numpy as np
import pytest
from affine import Affine

import terrio
import terrio.rio as rio
from terrio.rio import _tiff
from terrio.rio import shutil as rio_shutil
from terrio.rio.control import GroundControlPoint
from terrio.rio.transform import from_origin

T = from_origin(500000, 4100000, 30, 30)
PIXELS = (np.arange(3 * 40 * 50) % 251).astype("uint8").reshape(3, 40, 50)
PROFILE = dict(width=50, height=40, count=3, dtype="uint8", crs="EPSG:32611", transform=T)
# The centre and the corner of pixel (0, 0).
CENTRE = (500015.0, 4099985.0)
CORNER = (500000.0, 4100000.0)
# The corner of pixel (0, 0) in the raster space of a PixelIsPoint file, where (0, 0) is the
# centre of that pixel: the raster position of a GCP at row 0, column 0, as its tie point.
CORNER_AS_POINT = [-0.5, -0.5]
MASK = np.full((40, 50), 255, np.uint8)
MASK[:5] = 0
# The warning given when a PixelIsPoint raster has no CRS.
NOT_KEPT = "AREA_OR_POINT=Point is not kept: a GeoTIFF's raster type needs a CRS"


def _raster_type(path):
    """The file's GTRasterTypeGeoKey, and the model coordinates of its tie point."""
    tags = terrio._terrio.tiff_tags(str(path))[0]
    return _tiff._geokey(tags, 1025), tuple(tags[33922][3:5])


def _assert_pixel_is_point(path):
    with rio.open(path) as ds:
        assert ds.tags()["AREA_OR_POINT"] == "Point"
        assert ds.transform == T
        assert np.array_equal(ds.read(), PIXELS)
    assert _raster_type(path) == (2, CENTRE)


def _assert_written_without_a_raster_type(path):
    """Written without a CRS: no GeoKeys, so no raster-type key (a GeoTIFF without one is
    PixelIsArea) and no `AREA_OR_POINT` tag read back, the tie point the corner of pixel (0, 0)."""
    with rio.open(path) as ds:
        assert "AREA_OR_POINT" not in ds.tags()
        assert ds.crs is None and ds.transform == T
        assert np.array_equal(ds.read(), PIXELS)
    assert _raster_type(path) == (None, CORNER)


@pytest.fixture
def point_tif(tmp_path):
    """A PixelIsPoint GeoTIFF written by terrio's core API."""
    path = tmp_path / "point.tif"
    terrio.Raster.from_array(
        PIXELS, T.to_gdal(), crs="EPSG:32611", metadata={"AREA_OR_POINT": "Point"}
    ).write(str(path))
    assert _raster_type(path) == (2, CENTRE)
    return path


def _write(path, driver, mask=False, **profile):
    with rio.open(path, "w", driver=driver, **{**PROFILE, **profile}) as dst:
        dst.update_tags(AREA_OR_POINT="Point")
        dst.write(PIXELS)
        if mask:
            dst.write_mask(MASK)


# Each way the writer writes a file: whole (`Raster.write`: a GeoTIFF that fits in memory, a COG
# with a mask) or in rows (a GeoTIFF above `TERRIO_RIO_IN_MEMORY_LIMIT`, a COG without a mask);
# and a driver name in lower case, which the writers accept.
@pytest.mark.parametrize(
    "driver, mask, limit",
    [
        ("GTiff", False, None),
        ("GTiff", False, "1000"),
        ("COG", True, None),
        ("COG", False, None),
        ("gtiff", False, None),
    ],
    ids=["gtiff-whole", "gtiff-rows", "cog-whole", "cog-rows", "gtiff-lower-case"],
)
def test_update_tags_area_or_point_point_writes_pixel_is_point(
    tmp_path, monkeypatch, driver, mask, limit
):
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    out = tmp_path / "point.tif"
    _write(out, driver, mask=mask)
    _assert_pixel_is_point(out)
    if mask:
        with rio.open(out) as ds:
            assert np.array_equal(ds.dataset_mask(), MASK)


# A copy to GTiff is written whole, or in rows above the limit; a copy to COG is written in rows.
@pytest.mark.parametrize("limit", [None, "1000"], ids=["in-memory", "above-memory-limit"])
@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_shutil_copy_keeps_pixel_is_point(tmp_path, monkeypatch, point_tif, driver, limit):
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    out = tmp_path / "copy.tif"
    rio_shutil.copy(str(point_tif), str(out), driver=driver)
    _assert_pixel_is_point(out)


def test_an_update_keeps_pixel_is_point(tmp_path, point_tif):
    with rio.open(point_tif, "r+") as ds:
        ds.write(PIXELS[:, :4, :4], window=((0, 4), (0, 4)))
    _assert_pixel_is_point(point_tif)


@pytest.mark.parametrize(
    "driver, mask, limit",
    [("GTiff", False, None), ("GTiff", False, "1000"), ("COG", True, None), ("COG", False, None)],
    ids=["gtiff-whole", "gtiff-rows", "cog-whole", "cog-rows"],
)
def test_pixel_is_point_with_a_crs_and_the_identity_transform_keeps_both(
    tmp_path, monkeypatch, driver, mask, limit
):
    """A raster with a CRS and no georeferencing (the identity transform) is written
    PixelIsPoint too: the raster-type key beside the CRS's keys, and no tie point, pixel scale or
    transformation matrix. It reads back with its tag and the identity transform."""
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    out = tmp_path / "point.tif"
    with warnings.catch_warnings(record=True) as record:
        warnings.simplefilter("always")
        _write(out, driver, mask=mask, transform=Affine.identity())
    assert not [w for w in record if NOT_KEPT in str(w.message)]
    with rio.open(out) as ds:
        assert ds.tags()["AREA_OR_POINT"] == "Point"
        assert ds.transform == Affine.identity() and ds.crs.to_epsg() == 32611
        assert np.array_equal(ds.read(), PIXELS)
    tags = terrio._terrio.tiff_tags(str(out))[0]
    assert _tiff._geokey(tags, 1025) == 2
    assert not {33550, 33922, 34264} & set(tags)


def test_area_stays_pixel_is_area(tmp_path):
    """A new GeoTIFF is PixelIsArea, as is one given `AREA_OR_POINT=Area`: its tie point the
    corner of pixel (0, 0)."""
    for i, tags in enumerate([{}, {"AREA_OR_POINT": "Area"}]):
        out = tmp_path / f"area{i}.tif"
        with rio.open(out, "w", driver="GTiff", **PROFILE) as dst:
            dst.update_tags(**tags)
            dst.write(PIXELS)
        with rio.open(out) as ds:
            assert ds.tags()["AREA_OR_POINT"] == "Area" and ds.transform == T
        assert _raster_type(out) == (1, CORNER)


@pytest.mark.parametrize(
    "driver, limit",
    [("GTiff", None), ("GTiff", "1000"), ("COG", None)],
    ids=["gtiff-whole", "gtiff-rows", "cog"],
)
def test_pixel_is_point_without_a_crs_is_written_pixel_is_area_with_one_warning(
    tmp_path, monkeypatch, driver, limit
):
    """Without a CRS, a raster-type key alone would stand for a local CRS, so the raster is
    written PixelIsArea, as before; one warning now says that the raster type is not kept, and
    why."""
    if limit:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    out = tmp_path / "nocrs.tif"
    with pytest.warns(UserWarning, match=NOT_KEPT) as record:
        _write(out, driver, crs=None)
    assert len([w for w in record if NOT_KEPT in str(w.message)]) == 1
    _assert_written_without_a_raster_type(out)


def test_a_copy_of_pixel_is_point_without_a_crs_is_written_pixel_is_area_with_one_warning(
    tmp_path,
):
    """A source without a CRS (a VRT with `AREA_OR_POINT=Point` among its metadata) is copied
    as before, PixelIsArea, with one warning."""
    base = tmp_path / "base.tif"
    with rio.open(base, "w", driver="GTiff", **{**PROFILE, "crs": None}) as dst:
        dst.write(PIXELS)
    vrt = tmp_path / "point.vrt"
    bands = "".join(
        f'<VRTRasterBand dataType="Byte" band="{b}"><SimpleSource>'
        f'<SourceFilename relativeToVRT="1">base.tif</SourceFilename>'
        f"<SourceBand>{b}</SourceBand></SimpleSource></VRTRasterBand>"
        for b in (1, 2, 3)
    )
    vrt.write_text(
        '<VRTDataset rasterXSize="50" rasterYSize="40">'
        f"<GeoTransform>{', '.join(map(str, T.to_gdal()))}</GeoTransform>"
        '<Metadata><MDI key="AREA_OR_POINT">Point</MDI></Metadata>'
        f"{bands}</VRTDataset>"
    )
    with rio.open(vrt) as src:
        assert src.tags()["AREA_OR_POINT"] == "Point" and src.crs is None
    out = tmp_path / "copy.tif"
    with pytest.warns(UserWarning, match=NOT_KEPT) as record:
        rio_shutil.copy(str(vrt), str(out), driver="GTiff")
    assert len([w for w in record if NOT_KEPT in str(w.message)]) == 1
    _assert_written_without_a_raster_type(out)


def test_gcps_with_a_crs_keep_pixel_is_point_without_a_warning(tmp_path):
    """The CRS of a raster's GCPs is its CRS for the raster type: the GCPs are written as tie
    points of pixel centres, and read back as given."""
    gcps = [
        GroundControlPoint(row, col, 500000 + 30 * col, 4100000 - 30 * row)
        for row, col in ((0, 0), (0, 50), (40, 0), (40, 50))
    ]
    out = tmp_path / "gcps.tif"
    with warnings.catch_warnings(record=True) as record:
        warnings.simplefilter("always")
        with rio.open(
            out, "w", driver="GTiff", **{**PROFILE, "crs": None, "transform": None}
        ) as dst:
            dst.gcps = (gcps, "EPSG:32611")
            dst.update_tags(AREA_OR_POINT="Point")
            dst.write(PIXELS)
    assert not [w for w in record if NOT_KEPT in str(w.message)]
    with rio.open(out) as ds:
        assert ds.tags()["AREA_OR_POINT"] == "Point"
        read, crs = ds.gcps
        assert crs.to_epsg() == 32611
        assert [(g.row, g.col, g.x, g.y) for g in read] == [(g.row, g.col, g.x, g.y) for g in gcps]
    tags = terrio._terrio.tiff_tags(str(out))[0]
    assert _tiff._geokey(tags, 1025) == 2
    assert tags[33922][:2] == CORNER_AS_POINT
