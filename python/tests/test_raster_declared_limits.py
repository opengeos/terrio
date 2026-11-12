"""Configured declaration budgets must apply before raster pixels are decoded."""

import numpy as np
import pytest

import terrio
import terrio.rio


@pytest.mark.parametrize("route", ["converted-output", "fallback", "boundless"])
def test_read_buffers_are_refused_before_allocation(tmp_path, monkeypatch, route):
    """Check memory admission before Python read-buffer allocation.

    Args:
        tmp_path: Temporary directory fixture.
        monkeypatch: Scoped monkeypatch fixture.
        route: Output allocation path to exercise.
    """
    from terrio.rio.enums import Resampling
    from terrio.rio.windows import Window

    path = tmp_path / "small.tif"
    terrio.Raster.from_array(np.ones((1, 1), dtype="uint8"), (0, 1, 0, 1, 0, -1)).write(str(path))
    with terrio.rio.open(path) as source:
        monkeypatch.setattr(terrio._terrio, "usable_memory", lambda: 1 << 20)
        with pytest.raises(terrio.ResourceLimitError):
            if route == "converted-output":
                source.read(out_shape=(2048, 2048), out_dtype="float64")
            elif route == "fallback":
                source._io([1], None, (4097, 4096), Resampling.nearest)
            else:
                source._boundless(
                    [1], Window(-1, -1, 4096, 4097), (4097, 4096), Resampling.nearest, None
                )


@pytest.fixture(params=[("GTiff", "tif"), ("PNG", "png"), ("ENVI", "dat"), ("EHdr", "bil")])
def declared_raster(request, tmp_path, gdal):
    """Create independent UInt16 RGB controls with 3,072 declared sample bytes."""
    driver, extension = request.param
    source = gdal.GetDriverByName("MEM").Create("", 32, 16, 3, gdal.GDT_UInt16)
    for band in range(1, 4):
        source.GetRasterBand(band).WriteArray(np.full((16, 32), band * 17, dtype=np.uint16))
    path = tmp_path / f"control.{extension}"
    output = gdal.GetDriverByName(driver).CreateCopy(str(path), source)
    assert output is not None
    output = None
    return path


def test_declared_budget_is_off_by_default(declared_raster, monkeypatch):
    """An unset policy preserves ordinary reads and their original sample precision."""
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS"):
        monkeypatch.delenv(option, raising=False)
    values = terrio.open_raster(str(declared_raster), lazy=True).read()
    assert values.dtype == np.uint16
    assert values.shape == (3, 16, 32)
    for band in range(3):
        np.testing.assert_array_equal(values[band], (band + 1) * 17)


@pytest.mark.parametrize(
    "option,value", [("TERRIO_MAX_DECLARED_BYTES", "3071"), ("TERRIO_MAX_BANDS", "2")]
)
@pytest.mark.parametrize("surface", ["lazy", "whole", "generic", "rio"])
def test_oversized_declaration_is_refused_at_open(
    declared_raster, monkeypatch, option, value, surface
):
    """Refuse the header at open, including lazy APIs that have not requested pixels."""
    monkeypatch.setenv(option, value)
    with pytest.raises(terrio.ResourceLimitError, match=option):
        if surface == "rio":
            with terrio.rio.open(declared_raster):
                pass
        elif surface == "generic":
            terrio.open(str(declared_raster))
        else:
            terrio.open_raster(str(declared_raster), lazy=surface == "lazy")


@pytest.mark.parametrize(
    "option,value", [("TERRIO_MAX_DECLARED_BYTES", "3071"), ("TERRIO_MAX_BANDS", "2")]
)
def test_conversion_refuses_source_before_creating_output(
    declared_raster, tmp_path, monkeypatch, option, value
):
    """A rejected declaration must not start writing a conversion result."""
    output = tmp_path / "refused.tif"
    monkeypatch.setenv(option, value)
    with pytest.raises(terrio.ResourceLimitError, match=option):
        terrio.convert(str(declared_raster), str(output))
    assert not output.exists()


@pytest.mark.parametrize("surface", ["lazy", "whole", "generic", "rio"])
def test_exact_declaration_limits_allow_the_original_samples(declared_raster, surface):
    """Scoped limits admit their exact boundary and restore the prior configuration."""
    with terrio.rio.Env(TERRIO_MAX_DECLARED_BYTES="3072", TERRIO_MAX_BANDS="3"):
        if surface == "rio":
            with terrio.rio.open(declared_raster) as dataset:
                values = dataset.read()
        elif surface == "generic":
            values = terrio.open(str(declared_raster)).read()
        else:
            values = terrio.open_raster(str(declared_raster), lazy=surface == "lazy").read()
    assert values.dtype == np.uint16
    assert values.shape == (3, 16, 32)
    for band in range(3):
        np.testing.assert_array_equal(values[band], (band + 1) * 17)
