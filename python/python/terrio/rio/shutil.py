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

import contextlib
import os
import shutil as _shutil
from collections.abc import Iterator
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
    if os.path.abspath(a) == os.path.abspath(b):
        return True
    try:
        return os.path.samefile(a, b)
    except OSError:
        # A new destination has no file identity yet.
        return False


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
    interpretation and color table, and the dataset mask. A COG is written a row of its tiles at
    a time from the source, its overviews computed as the rows come, so that the source's pixels
    are not held. Taller source blocks are read in bounded slabs, then written one tile row
    at a time. A source with a dataset mask, overviews the COG copies, or compression written
    whole (JPEG, WebP, LERC, JPEG XL) is held until it is written. A GeoTIFF larger than the
    in-memory limit (`TERRIO_RIO_IN_MEMORY_LIMIT`) is also copied directly from its source,
    in rows of blocks, without a dataset mask; a smaller one is held and written whole. A copy
    whose source fails to read leaves its destination as it was: an existing file keeps its
    content, and no file is left where there was none.

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
    from terrio.rio.io import DatasetReader, _canonical_writer_driver, driver_from_extension

    dst_path = _fspath(dst)
    if driver is None:
        try:
            driver = driver_from_extension(dst_path)
        except ValueError:
            driver = "GTiff"
    try:
        driver = _canonical_writer_driver(driver)
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


@contextlib.contextmanager
def _unwritten_on_error(dst: Any) -> Iterator[None]:
    """Leaves a copy's dataset unwritten when the copy raises.

    When its source fails to read, or it is interrupted, the copy's dataset is not written with
    the rows read until then and the rest the nodata value or 0: the destination stays as it was,
    or absent.

    Args:
        dst: The dataset the copy writes.

    Yields:
        None, for the copy.
    """
    try:
        yield
    except BaseException:
        dst._dirty = False
        raise


def _copy_dataset(
    src: Any,
    dst_path: str,
    driver: str,
    creation_options: dict[str, Any],
    from_source: bool = True,
) -> None:
    """Writes an open dataset to a new file.

    A file written in rows (`DatasetWriter._streams_on_flush`) is written from the source as it is
    written; another is written block row by block row into the dataset, written when it closes,
    unless the copy raised (`_unwritten_on_error`).

    Args:
        src: The dataset.
        dst_path: The output path.
        driver: The output format.
        creation_options: Creation options.
        from_source: Whether a file written in rows is written from the source; otherwise the
            copy holds the source's pixels until it is written, whatever its format.
    """
    from terrio import _terrio
    from terrio.rio import open as rio_open
    from terrio.rio.windows import Window

    profile = dict(src.profile)
    for k in ("blockxsize", "blockysize", "tiled", "compress", "interleave", "photometric"):
        profile.pop(k, None)
    profile["driver"] = driver
    sample = getattr(src, "_complex_sample", None)
    if sample in ("CInt16", "CInt32") and not any(k.lower() == "dtype" for k in creation_options):
        profile["dtype"] = "complex64" if sample == "CInt16" else "complex128"
        profile["_complex_dtype"] = sample
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
    per_dataset_mask = all(
        "per_dataset" in [f.name for f in flags] for flags in src.mask_flag_enums
    )
    if from_source and not per_dataset_mask:
        # When the copy is written in rows (a COG without the source's overviews to copy, or a
        # GeoTIFF larger than the in-memory limit), it reads them from the source as it writes
        # them, rather than holding the source's pixels until it is closed.
        profile["_copy_of"] = src
    with rio_open(dst_path, "w", **profile) as dst, _unwritten_on_error(dst):
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
        if (
            _terrio.canonical_driver_name(src.driver) == "JP2"
            and _terrio.canonical_driver_name(driver) == "JP2"
        ):
            # Two Gray roles do not identify which stored component supplies luminance.
            dst._jp2_associations = (
                [m.get("color_association") for m in src._raster.band_metadata]
                if src._raster is not None
                else list(getattr(src, "_jp2_associations", []))
            )
        if gcps and not src._georeferenced:
            dst.gcps = (gcps, gcp_crs)
        try:
            cmap = src.colormap(1)
        except ValueError:
            cmap = None
        if cmap:
            dst.write_colormap(1, cmap)
        if dst._copy_of is not None:
            return
        itemsize = 16 if sample in ("CInt16", "CInt32", "CFloat64") else 8
        rows = max(1, min(src.height, (64 << 20) // max(1, src.width * src.count * itemsize)))
        for y in range(0, src.height, rows):
            w = Window(0, y, src.width, min(rows, src.height - y))
            dst.write(src._read_for_copy(window=w), window=w)
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
