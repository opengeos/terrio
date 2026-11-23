"""Area-matched COG overviews, read back by GDAL (rasterio).

terrio's `area_matched` overviews have no GDAL equivalent, so GDAL is the reader here: the levels
GDAL reads must have the sizes of GDAL's own COG overviews, keep the valid area exactly (to the
nearest cell) where GDAL's `average` grows it, and carry the band metadata items that record it,
which GDAL's COG driver copies when it converts the file again. Skipped without rasterio.

Tolerance: none on counts, sizes and items; the recomputed ratios equal the stored ones to 1e-12.
"""

import numpy as np
import pytest

import terrio

pytest.importorskip("rasterio")
import rasterio  # noqa: E402
import rasterio.shutil  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

ND = -9999.0


def _meander(width=1201, height=853):
    """A thin, ragged band of valid Float32 depths about 9 pixels wide, nodata elsewhere."""
    x = np.arange(width, dtype="float64")
    centre = height / 2 + 0.3 * height * np.sin(x / 97) + 9 * np.sin(x / 7)
    y = np.arange(height, dtype="float64")[:, None]
    d = np.abs(y - centre)
    return np.where(d < 4.5, 4.5 - d, ND).astype("float32")


def _write(path, data, method, **options):
    raster = terrio.Raster.from_array(
        data, from_origin(400000, 4500000, 10, 10).to_gdal(), crs="EPSG:32613", nodata=ND
    )
    raster.write(
        str(path), driver="COG", compress="DEFLATE", overview_resampling=method, options=options
    )


def _levels(path):
    """The full-resolution band and every overview of a file, as GDAL reads them."""
    with rasterio.open(path) as src:
        base = src.read(1)
        sizes = [
            (int(np.ceil(src.width / f)), int(np.ceil(src.height / f))) for f in src.overviews(1)
        ]
        tags = src.tags(1)
        structure = src.tags(ns="IMAGE_STRUCTURE")
    levels = []
    for i in range(len(sizes)):
        with rasterio.open(path, overview_level=i) as ovr:
            levels.append(ovr.read(1))
    return base, levels, tags, structure


def test_area_matched_levels_keep_the_valid_area(tmp_path):
    data = _meander()
    _write(tmp_path / "am.tif", data, "area_matched")
    _write(tmp_path / "avg.tif", data, "average")
    base, levels, tags, structure = _levels(tmp_path / "am.tif")
    _, avg_levels, avg_tags, _ = _levels(tmp_path / "avg.tif")
    assert structure["LAYOUT"] == "COG"
    assert structure["OVERVIEW_RESAMPLING"] == "AREA_MATCHED"
    np.testing.assert_array_equal(base, data)
    assert [lv.shape for lv in levels] == [lv.shape for lv in avg_levels]
    assert len(levels) == 2
    valid = int((data != ND).sum())
    ratios = []
    for am, avg in zip(levels, avg_levels, strict=True):
        cell = data.size / am.size
        kept = int((am != ND).sum())
        assert kept == round(valid * am.size / data.size)
        assert (avg != ND).sum() * cell > 1.2 * valid
        ratios.append(kept * cell / valid)
    stored = [float(r) for r in tags["OVERVIEW_AREA_RATIOS"].split(",")]
    np.testing.assert_allclose(stored, ratios, rtol=0, atol=1e-12)
    assert tags["OVERVIEW_BASE_VALID_PIXELS"] == str(valid)
    assert "OVERVIEW_AREA_RATIOS" not in avg_tags
    # terrio reads the same items.
    items = terrio.open_raster(str(tmp_path / "am.tif")).band_metadata[0]["items"]
    assert items["OVERVIEW_AREA_RATIOS"] == tags["OVERVIEW_AREA_RATIOS"]
    assert terrio.validate_cog(str(tmp_path / "am.tif"))[0]


def test_gdal_keeps_the_items_when_it_converts_the_cog(tmp_path):
    _write(tmp_path / "am.tif", _meander(), "area_matched")
    rasterio.shutil.copy(tmp_path / "am.tif", tmp_path / "gdal.tif", driver="COG")
    with rasterio.open(tmp_path / "am.tif") as a, rasterio.open(tmp_path / "gdal.tif") as b:
        assert b.tags(1)["OVERVIEW_AREA_RATIOS"] == a.tags(1)["OVERVIEW_AREA_RATIOS"]
        assert b.tags(1)["OVERVIEW_BASE_VALID_PIXELS"] == a.tags(1)["OVERVIEW_BASE_VALID_PIXELS"]


def test_area_matched_mode_keeps_classes(tmp_path):
    rng = np.random.default_rng(7)
    classes = rng.choice(
        np.array([0, 3, 7, 11], dtype="uint8"), size=(700, 900), p=[0.7, 0.1, 0.1, 0.1]
    )
    raster = terrio.Raster.from_array(classes, from_origin(0, 700, 1, 1).to_gdal(), nodata=0)
    raster.write(
        str(tmp_path / "c.tif"), driver="COG", options={"OVERVIEW_RESAMPLING": "AREA_MATCHED_MODE"}
    )
    base, levels, tags, structure = _levels(tmp_path / "c.tif")
    assert structure["OVERVIEW_RESAMPLING"] == "AREA_MATCHED_MODE"
    valid = int((classes != 0).sum())
    for level in levels:
        assert set(np.unique(level)) <= {0, 3, 7, 11}
        assert int((level != 0).sum()) == round(valid * level.size / classes.size)
    assert "OVERVIEW_AREA_RATIOS" in tags


def test_area_matched_needs_nodata_and_the_whole_raster(tmp_path):
    raster = terrio.Raster.from_array(np.ones((600, 600), dtype="float32"), (0, 1, 0, 600, 0, -1))
    with pytest.raises(terrio.TerrioError, match="nodata"):
        raster.write(str(tmp_path / "x.tif"), driver="COG", overview_resampling="area_matched")
    with pytest.raises(terrio.TerrioError, match="whole raster"):
        terrio._terrio.CogWriter(
            str(tmp_path / "rows.tif"),
            600,
            600,
            1,
            "float32",
            (0, 1, 0, 600, 0, -1),
            nodata=ND,
            overview_resampling="area_matched",
        )
