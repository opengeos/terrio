"""terrio.validate_cog, with rio-cogeo's cog_validate as the oracle when it is installed."""

import numpy as np
import pytest

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)


def raster(width, height):
    a = (np.arange(width * height, dtype="float32").reshape(height, width) % 1000).astype("float32")
    return terrio.Raster.from_array(a, TRANSFORM, "EPSG:32610", -9999.0)


@pytest.fixture
def files(tmp_path):
    r = raster(2100, 1500)
    out = {}
    out["cog"] = tmp_path / "cog.tif"
    r.write(str(out["cog"]), driver="COG", compress="DEFLATE")
    out["cog_small"] = tmp_path / "small.tif"
    raster(300, 200).write(str(out["cog_small"]), driver="COG")
    out["strips"] = tmp_path / "strips.tif"
    r.write(str(out["strips"]), driver="GTiff")
    out["tiled_overviews"] = tmp_path / "tiled_ovr.tif"
    r.write(str(out["tiled_overviews"]), driver="GTiff", tile_size=256, overviews=[2, 4])
    out["strip_overviews"] = tmp_path / "strip_ovr.tif"
    r.write(str(out["strip_overviews"]), driver="GTiff", overviews=[2, 4])
    out["not_tiff"] = tmp_path / "x.tif"
    out["not_tiff"].write_bytes(b"definitely not a TIFF")
    return out


def test_verdicts(files):
    assert terrio.validate_cog(str(files["cog"])) == (True, [], [])
    assert terrio.validate_cog(str(files["cog_small"])) == (True, [], [])
    ok, errors, warnings = terrio.validate_cog(str(files["strips"]))
    assert not ok
    assert "The file is greater than 512xH or 512xW, but is not tiled" in errors
    assert warnings == [
        "The file is greater than 512xH or 512xW, it is recommended to include internal overviews"
    ]
    assert not terrio.validate_cog(str(files["tiled_overviews"]))[0]
    ok, errors, _ = terrio.validate_cog(str(files["strip_overviews"]))
    assert "Overview of index 0 is not tiled" in errors
    assert terrio.validate_cog(str(files["not_tiff"])) == (False, ["The file is not a GeoTIFF"], [])
    # Warnings fail strict validation.
    assert terrio.validate_cog(str(files["strips"]), strict=True)[0] is False
    with pytest.raises(terrio.TerrioError):
        terrio.validate_cog(str(files["cog"]) + ".missing")


def test_accepts_path_like(files):
    assert terrio.validate_cog(files["cog"]) == (True, [], [])
    with pytest.raises(ValueError):
        terrio.validate_cog(raster(10, 10))  # type: ignore[arg-type]


def test_agrees_with_rio_cogeo(files):
    cogeo = pytest.importorskip("rio_cogeo.cogeo")
    for name, path in files.items():
        if name == "not_tiff":
            continue
        for strict in (False, True):
            expected = cogeo.cog_validate(str(path), strict=strict, quiet=True)
            got = terrio.validate_cog(str(path), strict=strict)
            assert got == (expected[0], list(expected[1]), list(expected[2])), name


def test_non_square_blocks_agree_with_rio_cogeo(tmp_path):
    # rio-cogeo scans the first block with the block width and height swapped (rasterio's block
    # shapes are (rows, cols)); with non-square blocks this changes which blocks are scanned.
    rasterio = pytest.importorskip("rasterio")
    cogeo = pytest.importorskip("rio_cogeo.cogeo")
    from rasterio.enums import Resampling
    from rasterio.transform import from_origin
    from rasterio.windows import Window

    width, height = 2100, 1500
    a = (np.arange(width * height, dtype="float32").reshape(height, width) % 1000).astype("float32")
    paths = []
    for name, bx, by, overviews, sparse in [
        ("tiled_512x256.tif", 512, 256, None, False),
        ("tiled_256x512.tif", 256, 512, None, False),
        ("tiled_512x256_overviews.tif", 512, 256, [2, 4], False),
        ("tiled_256x512_overviews.tif", 256, 512, [2, 4], False),
        # Only the bottom block rows stored: outside the main image's scanned region.
        ("sparse_512x256.tif", 512, 256, None, True),
        ("sparse_256x512.tif", 256, 512, None, True),
    ]:
        path = tmp_path / name
        profile = dict(
            driver="GTiff",
            width=width,
            height=height,
            count=1,
            dtype="float32",
            crs="EPSG:32610",
            transform=from_origin(500000, 4000000, 10, 10),
            tiled=True,
            blockxsize=bx,
            blockysize=by,
        )
        if sparse:
            profile.update(sparse_ok=True, nodata=0)
        with rasterio.open(path, "w", **profile) as dst:
            if sparse:
                dst.write(a[1024:], 1, window=Window(0, 1024, width, height - 1024))
            else:
                dst.write(a, 1)
            if overviews:
                dst.build_overviews(overviews, Resampling.average)
        paths.append(path)
    for path in paths:
        for strict in (False, True):
            expected = cogeo.cog_validate(str(path), strict=strict, quiet=True)
            got = terrio.validate_cog(path, strict=strict)
            assert got == (expected[0], list(expected[1]), list(expected[2])), path.name
