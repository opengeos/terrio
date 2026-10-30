# Written to match the behavior of:
#   rasterio 1.5.2:
#     rasterio/
# No code is translated from these sources.

"""Raster file management, as `rasterio.shutil`.

Whether a dataset exists, copying a dataset to another format or its files to another place, and
deleting a dataset's files.

A dataset's files are its main file and the sidecars GDAL keeps beside it (`.aux.xml`, `.ovr`,
`.msk`, world files, RPC files), as `DatasetBase.files` lists them.
"""

from __future__ import annotations

import os
import shutil as _shutil
from typing import Any

from terrio.rio.errors import DriverRegistrationError, RasterioIOError


def _fspath(path: Any) -> str:
    """A path as a string.

    Args:
        path: A string or path-like object.

    Returns:
        str: The path.
    """
    return os.fspath(path) if isinstance(path, os.PathLike) else str(path)


def _same(a: str, b: str) -> bool:
    """Whether two paths name the same dataset.

    Args:
        a: A path.
        b: Another path.

    Returns:
        bool: True when they are the same file.
    """
    if "://" in a or "://" in b:
        return a == b
    return os.path.abspath(a) == os.path.abspath(b)


def exists(path: Any) -> bool:
    """Whether a dataset exists, by trying to open it.

    Args:
        path: A path or URL.

    Returns:
        bool: True when terrio opens it.
    """
    from terrio.rio import open as rio_open

    try:
        with rio_open(path):
            return True
    except Exception:
        return False


def copy(
    src: Any, dst: Any, driver: str | None = None, strict: bool = True, **creation_options: Any
) -> None:
    """Copies a raster to a new file, with GDAL's `CreateCopy` semantics.

    The copy keeps the pixels, georeferencing, nodata values, tags, band descriptions, color
    interpretation and color table, and the dataset mask. It is written window by window.

    Args:
        src: A path, or a dataset opened for reading.
        dst: The output path.
        driver: The output format, by default from the extension (GeoTIFF otherwise).
        strict: Accepted for compatibility (terrio fails rather than adapting the copy).
        **creation_options: Creation options, such as `compress="deflate"`, `tiled=True`.

    Raises:
        RasterioIOError: If `src` and `dst` are the same dataset, or the source cannot be read.
        DriverRegistrationError: If the driver is not one terrio writes.
    """
    from terrio.rio import open as rio_open
    from terrio.rio.io import DatasetReader, driver_from_extension, get_writer_for_driver

    dst_path = _fspath(dst)
    if driver is None:
        try:
            driver = driver_from_extension(dst_path)
        except ValueError:
            driver = "GTiff"
    try:
        get_writer_for_driver(driver)
    except Exception as err:
        raise DriverRegistrationError(f"Unrecognized driver: {driver}") from err
    if isinstance(src, DatasetReader):
        if _same(src.name, dst_path):
            raise RasterioIOError(f"{src.name} and {dst_path} identify the same dataset.")
        _copy_dataset(src, dst_path, driver, creation_options)
        return
    src_path = _fspath(src)
    if _same(src_path, dst_path):
        raise RasterioIOError(f"{src_path} and {dst_path} identify the same dataset.")
    with rio_open(src_path) as ds:
        _copy_dataset(ds, dst_path, driver, creation_options)


def _copy_dataset(src: Any, dst_path: str, driver: str, creation_options: dict[str, Any]) -> None:
    """Writes an open dataset to a new file, block row by block row.

    Args:
        src: The dataset.
        dst_path: The output path.
        driver: The output format.
        creation_options: Creation options.
    """
    from terrio.rio import open as rio_open
    from terrio.rio.windows import Window

    profile = dict(src.profile)
    for k in ("blockxsize", "blockysize", "tiled", "compress", "interleave", "photometric"):
        profile.pop(k, None)
    profile["driver"] = driver
    profile.update({k.lower(): v for k, v in creation_options.items()})
    if driver.upper() in ("GTIFF", "COG") and src.count > 1 and "interleave" not in profile:
        # GDALDriver::CreateCopy (gcore/gdaldriver.cpp): without INTERLEAVE, and without a
        # COMPRESS other than NONE, the copy keeps its source's pixel or band interleaving;
        # otherwise it is the drivers' default, PIXEL.
        src_interleave = src.tags(ns="IMAGE_STRUCTURE").get("INTERLEAVE", "").upper()
        compress = profile.get("compress", "NONE")
        keep = str(getattr(compress, "value", compress)).upper() == "NONE"
        profile["interleave"] = (
            src_interleave.lower() if keep and src_interleave in ("PIXEL", "BAND") else "pixel"
        )
    if driver.upper() == "COG":
        # GDAL's COG driver has no PHOTOMETRIC creation option and ignores the one a source's
        # profile carries (YCbCr, say): GTiff's CreateCopy derives it from the source's color
        # interpretation instead, RGB for red, green and blue.
        profile.pop("photometric", None)
        if [c.name for c in src.colorinterp[:3]] == ["red", "green", "blue"]:
            profile["photometric"] = "rgb"
        # A source with overviews of its own: GDAL's COG driver copies them (OVERVIEWS=AUTO).
        if (
            str(profile.get("overviews", "AUTO")).upper() in ("AUTO", "FORCE_USE_EXISTING")
            and src.driver in ("GTiff", "COG")
            and src.overviews(1)
        ):
            profile["@copy_src_overviews"] = src.name
    gcps, gcp_crs = src.gcps
    with rio_open(dst_path, "w", **profile) as dst:
        dst._mask_has_dataset = True
        dst.update_tags(**src.tags())
        for b in src.indexes:
            band_tags = src.tags(b)
            if band_tags:
                dst.update_tags(b, **band_tags)
        if any(src.descriptions):
            dst.descriptions = src.descriptions
        if any(u for u in src.units):
            dst.units = src.units
        if any(s != 1.0 for s in src.scales):
            dst.scales = src.scales
        if any(o != 0.0 for o in src.offsets):
            dst.offsets = src.offsets
        dst.colorinterp = src.colorinterp
        if gcps and not src._georeferenced:
            dst.gcps = (gcps, gcp_crs)
        try:
            cmap = src.colormap(1)
        except ValueError:
            cmap = None
        if cmap:
            dst.write_colormap(1, cmap)
        per_dataset_mask = all(
            "per_dataset" in [f.name for f in flags] for flags in src.mask_flag_enums
        )
        rows = max(1, min(src.height, (64 << 20) // max(1, src.width * src.count * 8)))
        for y in range(0, src.height, rows):
            w = Window(0, y, src.width, min(rows, src.height - y))
            dst.write(src.read(window=w), window=w)
            if per_dataset_mask:
                dst.write_mask(src.dataset_mask(window=w), window=w)


def copyfiles(src: Any, dst: Any) -> None:
    """Copies the files of a dataset to another name: the main file and its sidecars.

    Args:
        src: The dataset's path.
        dst: The new path of the main file; the sidecars take its name.

    Raises:
        RasterioIOError: If `src` and `dst` are the same dataset, or `src` cannot be opened.
    """
    from terrio.rio import open as rio_open

    src_path, dst_path = _fspath(src), _fspath(dst)
    if _same(src_path, dst_path):
        raise RasterioIOError(f"{src_path} and {dst_path} identify the same dataset.")
    try:
        with rio_open(src_path) as ds:
            files = ds.files
    except RasterioIOError:
        raise
    except Exception as err:
        raise RasterioIOError(str(err)) from err
    base = os.path.basename(src_path)
    for f in files:
        name = os.path.basename(f)
        if name == base:
            target = dst_path
        elif name.startswith(base):
            target = dst_path + name[len(base) :]
        else:
            stem = os.path.splitext(base)[0]
            target = os.path.join(
                os.path.dirname(dst_path),
                os.path.splitext(os.path.basename(dst_path))[0] + name[len(stem) :],
            )
        _shutil.copyfile(f, target)


def delete(path: Any, driver: str | None = None) -> None:
    """Deletes a dataset's files.

    Args:
        path: The dataset's path.
        driver: Accepted for compatibility.

    Raises:
        RasterioIOError: If the dataset does not exist.
    """
    from terrio.rio import open as rio_open

    p = _fspath(path)
    try:
        with rio_open(p) as ds:
            files = ds.files
    except Exception as err:
        raise RasterioIOError(f"Invalid dataset: {p}") from err
    for f in files:
        if os.path.isdir(f):
            _shutil.rmtree(f)
        elif os.path.exists(f):
            os.remove(f)


__all__ = ["copy", "copyfiles", "delete", "exists"]
