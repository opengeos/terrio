"""Datasets: `DatasetReader`, `DatasetWriter` and `MemoryFile`, as rasterio defines them.

Readers read GeoTIFFs and JPEG 2000 files (local or remote) lazily, by window, through terrio;
other formats are read into memory when opened. Writers hold the pixels in memory (or, above
`TERRIO_RIO_IN_MEMORY_LIMIT` bytes, in a temporary file mapped in memory, written to GeoTIFF in rows
of blocks) and write the file when closed.
"""

from __future__ import annotations

import contextlib
import math
import os
import shutil
import tempfile
import uuid
import warnings
from collections.abc import Iterator, Sequence
from contextlib import ExitStack
from typing import Any

import numpy as np
from affine import Affine

from terrio import _terrio
from terrio.rio import _tiff
from terrio.rio._resample import gdal_cast, raster_io
from terrio.rio.coords import BoundingBox
from terrio.rio.crs import CRS
from terrio.rio.dtypes import _getnpdtype, check_dtype, in_dtype_range
from terrio.rio.enums import (
    ColorInterp,
    Compression,
    Interleaving,
    MaskFlags,
    PhotometricInterp,
    Resampling,
)
from terrio.rio.errors import (
    CRSError,
    DatasetAttributeError,
    DriverCapabilityError,
    NotGeoreferencedWarning,
    OverviewCreationError,
    RasterBlockError,
    RasterioIOError,
    UnsupportedOperation,
)
from terrio.rio.profiles import Profile
from terrio.rio.transform import TransformMethodsMixin, guard_transform
from terrio.rio.windows import Window, WindowMethodsMixin, evaluate

# Drivers terrio writes, and the extensions rasterio maps to them.
_EXTENSIONS = {
    ".tif": "GTiff",
    ".tiff": "GTiff",
    ".asc": "AAIGrid",
    ".img": "HFA",
    ".png": "PNG",
    ".jpg": "JPEG",
    ".jp2": "JP2OpenJPEG",
    ".nc": "netCDF",
    ".vrt": "VRT",
    ".gpkg": "GPKG",
    ".zarr": "Zarr",
    ".bil": "EHdr",
    ".envi": "ENVI",
    ".rst": "RST",
    ".sdat": "SAGA",
    ".grd": "GSBG",
    ".hgt": "SRTMHGT",
    ".xyz": "XYZ",
    ".mbtiles": "MBTiles",
    ".pmtiles": "PMTiles",
}

# Writers larger than this (in bytes; `TERRIO_RIO_IN_MEMORY_LIMIT` overrides it) keep their
# pixels in a temporary file next to the output, mapped in memory, and are written to GeoTIFF in
# rows of blocks, without holding the raster in RAM.
_IN_MEMORY_LIMIT = 512 * 1024 * 1024

# The drivers of files that can hold subdatasets (several variables or arrays).
_SUBDATASET_DRIVERS = frozenset({"netCDF", "HDF5", "HDF4", "Zarr", "STACIT", "STACTA", "GPKG"})


def _in_memory_limit() -> int:
    """The size above which writers keep their pixels on disk.

    Returns:
        int: Bytes.
    """
    try:
        return int(os.environ.get("TERRIO_RIO_IN_MEMORY_LIMIT", _IN_MEMORY_LIMIT))
    except ValueError:
        return _IN_MEMORY_LIMIT


# Resampling names for overviews.
_OVERVIEW_RESAMPLING = {
    Resampling.nearest: "nearest",
    Resampling.average: "average",
    Resampling.mode: "mode",
    Resampling.min: "min",
    Resampling.max: "max",
}

_SUPPORTED_DTYPES = {
    "uint8",
    "int8",
    "uint16",
    "int16",
    "uint32",
    "int32",
    "uint64",
    "int64",
    "float32",
    "float64",
}


def driver_from_extension(path: Any) -> str:
    """The driver rasterio picks for a file name's extension.

    Args:
        path: The path.

    Returns:
        str: The driver's short name.

    Raises:
        ValueError: If the extension is unknown.
    """
    ext = os.path.splitext(str(path))[1].lower()
    try:
        return _EXTENSIONS[ext]
    except KeyError:
        raise ValueError(f"Unable to detect driver. Extension {ext} is not known.") from None


def _to_error(err: Exception, path: str) -> RasterioIOError:
    """A `RasterioIOError` for a terrio error.

    Args:
        err: The error.
        path: The dataset path.

    Returns:
        RasterioIOError: The error to raise.
    """
    if isinstance(err, FileNotFoundError) or "No such file" in str(err):
        return RasterioIOError(f"{path}: No such file or directory")
    return RasterioIOError(f"{path}: {err}")


def _terrio_path(path: str) -> str:
    """A path terrio opens: GDAL `/vsicurl/` and `/vsis3/` prefixes become URLs.

    Args:
        path: The path.

    Returns:
        str: The path or URL.
    """
    if path.startswith("/vsicurl/"):
        return path[len("/vsicurl/") :]
    if path.startswith("/vsis3/"):
        return "s3://" + path[len("/vsis3/") :]
    if path.startswith("/vsigs/"):
        return "gs://" + path[len("/vsigs/") :]
    if path.startswith("/vsiaz/"):
        return "az://" + path[len("/vsiaz/") :]
    if path.startswith("zip://"):
        return path[len("zip://") :]
    if path.startswith("file://"):
        return path[len("file://") :]
    return path


def _is_remote(path: str) -> bool:
    """Whether a path is a URL or cloud object.

    Args:
        path: The path.

    Returns:
        bool: True when remote.
    """
    return "://" in path and not path.startswith("file://")


class DatasetBase(WindowMethodsMixin, TransformMethodsMixin):
    """What every dataset has: its shape, georeferencing and metadata."""

    def __init__(self) -> None:
        """Creates an empty dataset; subclasses fill it."""
        self.name = ""
        self.mode = "r"
        self.driver = "GTiff"
        self.width = 0
        self.height = 0
        self._count = 0
        self._dtype = "uint8"
        self._nodata: float | None = None
        self._crs: CRS | None = None
        self._transform: Affine = Affine.identity()
        self._structure: _tiff.Structure | None = None
        self._subdatasets: list[str] | None = None
        self._overview_sizes: list[tuple[int, int]] = []
        self._closed = False
        self._env = ExitStack()
        self._georeferenced = True
        self._nodatavals: list[float | None] | None = None
        self._gcps: list[Any] = []
        self._gcp_crs: CRS | None = None
        self._rpcs: Any = None

    # Context manager and state.

    def __enter__(self) -> Any:
        """Enters a `with` block.

        Returns:
            The dataset.
        """
        return self

    def __exit__(self, *args: object) -> None:
        """Closes the dataset.

        Args:
            *args: Exception details.
        """
        self.close()

    def close(self) -> None:
        """Closes the dataset."""
        if not self._closed:
            self._closed = True
            self._env.close()

    def stop(self) -> None:
        """Closes the dataset (rasterio's name for it on some classes)."""
        self.close()

    @property
    def closed(self) -> bool:
        """Whether the dataset is closed.

        Returns:
            bool: True when closed.
        """
        return self._closed

    def _check_open(self) -> None:
        """Raises when the dataset is closed.

        Raises:
            RasterioIOError: If it is closed.
        """
        if self._closed:
            raise RasterioIOError("Dataset is closed")

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `<open DatasetReader name='...' mode='r'>`.
        """
        state = "closed" if self._closed else "open"
        return f"<{state} {type(self).__name__} name='{self.name}' mode='{self.mode}'>"

    # Shape.

    @property
    def count(self) -> int:
        """The number of bands.

        Returns:
            int: The count.
        """
        return self._count

    @property
    def shape(self) -> tuple[int, int]:
        """The raster's `(height, width)`.

        Returns:
            tuple: The shape.
        """
        return (self.height, self.width)

    @property
    def indexes(self) -> tuple[int, ...]:
        """The band numbers, from 1.

        Returns:
            tuple: The band numbers.
        """
        return tuple(range(1, self._count + 1))

    @property
    def dtypes(self) -> tuple[str, ...]:
        """The data type of each band.

        Returns:
            tuple: NumPy dtype names.
        """
        return tuple(self._dtype for _ in range(self._count))

    @property
    def structure(self) -> _tiff.Structure:
        """What GDAL reports about the dataset besides its pixels.

        Returns:
            Structure: The structure.
        """
        if self._structure is None:
            self._structure = _tiff.Structure(
                count=self._count,
                width=self.width,
                height=self.height,
                block_shape=(1, self.width),
                colorinterp=[ColorInterp.gray] + [ColorInterp.undefined] * (self._count - 1),
                band_tags=[{} for _ in range(self._count)],
                descriptions=[None] * self._count,
                scales=[1.0] * self._count,
                offsets=[0.0] * self._count,
                units=[None] * self._count,
            )
        return self._structure

    @property
    def block_shapes(self) -> list[tuple[int, int]]:
        """The block shape `(rows, cols)` of each band.

        Returns:
            list: The block shapes.
        """
        return [self.structure.block_shape] * self._count

    def block_window(self, bidx: int, i: int, j: int) -> Window:
        """The window of a block.

        Args:
            bidx: The band, from 1.
            i: The block row.
            j: The block column.

        Returns:
            Window: The block's window, cropped to the raster.
        """
        h, w = self.block_shapes[bidx - 1]
        row, col = i * h, j * w
        return Window(col, row, min(w, self.width - col), min(h, self.height - row))

    def block_windows(self, bidx: int = 0) -> Iterator[tuple[tuple[int, int], Window]]:
        """Iterates over the blocks of a band, row by row.

        Args:
            bidx: The band, from 1 (0 for the first band when all bands have the same blocks).

        Yields:
            tuple: `((block_row, block_col), window)`.

        Raises:
            ValueError: If `bidx` is 0 and the bands' blocks differ.
        """
        shapes = self.block_shapes
        if bidx < 1:
            if len(set(shapes)) > 1:
                raise ValueError(
                    "A band index must be provided when band block shapes are inhomogeneous"
                )
            bidx = 1
        h, w = shapes[bidx - 1]
        rows = -(-self.height // h)
        cols = -(-self.width // w)
        for j in range(rows):
            row = j * h
            height = min(h, self.height - row)
            for i in range(cols):
                col = i * w
                yield (j, i), Window(col, row, min(w, self.width - col), height)

    def block_size(self, bidx: int, i: int, j: int) -> int:
        """The size in bytes of a block, uncompressed.

        Args:
            bidx: The band.
            i: The block row.
            j: The block column.

        Returns:
            int: The size.
        """
        h, w = self.block_shapes[bidx - 1]
        return h * w * np.dtype(self.dtypes[bidx - 1]).itemsize

    # Georeferencing.

    @property
    def crs(self) -> CRS | None:
        """The coordinate reference system, or None.

        Returns:
            CRS | None: The CRS.
        """
        return self._crs

    @crs.setter
    def crs(self, value: Any) -> None:
        """Sets the CRS (write modes only).

        Args:
            value: Anything `CRS.from_user_input` accepts.
        """
        self._set_crs(value)

    def _set_crs(self, value: Any) -> None:
        """Sets the CRS.

        Args:
            value: The CRS.

        Raises:
            DatasetAttributeError: In read mode.
        """
        raise DatasetAttributeError("read-only attribute")

    @property
    def transform(self) -> Affine:
        """The affine transform from pixel to map coordinates.

        Returns:
            Affine: The transform.
        """
        return self._transform

    @transform.setter
    def transform(self, value: Affine) -> None:
        """Sets the transform (write modes only).

        Args:
            value: The transform.
        """
        self.write_transform(value)

    def write_transform(self, value: Any) -> None:
        """Sets the transform.

        Args:
            value: The transform.

        Raises:
            DatasetAttributeError: In read mode.
        """
        raise DatasetAttributeError("read-only attribute")

    def get_transform(self) -> list[float]:
        """The GDAL geotransform.

        Returns:
            list: Six numbers.
        """
        return list(self._transform.to_gdal())

    @property
    def bounds(self) -> BoundingBox:
        """The dataset's bounds.

        Returns:
            BoundingBox: `(left, bottom, right, top)`.
        """
        a, b, c, d, e, f, _, _, _ = self._transform
        if b == d == 0:
            return BoundingBox(c, f + e * self.height, c + a * self.width, f)
        from terrio.rio.transform import array_bounds

        return BoundingBox(*array_bounds(self.height, self.width, self._transform))

    @property
    def res(self) -> tuple[float, float]:
        """The pixel size.

        Returns:
            tuple: `(x_resolution, y_resolution)`, positive.
        """
        a, b, c, d, e, f, _, _, _ = self._transform
        return math.sqrt(a * a + d * d), math.sqrt(b * b + e * e)

    def lnglat(self) -> tuple[float, float]:
        """The longitude and latitude of the dataset's centre.

        Returns:
            tuple: `(longitude, latitude)`.
        """
        from terrio.rio.warp import transform

        w, s, e, n = self.bounds
        xs, ys = transform(self.crs, "EPSG:4326", [(w + e) / 2.0], [(s + n) / 2.0])
        return xs[0], ys[0]

    @property
    def gcps(self) -> tuple[list[Any], CRS | None]:
        """Ground control points, and their CRS (GeoTIFF tie points, as GDAL reads them).

        Returns:
            tuple: `([GroundControlPoint, ...], crs)`, `([], None)` without GCPs.
        """
        return (list(self._gcps), self._gcp_crs if self._gcps else None)

    @gcps.setter
    def gcps(self, value: Any) -> None:
        """Sets the ground control points (write modes only).

        Args:
            value: `(gcps, crs)`.
        """
        self._set_gcps(*value)

    def _set_gcps(self, gcps: Any, crs: Any = None) -> None:
        """Sets ground control points.

        Args:
            gcps: `GroundControlPoint`s.
            crs: Their CRS.

        Raises:
            DatasetAttributeError: In read mode.
        """
        raise DatasetAttributeError("read-only attribute")

    @property
    def rpcs(self) -> Any:
        """Rational polynomial coefficients, as GDAL reads them.

        From the GeoTIFF RPCCoefficientTag, or a `_RPC.TXT` or `.RPB` sidecar.

        Returns:
            RPC | None: The coefficients.
        """
        return self._rpcs

    @rpcs.setter
    def rpcs(self, value: Any) -> None:
        """Sets the rational polynomial coefficients (write modes only).

        Args:
            value: An `RPC` or GDAL's RPC items.
        """
        self._set_rpcs(value)

    def _set_rpcs(self, value: Any) -> None:
        """Sets RPCs.

        Args:
            value: The coefficients.

        Raises:
            DatasetAttributeError: In read mode.
        """
        raise DatasetAttributeError("read-only attribute")

    # Nodata and masks.

    @property
    def nodatavals(self) -> tuple[float | None, ...]:
        """The nodata value of each band (None when unset or outside the data type's range).

        Returns:
            tuple: The values.
        """
        vals = self._nodatavals if self._nodatavals is not None else [self._nodata] * self._count
        return tuple(
            None if nd is None or not in_dtype_range(nd, self._dtype) else nd for nd in vals
        )

    def get_nodatavals(self) -> tuple[float | None, ...]:
        """The nodata value of each band.

        Returns:
            tuple: The values.
        """
        return self.nodatavals

    @property
    def nodata(self) -> float | None:
        """The nodata value of the first band.

        Returns:
            float | None: The value.
        """
        return self.nodatavals[0] if self._count else None

    @nodata.setter
    def nodata(self, value: float | None) -> None:
        """Sets the nodata value of every band (write modes only).

        Args:
            value: The value, or None to remove it.
        """
        self._set_nodatavals([value] * self._count)

    def _set_nodatavals(self, vals: Sequence[float | None]) -> None:
        """Sets nodata values.

        Args:
            vals: One value per band.

        Raises:
            DatasetAttributeError: In read mode.
        """
        raise DatasetAttributeError("read-only attribute")

    def _mask_flags(self) -> tuple[int, ...]:
        """The GDAL mask flags of each band.

        Returns:
            tuple: The flags.
        """
        s = self.structure
        alpha = s.alpha_band()
        out = []
        for b in self.indexes:
            if s.has_mask:
                out.append(MaskFlags.per_dataset.value)
            elif self.nodatavals[b - 1] is not None:
                out.append(MaskFlags.nodata.value)
            elif alpha is not None and b != alpha:
                out.append(MaskFlags.per_dataset.value | MaskFlags.alpha.value)
            else:
                out.append(MaskFlags.all_valid.value)
        return tuple(out)

    @property
    def mask_flag_enums(self) -> tuple[list[MaskFlags], ...]:
        """The sources of each band's mask.

        Returns:
            tuple: One list of `MaskFlags` per band.
        """
        return tuple([f for f in MaskFlags if x & f.value] for x in self._mask_flags())

    # Metadata.

    @property
    def descriptions(self) -> tuple[str | None, ...]:
        """Band descriptions.

        Returns:
            tuple: One description (or None) per band.
        """
        return tuple(self.structure.descriptions)

    @descriptions.setter
    def descriptions(self, value: Sequence[str | None]) -> None:
        """Sets every band description (write modes only).

        Args:
            value: One description per band.
        """
        self._set_all("descriptions", value, "description")

    @property
    def scales(self) -> tuple[float, ...]:
        """Band scales.

        Returns:
            tuple: One scale per band.
        """
        return tuple(self.structure.scales)

    @scales.setter
    def scales(self, value: Sequence[float]) -> None:
        """Sets every band scale (write modes only).

        Args:
            value: One scale per band.
        """
        self._set_all("scales", value, "scale")

    @property
    def offsets(self) -> tuple[float, ...]:
        """Band offsets.

        Returns:
            tuple: One offset per band.
        """
        return tuple(self.structure.offsets)

    @offsets.setter
    def offsets(self, value: Sequence[float]) -> None:
        """Sets every band offset (write modes only).

        Args:
            value: One offset per band.
        """
        self._set_all("offsets", value, "offset")

    @property
    def units(self) -> tuple[str | None, ...]:
        """Band units.

        Returns:
            tuple: One unit (or None) per band.
        """
        return tuple(self.structure.units)

    @units.setter
    def units(self, value: Sequence[str | None]) -> None:
        """Sets every band unit (write modes only).

        Args:
            value: One unit per band.
        """
        self._set_all("units", value, "unit")

    def _set_all(self, attr: str, value: Sequence[Any], what: str) -> None:
        """Sets a per-band attribute.

        Args:
            attr: The structure attribute.
            value: One value per band.
            what: The attribute's name in messages.

        Raises:
            DatasetAttributeError: In read mode.
        """
        raise DatasetAttributeError("read-only attribute")

    @property
    def colorinterp(self) -> tuple[ColorInterp, ...]:
        """The color interpretation of each band.

        Returns:
            tuple: The color interpretations.
        """
        return tuple(self.structure.colorinterp)

    @colorinterp.setter
    def colorinterp(self, value: Sequence[ColorInterp]) -> None:
        """Sets the color interpretations (write modes only).

        Args:
            value: One color interpretation per band.

        Raises:
            RasterioIOError: In read mode.
            ValueError: If the number of values is wrong.
        """
        if self.mode == "r":
            raise RasterioIOError(
                "Can only set color interpretation when dataset is opened in 'r+' or 'w' mode, "
                "not 'r'."
            )
        if len(value) != self._count:
            raise ValueError(
                f"Must set color interpretation for all bands.  Found {self._count} bands but "
                f"attempting to set color interpretation to: {value}"
            )
        self.structure.colorinterp = [ColorInterp(v) for v in value]
        self._colorinterp_set = True
        self._dirty = True

    def colormap(self, bidx: int) -> dict[int, tuple[int, int, int, int]]:
        """The color table of a band.

        Args:
            bidx: The band, from 1.

        Returns:
            dict: Index to `(r, g, b, a)`.

        Raises:
            ValueError: If the band has no color table.
        """
        if bidx < 1 or bidx > self._count:
            raise IndexError("band index out of range")
        cmap = self.structure.colormap
        if bidx != 1 or cmap is None:
            raise ValueError("NULL color table")
        return dict(cmap)

    def tags(self, bidx: int = 0, ns: str | None = None) -> dict[str, str]:
        """Metadata items of the dataset or a band.

        Args:
            bidx: The band, from 1, or 0 for the dataset.
            ns: The metadata domain (namespace), such as `IMAGE_STRUCTURE`.

        Returns:
            dict: The items.
        """
        s = self.structure
        if bidx > 0:
            if bidx > self._count:
                raise IndexError("band index out of range")
            return dict(s.band_tags[bidx - 1]) if not ns else {}
        if ns:
            return dict(s.namespaces.get(ns, {}))
        return dict(s.tags)

    def tag_namespaces(self, bidx: int = 0) -> list[str]:
        """The metadata domains of the dataset or a band.

        Args:
            bidx: The band, or 0 for the dataset.

        Returns:
            list: The domain names.
        """
        return [] if bidx > 0 else [k for k, v in self.structure.namespaces.items() if v]

    def get_tag_item(
        self, ns: str, dm: str | None = None, bidx: int = 0, ovr: int | None = None
    ) -> str | None:
        """One metadata item.

        Args:
            ns: The item's name.
            dm: The domain.
            bidx: The band, or 0 for the dataset.
            ovr: Unused.

        Returns:
            str | None: The value.
        """
        return self.tags(bidx, dm).get(ns)

    @property
    def compression(self) -> Compression | str | None:
        """The compression, from the `IMAGE_STRUCTURE` metadata.

        Returns:
            Compression | str | None: The compression.
        """
        val = self.tags(ns="IMAGE_STRUCTURE").get("COMPRESSION")
        if not val:
            return None
        val = val.split()[-1]
        try:
            return Compression(val)
        except ValueError:
            return val

    @property
    def interleaving(self) -> Interleaving | None:
        """How bands are interleaved.

        Returns:
            Interleaving | None: The interleaving.
        """
        val = self.tags(ns="IMAGE_STRUCTURE").get("INTERLEAVE")
        return Interleaving(val) if val else None

    @property
    def photometric(self) -> PhotometricInterp | None:
        """The source color space of JPEG-compressed YCbCr or CMYK images.

        Returns:
            PhotometricInterp | None: The color space.
        """
        val = self.tags(ns="IMAGE_STRUCTURE").get("SOURCE_COLOR_SPACE")
        return PhotometricInterp(val) if val else None

    @property
    def is_tiled(self) -> bool:
        """Whether the blocks are tiles rather than strips.

        Returns:
            bool: True when tiled.
        """
        return bool(self.block_shapes) and all(self.width != w for _, w in self.block_shapes)

    @property
    def meta(self) -> dict[str, Any]:
        """The basic metadata: driver, dtype, nodata, size, count, CRS and transform.

        Returns:
            dict: The metadata.
        """
        return {
            "driver": self.driver,
            "dtype": self.dtypes[0] if self._count else "float_",
            "nodata": self.nodata,
            "width": self.width,
            "height": self.height,
            "count": self.count,
            "crs": self.crs,
            "transform": self.transform,
        }

    @property
    def profile(self) -> Profile:
        """The metadata and creation options, to open a copy of the dataset with.

        Returns:
            Profile: The profile.
        """
        m = Profile(**self.meta)
        if self.block_shapes:
            m.update(blockxsize=self.block_shapes[0][1], blockysize=self.block_shapes[0][0])
        m.update(tiled=bool(self.block_shapes) and self.is_tiled)
        comp = self.compression
        if comp:
            m["compress"] = getattr(comp, "name", comp)
        inter = self.interleaving
        if inter:
            m["interleave"] = inter.name
        photo = self.photometric
        if photo:
            m["photometric"] = photo.name
        return m

    def overviews(self, bidx: int) -> list[int]:
        """The decimation factors of a band's overviews.

        Args:
            bidx: The band, from 1.

        Returns:
            list: The factors, such as `[2, 4, 8]`.
        """
        return [int(round(self.width / ow)) for ow, _ in self._overview_sizes]

    @property
    def files(self) -> list[str]:
        """The files of the dataset.

        Returns:
            list: The paths.
        """
        if not (self.name and os.path.exists(self.name)):
            return []
        out = [self.name]
        stem, ext = os.path.splitext(self.name)
        world = "." + ext[1:2] + ext[-1:] + "w" if len(ext) > 2 else ".wld"
        sidecars = [
            self.name + ".aux.xml",
            self.name + ".ovr",
            self.name + ".msk",
            stem + world,
            stem + ".wld",
            stem + "_rpc.txt",
            stem + "_RPC.TXT",
            stem + ".RPB",
            stem + ".rpb",
            stem + ".hdr",
        ]
        for f in sidecars:
            if f not in out and os.path.isfile(f):
                out.append(f)
        return out

    @property
    def subdatasets(self) -> list[str]:
        """The names of the dataset's subdatasets, as GDAL lists them.

        Files of several variables or arrays (netCDF, HDF5, Zarr, STAC collections) have some,
        such as `NETCDF:"file.nc":/group/var`; a subdataset itself and other rasters have none.

        Returns:
            list: The subdatasets' names.
        """
        if self.driver not in _SUBDATASET_DRIVERS or not self.name:
            return []
        if self._subdatasets is None:
            # A failure (a remote file unreachable) is not remembered: the next call tries again.
            try:
                info = _terrio.info(_terrio_path(self.name))
            except Exception:
                return []
            self._subdatasets = [s["name"] for s in info.get("subdatasets") or []]
        return list(self._subdatasets)

    def checksum(self, bidx: int, window: Any = None) -> int:
        """GDAL's checksum of a band (`GDALChecksumImage`).

        Args:
            bidx: The band.
            window: The window, or None for the whole band.

        Returns:
            int: The checksum.
        """
        arr = np.asarray(self.read(bidx, window=window))  # type: ignore[attr-defined]
        if arr.dtype.kind == "f":
            x = arr.astype(np.float64)
            with np.errstate(invalid="ignore"):
                v = np.clip(np.floor(x + 0.5), -2147483647, 2147483647)
            v = np.where(np.isfinite(x), v, -2147483648).astype(np.int64)
        else:
            v = arr.astype(np.int64)
        primes = np.array([7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43], dtype=np.int64)
        flat = v.ravel()
        total = int(np.fmod(flat, primes[np.arange(flat.size) % 11]).sum())
        return total % 65536


class DatasetReader(DatasetBase):
    """A dataset opened for reading."""

    def __init__(self, path: Any, driver: Any = None, **kwargs: Any) -> None:
        """Opens a dataset for reading.

        Args:
            path: A path, URL, `s3://` object or GDAL `/vsicurl/` path.
            driver: Unused: terrio detects the format.
            **kwargs: Unused open options.

        Raises:
            RasterioIOError: If the dataset cannot be opened.
        """
        super().__init__()
        self.name = os.fspath(path) if not isinstance(path, str) else path
        self.mode = "r"
        self._path = _terrio_path(self.name)
        try:
            raster = _terrio.open_raster(self._path, lazy=True)
        except Exception as err:
            raise _to_error(err, self.name) from err
        self._load(raster)

    def _load(self, raster: Any) -> None:
        """Takes the dataset's properties from a terrio raster.

        Args:
            raster: The `terrio.Raster`.
        """
        self._raster = raster
        self.driver = raster.driver
        self.width = raster.width
        self.height = raster.height
        self._count = raster.count
        self._dtype = raster.dtype
        self._nodata = raster.nodata
        crs = raster.crs_wkt or raster.crs
        if crs:
            try:
                self._crs = CRS.from_user_input(raster.crs) if raster.crs else None
            except CRSError:
                self._crs = None
            if self._crs is None and raster.crs_wkt:
                try:
                    self._crs = CRS.from_wkt(raster.crs_wkt)
                except CRSError:
                    self._crs = None
        self._transform = Affine.from_gdal(*raster.transform)
        self._overview_sizes = list(raster.overviews)
        structure = None
        if self.driver == "GTiff":
            structure = _tiff.tiff_structure(self._path, raster)
        if structure is None:
            structure = _tiff.default_structure(raster)
        if not _is_remote(self._path):
            _tiff.apply_pam(structure, self._path)
        # A per-dataset mask, internal or in a `.msk` sidecar (read by the raster, when the file
        # has no internal one).
        structure.mask_in_sidecar = bool(raster.has_mask) and not structure.has_mask
        structure.has_mask = structure.has_mask or bool(raster.has_mask)
        # Bands a PAM sidecar doesn't list keep the file's own nodata, not band 1's PAM value.
        file_nodata = self._nodata
        if structure.nodata_override is not None:
            self._nodata = structure.nodata_override
        if len(structure.band_nodata) > 0:
            vals = [structure.band_nodata.get(b, file_nodata) for b in range(self._count)]
            if len(set(map(repr, vals))) > 1:
                self._nodatavals = vals
        self._structure = structure
        self._load_gcps_rpcs(structure)
        if self._transform == Affine(1.0, 0.0, 0.0, 0.0, -1.0, float(self.height)):
            # terrio's default for rasters without georeferencing.
            self._transform = Affine.identity()
            self._georeferenced = False

    def _load_gcps_rpcs(self, structure: _tiff.Structure) -> None:
        """Takes the GCPs and RPCs of a structure (or of RPC sidecars).

        Args:
            structure: The dataset's structure.
        """
        from terrio.rio.control import GroundControlPoint
        from terrio.rio.rpc import RPC, read_rpc_file

        if structure.gcps:
            self._gcps = [
                GroundControlPoint(row=line, col=pixel, x=x, y=y, z=z, id=str(i + 1), info="")
                for i, (pixel, line, x, y, z) in enumerate(structure.gcps)
            ]
            # GDAL reports the CRS as the GCPs' and the dataset as not georeferenced.
            self._gcp_crs = self._crs
            self._crs = None
        if structure.rpc_tag is not None:
            self._rpcs = RPC.from_tiff_tag(structure.rpc_tag)
        elif not _is_remote(self._path) and os.path.isfile(self._path):
            stem = os.path.splitext(self._path)[0]
            for side in (stem + "_rpc.txt", stem + "_RPC.TXT", stem + ".RPB", stem + ".rpb"):
                items = read_rpc_file(side) if os.path.isfile(side) else None
                if items:
                    self._rpcs = RPC.from_gdal(items)
                    break

    def close(self) -> None:
        """Closes the dataset, releasing the file."""
        if not self._closed:
            self._raster = None
        super().close()

    # Reading.

    def _read_level(
        self, bands: Sequence[int], window: tuple[int, int, int, int], level: int | None
    ) -> np.ndarray:
        """Reads an integer window of a level.

        Args:
            bands: The bands, from 1.
            window: `(x, y, w, h)`.
            level: None for full resolution, else an overview index.

        Returns:
            numpy.ndarray: `(bands, h, w)`.

        Raises:
            RasterioIOError: If the read fails.
        """
        self._check_open()
        try:
            return np.asarray(self._raster.read(list(bands), window=window, overview=level))
        except Exception as err:
            raise RasterioIOError(f"Read failed: {err}") from err

    def _readable(self) -> None:
        """Raises when the dataset cannot be read.

        Raises:
            UnsupportedOperation: In `w` mode.
        """
        self._check_open()
        if self.mode == "w":
            raise UnsupportedOperation("not readable")

    def _io(
        self,
        indexes: Sequence[int],
        window: Window | None,
        buf: tuple[int, int],
        resampling: Resampling,
    ) -> np.ndarray:
        """Reads bands of a window (inside the raster) into a buffer.

        Args:
            indexes: The bands.
            window: The window, or None for the whole raster.
            buf: The output `(height, width)`.
            resampling: The resampling method.

        Returns:
            numpy.ndarray: `(bands, height, width)` in the dataset's data type.
        """
        resampling = Resampling(resampling)
        if resampling != Resampling.rms and resampling > 7:
            from terrio.rio.errors import ResamplingAlgorithmError

            raise ResamplingAlgorithmError(
                f"{resampling!r} can be used for warp operations but not for reads and writes"
            )
        kernels = (
            Resampling.bilinear,
            Resampling.cubic,
            Resampling.cubic_spline,
            Resampling.lanczos,
        )
        if resampling in kernels and self.structure.colormap is not None:
            # GDAL falls back to nearest neighbour on paletted bands.
            resampling = Resampling.nearest
        if window is None:
            fw: tuple[float, float, float, float] = (0, 0, self.width, self.height)
        else:
            fw = (window.col_off, window.row_off, window.width, window.height)
        if buf[0] <= 0 or buf[1] <= 0:
            return np.zeros((len(indexes), max(buf[0], 0), max(buf[1], 0)), self._dtype)
        if fw[2] <= 0 or fw[3] <= 0:
            return np.zeros((len(indexes), buf[0], buf[1]), self._dtype)
        nodata = self.nodatavals[0] if self._count else None
        return raster_io(
            self._read_level,
            indexes,
            (self.width, self.height),
            self._overview_sizes,
            fw,
            buf,
            int(resampling),
            nodata,
        )

    def _masks_from(
        self, indexes: Sequence[int], data: np.ndarray | None, window: Window | None, buf: Any
    ) -> np.ndarray:
        """The masks (0 or 255) of bands, from their pixels.

        Args:
            indexes: The bands.
            data: The bands' pixels, or None to read them.
            window: The window.
            buf: The output `(height, width)`.

        Returns:
            numpy.ndarray: The masks, uint8.
        """
        flags = self._mask_flags()
        out = np.full((len(indexes), buf[0], buf[1]), 255, np.uint8)
        alpha = self.structure.alpha_band()
        for k, b in enumerate(indexes):
            f = flags[b - 1]
            if f & MaskFlags.nodata.value:
                vals = (
                    data[k]
                    if data is not None
                    else self._io([b], window, buf, Resampling.nearest)[0]
                )
                nd = self.nodatavals[b - 1]
                assert nd is not None
                if np.isnan(nd):
                    invalid = (
                        np.isnan(vals) if vals.dtype.kind == "f" else np.zeros(vals.shape, bool)
                    )
                else:
                    ndv = np.array(nd).astype(vals.dtype) if vals.dtype.kind == "f" else nd
                    invalid = vals == ndv
                out[k][invalid] = 0
            elif f & MaskFlags.alpha.value and alpha is not None:
                a = self._io([alpha], window, buf, Resampling.nearest)[0]
                if a.dtype == np.uint8:
                    out[k] = a
                elif a.dtype.kind in "iu":
                    out[k] = np.where(a == 0, 0, np.clip(a.astype(np.int64) >> 8, 1, 255))
                else:
                    out[k] = np.clip(a, 0, 255).astype(np.uint8)
            elif f & MaskFlags.per_dataset.value and self.structure.has_mask:
                out[k] = self._internal_mask(window, buf)
        return out

    def _internal_mask(self, window: Window | None, buf: Any) -> np.ndarray:
        """Reads the internal (per-dataset) mask of a GeoTIFF into a buffer.

        Args:
            window: The window, or None for the whole raster.
            buf: The output `(height, width)`.

        Returns:
            numpy.ndarray: The mask, 0 or 255.
        """
        raster = self._raster

        def read(bands: Sequence[int], w: tuple[int, int, int, int], level: int | None) -> Any:
            m = raster.read_mask(window=w, overview=level)
            if m is None:
                return np.full((1, w[3], w[2]), 255, np.uint8)
            return np.asarray(m)[np.newaxis]

        fw: tuple[float, float, float, float] = (
            (0, 0, self.width, self.height)
            if window is None
            else (window.col_off, window.row_off, window.width, window.height)
        )
        out: np.ndarray = np.asarray(
            raster_io(read, [1], (self.width, self.height), self._overview_sizes, fw, buf, 0, None)
        )[0]
        return out

    def _prepare(
        self, indexes: Any, window: Any, boundless: bool
    ) -> tuple[list[int], bool, Window | None, tuple[int, int]]:
        """Normalizes the band and window arguments of a read.

        Args:
            indexes: None, an int or a sequence of ints.
            window: None, a `Window` or ranges.
            boundless: Whether the window may extend past the raster.

        Returns:
            tuple: The bands, whether to return 2-D arrays, the window and its shape.

        Raises:
            ValueError: If no band is asked for.
            IndexError: If a band does not exist.
        """
        return2d = False
        if indexes is None:
            idx = list(self.indexes)
        elif isinstance(indexes, (int, np.integer)):
            idx = [int(indexes)]
            return2d = True
        else:
            idx = [int(i) for i in indexes]
        if not idx:
            raise ValueError("No indexes to read")
        for b in idx:
            if b not in self.indexes:
                raise IndexError(f"band index {b} out of range (not in {self.indexes})")
        win: Window | None = None
        shape = (self.height, self.width)
        if window:
            if isinstance(window, tuple):
                window = Window.from_slices(
                    window[0], window[1], height=self.height, width=self.width, boundless=boundless
                )
            win = (
                window if isinstance(window, Window) else evaluate(window, self.height, self.width)
            )
            if not boundless:
                win = win.crop(self.height, self.width)
            r = win.round_lengths()
            shape = (int(r.height), int(r.width))
        return idx, return2d, win, shape

    def read(
        self,
        indexes: Any = None,
        out: np.ndarray | None = None,
        window: Any = None,
        masked: bool = False,
        out_shape: Any = None,
        boundless: bool = False,
        resampling: Resampling = Resampling.nearest,
        fill_value: Any = None,
        out_dtype: Any = None,
    ) -> Any:
        """Reads bands into an array.

        Args:
            indexes: A band (1-based int, giving a 2-D array), a sequence of bands, or None for
                all bands.
            out: An array to read into.
            window: The pixels to read: a `Window` (possibly fractional) or ranges.
            masked: Return a masked array, masking nodata (or alpha) pixels.
            out_shape: The output shape `(rows, cols)` or `(bands, rows, cols)`: pixels are
                resampled (through overviews when decimating, as GDAL does).
            boundless: Let the window extend past the raster; pixels outside are filled.
            resampling: The resampling method when the output shape differs.
            fill_value: The value of pixels outside the raster (boundless reads) or masked.
            out_dtype: Convert pixels to this data type, as GDAL converts them.

        Returns:
            numpy.ndarray | numpy.ma.MaskedArray: `(bands, rows, cols)`, or `(rows, cols)` for a
            single band index.

        Raises:
            ValueError: If `out` and `out_shape` are both given, or `out` has the wrong shape.
        """
        self._readable()
        idx, return2d, win, win_shape = self._prepare(indexes, window, boundless)
        if return2d and out is not None and out.ndim == 2:
            out = out[np.newaxis]
        dtype = _getnpdtype(out_dtype if out_dtype is not None else self._dtype)
        if out is not None and out_shape is not None:
            raise ValueError("out and out_shape are exclusive")
        if out is not None:
            if out.shape[0] != len(idx):
                raise ValueError(
                    f"'out' shape {out.shape} does not match window shape {(len(idx),) + win_shape}"
                )
            buf = (out.shape[-2], out.shape[-1])
            dtype = out.dtype
        elif out_shape is not None:
            buf = (int(out_shape[-2]), int(out_shape[-1]))
        else:
            buf = win_shape
        nodatavals = [self.nodatavals[b - 1] for b in idx]
        fill = None if not nodatavals else nodatavals[-1]
        if fill is not None and not np.isnan(fill):
            kind = np.dtype(self._dtype).kind
            lim: Any = np.iinfo(self._dtype) if kind in "iu" else np.finfo(self._dtype)
            fill = min(max(fill, lim.min), lim.max)
            nodatavals = [
                None if v is None else (v if np.isnan(v) else min(max(v, lim.min), lim.max))
                for v in nodatavals
            ]

        if not boundless or win is None:
            data = self._io(idx, win, buf, resampling)
            arr = gdal_cast(data, dtype)
            if masked or fill_value is not None:
                if all(f == MaskFlags.all_valid.value for f in self._mask_flags()):
                    mask: Any = np.ma.nomask
                else:
                    mask = ~self._masks_from(idx, data, win, buf).astype(bool)
                kwds: dict[str, Any] = {"mask": mask}
                if fill_value is not None:
                    kwds["fill_value"] = fill_value
                elif len(set(nodatavals)) == 1 and nodatavals[0] is not None:
                    kwds["fill_value"] = nodatavals[0]
                result: Any = np.ma.array(arr, **kwds)
                if not masked:
                    result = result.filled(fill_value)
            else:
                result = arr
        else:
            background = fill_value if fill_value is not None else fill
            data, inside = self._boundless(idx, win, buf, resampling, background)
            arr = gdal_cast(data, dtype)
            result = arr
            if masked:
                valid = inside.copy()
                flags = self._mask_flags()
                if all(f & MaskFlags.nodata.value for f in flags) and background is not None:
                    # rasterio reads through a VRT whose nodata is the (clamped) fill value.
                    if np.isnan(background):
                        valid &= ~np.isnan(data)
                    else:
                        valid &= data != np.array(background).astype(data.dtype)
                elif not all(f == MaskFlags.all_valid.value for f in flags):
                    valid &= self._masks_from(idx, data, None, buf).astype(bool)
                kwds = {"mask": ~valid}
                if fill_value is not None:
                    kwds["fill_value"] = fill_value
                elif len(set(nodatavals)) == 1 and nodatavals[0] is not None:
                    kwds["fill_value"] = nodatavals[0]
                result = np.ma.array(arr, **kwds)
        if out is not None and not isinstance(result, np.ma.MaskedArray):
            out[...] = result
            result = out
        if return2d:
            result = result[0]
        return result

    def _boundless(
        self,
        idx: list[int],
        win: Window,
        buf: tuple[int, int],
        resampling: Resampling,
        background: Any,
    ) -> tuple[np.ndarray, np.ndarray]:
        """Reads a window that may extend past the raster.

        Args:
            idx: The bands.
            win: The window.
            buf: The output `(height, width)`.
            resampling: The resampling method.
            background: The value of pixels outside the raster (None for 0).

        Returns:
            tuple: The pixels `(bands, h, w)` and where they are inside the raster.
        """
        bh, bw = buf
        sx, sy = win.width / bw if bw else 1.0, win.height / bh if bh else 1.0
        # Output pixels whose centres fall inside the raster.
        cx = win.col_off + (np.arange(bw) + 0.5) * sx
        cy = win.row_off + (np.arange(bh) + 0.5) * sy
        inx = (cx >= 0) & (cx < self.width)
        iny = (cy >= 0) & (cy < self.height)
        out = np.full((len(idx), bh, bw), background if background is not None else 0, self._dtype)
        inside = np.zeros((len(idx), bh, bw), bool)
        if inx.any() and iny.any():
            c0, c1 = int(np.argmax(inx)), int(len(inx) - np.argmax(inx[::-1]))
            r0, r1 = int(np.argmax(iny)), int(len(iny) - np.argmax(iny[::-1]))
            sub = Window(
                win.col_off + c0 * sx, win.row_off + r0 * sy, (c1 - c0) * sx, (r1 - r0) * sy
            ).crop(self.height, self.width)
            part = self._io(idx, sub, (r1 - r0, c1 - c0), resampling)
            out[:, r0:r1, c0:c1] = part
            inside[:, r0:r1, c0:c1] = True
        return out, inside

    def read_masks(
        self,
        indexes: Any = None,
        out: np.ndarray | None = None,
        out_shape: Any = None,
        window: Any = None,
        boundless: bool = False,
        resampling: Resampling = Resampling.nearest,
    ) -> np.ndarray:
        """Reads band masks: 0 where pixels are invalid, 255 where valid.

        Args:
            indexes: A band, a sequence of bands, or None for all.
            out: An array to read into (uint8).
            out_shape: The output shape.
            window: The window.
            boundless: Let the window extend past the raster (outside is invalid).
            resampling: The resampling method.

        Returns:
            numpy.ndarray: The masks, uint8.

        Raises:
            ValueError: If `out` and `out_shape` are both given or do not fit.
        """
        self._readable()
        idx, return2d, win, win_shape = self._prepare(indexes, window, boundless)
        if return2d and out is not None and out.ndim == 2:
            out = out[np.newaxis]
        if out is not None and out_shape is not None:
            raise ValueError("out and out_shape are exclusive")
        if out is not None:
            if out.dtype != np.uint8:
                raise ValueError(f"the out array's dtype '{out.dtype}' does not match 'uint8'")
            buf = (out.shape[-2], out.shape[-1])
        elif out_shape is not None:
            buf = (int(out_shape[-2]), int(out_shape[-1]))
        else:
            buf = win_shape
        if not boundless or win is None:
            masks = self._masks_from(idx, None, win, buf)
        else:
            data, inside = self._boundless(idx, win, buf, resampling, None)
            masks = self._masks_from(idx, data, None, buf)
            masks[~inside] = 0
        if out is not None:
            out[...] = masks
            masks = out
        return masks[0] if return2d else masks

    def dataset_mask(
        self,
        out: np.ndarray | None = None,
        out_shape: Any = None,
        window: Any = None,
        boundless: bool = False,
        resampling: Resampling = Resampling.nearest,
    ) -> np.ndarray:
        """The dataset's validity mask: 255 where any band is valid.

        Args:
            out: An array to read into.
            out_shape: The output shape.
            window: The window.
            boundless: Let the window extend past the raster.
            resampling: The resampling method.

        Returns:
            numpy.ndarray: The 2-D mask, uint8.
        """
        kwargs: dict[str, Any] = {
            "out": out,
            "out_shape": out_shape,
            "window": window,
            "boundless": boundless,
            "resampling": resampling,
        }
        if MaskFlags.per_dataset in self.mask_flag_enums[0]:
            return self.read_masks(1, **kwargs)
        if self.count == 4 and self.colorinterp[0] == ColorInterp.red:
            return self.read_masks(4, **kwargs)
        if out is not None:
            kwargs.pop("out")
            kwargs["out_shape"] = (self.count, out.shape[-2], out.shape[-1])
            out[...] = np.logical_or.reduce(self.read_masks(**kwargs)) * np.uint8(255)
            return out
        if out_shape is not None:
            kwargs["out_shape"] = (self.count, out_shape[-2], out_shape[-1])
        mask: np.ndarray = np.logical_or.reduce(self.read_masks(**kwargs)) * np.uint8(255)
        return mask

    def sample(self, xy: Any, indexes: Any = None, masked: bool = False) -> Iterator[np.ndarray]:
        """Samples the bands at points.

        Args:
            xy: An iterable of `(x, y)` points in the dataset's CRS.
            indexes: A band, a sequence of bands, or None for all.
            masked: Return masked arrays.

        Returns:
            iterator: One array of band values per point.
        """
        from terrio.rio.sample import sample_gen

        return sample_gen(self, xy, indexes=indexes, masked=masked)

    def statistics(self, bidx: int, approx: bool = False, clear_cache: bool = False) -> Any:
        """Statistics of a band's valid pixels.

        Args:
            bidx: The band.
            approx: Unused: statistics are exact.
            clear_cache: Unused.

        Returns:
            Statistics: `min`, `max`, `mean` and `std`.
        """
        return self.stats(indexes=[bidx])[0]

    def stats(self, *, indexes: Any = None, approx: bool = False) -> list[Any]:
        """Statistics of bands' valid pixels.

        Args:
            indexes: A band, a sequence of bands, or None for all.
            approx: Unused: statistics are exact.

        Returns:
            list: One `Statistics` per band.
        """
        if isinstance(indexes, int):
            indexes = [indexes]
        out = []
        for b in indexes or self.indexes:
            a = self.read(b, masked=True).astype("float64")
            vals = a.compressed()
            if vals.dtype.kind == "f":
                vals = vals[~np.isnan(vals)]
            out.append(
                Statistics(
                    float(vals.min()), float(vals.max()), float(vals.mean()), float(vals.std())
                )
            )
        return out


class Statistics:
    """Band statistics: minimum, maximum, mean and standard deviation."""

    def __init__(self, min: float, max: float, mean: float, std: float) -> None:
        """Creates the statistics.

        Args:
            min: The minimum.
            max: The maximum.
            mean: The mean.
            std: The population standard deviation.
        """
        self.min, self.max, self.mean, self.std = min, max, mean, std

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `Statistics(min=..., max=..., mean=..., std=...)`.
        """
        return f"Statistics(min={self.min}, max={self.max}, mean={self.mean}, std={self.std})"


class DatasetWriter(DatasetReader):
    """A dataset opened for writing (`w`, `w+`) or updating (`r+`).

    The pixels are held in memory; the file is written when the dataset is closed.
    """

    def __init__(
        self,
        path: Any,
        mode: str = "w",
        driver: str | None = None,
        width: Any = None,
        height: Any = None,
        count: Any = None,
        crs: Any = None,
        transform: Any = None,
        dtype: Any = None,
        nodata: Any = None,
        **kwargs: Any,
    ) -> None:
        """Creates (or opens for update) a dataset.

        Args:
            path: The output path.
            mode: `w`, `w+` or `r+`.
            driver: The format's short name, such as `GTiff` or `COG`.
            width: The number of columns.
            height: The number of rows.
            count: The number of bands.
            crs: The CRS.
            transform: The affine transform.
            dtype: The data type.
            nodata: The nodata value.
            **kwargs: Creation options: `tiled`, `blockxsize`, `blockysize`, `compress`,
                `interleave`, `photometric`, `alpha`, `bigtiff`... (others are accepted and
                ignored when terrio has no use for them).

        Raises:
            TypeError: If the driver, size, count or dtype is missing or invalid.
            ValueError: If the nodata value does not fit the data type.
            RasterBlockError: If tile sizes are not multiples of 16.
        """
        DatasetBase.__init__(self)
        self.name = os.fspath(path) if not isinstance(path, str) else path
        self.mode = mode
        self._path = _terrio_path(self.name)
        self._raster = None
        self._dirty = False
        self._colorinterp_set = False
        self._overview_factors: list[int] = []
        self._overview_resampling = "nearest"
        self._mask: np.ndarray | None = None
        self._mask_in_sidecar = False
        self._spill: list[str] = []
        kwargs = {k.lower(): v for k, v in kwargs.items()}
        kwargs.pop("sharing", None)
        kwargs.pop("thread_safe", None)
        gcps = kwargs.pop("gcps", None)
        rpcs = kwargs.pop("rpcs", None)
        if mode in ("w", "w+"):
            if not isinstance(driver, str):
                raise TypeError("A driver name string is required.")
            try:
                width, height = int(width), int(height)
            except (TypeError, ValueError):
                raise TypeError("Integer width and height are required.") from None
            try:
                count = int(count)
            except (TypeError, ValueError):
                raise TypeError("Integer band count is required.") from None
            try:
                assert dtype is not None
                dt = _getnpdtype(dtype).name
            except Exception:
                raise TypeError("A valid dtype is required.") from None
            if dt not in _SUPPORTED_DTYPES:
                raise TypeError(f"terrio cannot write {dt} rasters")
            if nodata is not None and not in_dtype_range(nodata, dt):
                raise ValueError(
                    f"Given nodata value, {nodata}, is beyond the valid range of its data type, "
                    f"{dt}."
                )
            self.driver = "GTiff" if driver.upper() == "COG" else driver
            self._out_driver = driver
            self.width, self.height, self._count, self._dtype = width, height, count, dt
            self._nodata = None if nodata is None else float(nodata)
            self._options = self._creation_options(kwargs)
            fill = self._nodata if self._nodata is not None else 0
            with np.errstate(invalid="ignore"):
                self._data = self._allocate((count, height, width), dt, fill)
            self._structure = self._new_structure()
            if transform is not None:
                self._transform = guard_transform(transform)
            if crs is not None:
                self._crs = CRS.from_user_input(crs)
            self._dirty = True
        elif mode == "r+":
            self._open_update(kwargs)
        else:
            raise ValueError(f"Invalid mode: '{mode}'")
        if gcps:
            self._set_gcps(gcps, crs)
        if rpcs:
            self._set_rpcs(rpcs)

    def _streams(self, shape: tuple[int, ...], dtype: Any) -> bool:
        """Whether pixels of this shape are kept on disk and written in rows of blocks.

        Args:
            shape: The array's shape.
            dtype: Its data type.

        Returns:
            bool: True for GeoTIFFs larger than the in-memory limit.
        """
        size = int(np.prod(shape)) * np.dtype(dtype).itemsize
        return self.driver == "GTiff" and size > _in_memory_limit()

    def _allocate(self, shape: tuple[int, ...], dtype: Any, fill: Any) -> np.ndarray:
        """An array for pixels, in memory or mapped from a temporary file.

        The file, beside the output, is used when the array is larger than the in-memory limit.

        Args:
            shape: The shape.
            dtype: The data type.
            fill: The initial value.

        Returns:
            numpy.ndarray: The array.
        """
        if not self._streams(shape, dtype):
            return np.full(shape, fill, dtype=dtype)
        spill = self._path + f".terrio-{uuid.uuid4().hex}.tmp"
        self._spill.append(spill)
        arr = np.memmap(spill, dtype=dtype, mode="w+", shape=shape)
        if fill != 0:
            for y, n in self._slabs():
                arr[..., y : y + n, :] = fill
        return arr

    def _slabs(self) -> Iterator[tuple[int, int]]:
        """Rows of blocks of about 16 MB.

        Yields:
            tuple: `(first row, number of rows)`.
        """
        row = max(1, self.width * max(self._count, 1) * np.dtype(self._dtype).itemsize)
        n = max(1, (16 << 20) // row)
        block = self.structure.block_shape[0] if self._structure is not None else 1
        n = max(block, n - n % max(block, 1))
        for y in range(0, self.height, n):
            yield y, min(n, self.height - y)

    def _release_spill(self) -> None:
        """Deletes the temporary files of file-backed arrays."""
        data = getattr(self, "_data", None)
        if isinstance(data, np.memmap):
            data._mmap.close()  # type: ignore[attr-defined]
        if isinstance(self._mask, np.memmap):
            self._mask._mmap.close()  # type: ignore[attr-defined]
        self._data = np.empty((0, 0, 0), self._dtype)
        self._mask = None
        for f in self._spill:
            try:
                os.remove(f)
            except OSError:
                pass
        self._spill = []

    def _creation_options(self, kwargs: dict[str, Any]) -> dict[str, str]:
        """Checks creation options and fills in GDAL's defaults for the GeoTIFF layout.

        Args:
            kwargs: Creation options, lower case.

        Returns:
            dict: The options terrio's writer takes, upper case.

        Raises:
            RasterBlockError: If tile sizes are not multiples of 16.
        """
        opts = {k.upper(): v for k, v in kwargs.items() if v is not None}
        tiled = opts.pop("TILED", False)
        if isinstance(tiled, str):
            tiled = tiled.lower() in ("true", "yes", "on", "1")
        if self._out_driver.upper() == "COG":
            tiled = True
        out: dict[str, str] = {}
        for k, v in opts.items():
            if isinstance(v, bool):
                v = "YES" if v else "NO"
            out[k] = str(v.value if hasattr(v, "value") else v)
        if self._out_driver.upper() == "GTIFF":
            if tiled:
                bx = int(out.get("BLOCKXSIZE", 256)) if "BLOCKYSIZE" in out else 256
                by = int(out.get("BLOCKYSIZE", 256)) if "BLOCKXSIZE" in out else 256
                if bx % 16 or by % 16:
                    raise RasterBlockError(
                        "The height and width of TIFF dataset blocks must be multiples of 16"
                    )
                out.update(TILED="YES", BLOCKXSIZE=str(bx), BLOCKYSIZE=str(by))
            else:
                out.pop("BLOCKXSIZE", None)
                interleave = out.get("INTERLEAVE", "PIXEL").upper()
                spp = 1 if interleave == "BAND" else self._count
                if "BLOCKYSIZE" in out:
                    rows = int(out["BLOCKYSIZE"])
                else:
                    line = self.width * np.dtype(self._dtype).itemsize * spp
                    rows = max(1, 8192 // max(line, 1))
                out["BLOCKYSIZE"] = str(max(1, min(rows, self.height)))
            # GDAL's defaults: RGB for three or four Byte bands, the fourth as alpha.
            if "PHOTOMETRIC" not in out and self._dtype == "uint8" and self._count in (3, 4):
                out["PHOTOMETRIC"] = "RGB"
                if self._count == 4:
                    out.setdefault("ALPHA", "YES")
        return out

    def _new_structure(self) -> _tiff.Structure:
        """The structure of the dataset being created, as GDAL would report it.

        Returns:
            Structure: The structure.
        """
        n = self._count
        o = self._options
        tiled = o.get("TILED") == "YES" or self._out_driver.upper() == "COG"
        if self._out_driver.upper() == "COG":
            b = int(o.get("BLOCKSIZE", 512))
            block = (b, b)
        elif tiled:
            block = (int(o["BLOCKYSIZE"]), int(o["BLOCKXSIZE"]))
        elif "BLOCKYSIZE" in o:
            block = (int(o["BLOCKYSIZE"]), self.width)
        else:
            block = (1, self.width)
        s = _tiff.Structure(count=n, width=self.width, height=self.height, block_shape=block)
        s.tiled = tiled
        ci = [ColorInterp.gray] + [ColorInterp.undefined] * (n - 1)
        if o.get("PHOTOMETRIC", "").upper() == "RGB" and n >= 3:
            ci[:3] = [ColorInterp.red, ColorInterp.green, ColorInterp.blue]
        if o.get("ALPHA", "").upper() in ("YES", "NON-PREMULTIPLIED", "PREMULTIPLIED") and n > 1:
            ci[-1] = ColorInterp.alpha
        s.colorinterp = ci
        s.band_tags = [{} for _ in range(n)]
        s.descriptions = [None] * n
        s.scales = [1.0] * n
        s.offsets = [0.0] * n
        s.units = [None] * n
        comp = (o.get("COMPRESS") or ("LZW" if self._out_driver.upper() == "COG" else "")).upper()
        if comp == "ZIP":
            comp = "DEFLATE"
        s.compression = comp if comp and comp != "NONE" else None
        if self._out_driver.upper() in ("GTIFF", "COG"):
            s.interleave = (
                "BAND" if n == 1 or o.get("INTERLEAVE", "").upper() == "BAND" else "PIXEL"
            )
            ns = {"INTERLEAVE": s.interleave}
            if s.compression:
                ns["COMPRESSION"] = s.compression
                if o.get("PREDICTOR", "1") not in ("1", "NO", "NONE"):
                    ns["PREDICTOR"] = {"YES": "2"}.get(o["PREDICTOR"].upper(), o["PREDICTOR"])
            s.namespaces["IMAGE_STRUCTURE"] = ns
            s.tags = {"AREA_OR_POINT": "Area"}
        return s

    def _open_update(self, kwargs: dict[str, Any]) -> None:
        """Opens an existing dataset for update: reads it whole into memory.

        Args:
            kwargs: Unused open options.
        """
        DatasetReader.__init__(self, self.name)
        self.mode = "r+"
        raster = self._raster
        assert raster is not None
        try:
            shape = (self._count, self.height, self.width)
            if self._streams(shape, self._dtype):
                # Copied into a file-backed array, block row by block row.
                self._data = self._allocate(shape, self._dtype, 0)
                for y, n in self._slabs():
                    self._data[:, y : y + n, :] = np.asarray(
                        raster.read(window=(0, y, self.width, n))
                    )
            else:
                self._data = np.asarray(raster.read())
        except Exception as err:
            raise RasterioIOError(f"Read failed: {err}") from err
        self._out_driver = self.driver
        s = self.structure
        opts: dict[str, str] = {}
        if s.compression:
            opts["COMPRESS"] = s.compression
            predictor = s.namespaces.get("IMAGE_STRUCTURE", {}).get("PREDICTOR")
            if predictor:
                opts["PREDICTOR"] = predictor
        if s.tiled:
            opts.update(
                TILED="YES", BLOCKXSIZE=str(s.block_shape[1]), BLOCKYSIZE=str(s.block_shape[0])
            )
        else:
            opts["BLOCKYSIZE"] = str(s.block_shape[0])
        if s.interleave == "BAND" and self._count > 1:
            opts["INTERLEAVE"] = "BAND"
        if s.colorinterp[:3] == [ColorInterp.red, ColorInterp.green, ColorInterp.blue]:
            opts["PHOTOMETRIC"] = "RGB"
        if self.structure.alpha_band() is not None:
            opts["ALPHA"] = "YES"
        self._options = opts
        self._overview_factors = DatasetBase.overviews(self, 1)
        if s.has_mask:
            m = raster.read_mask()
            if m is not None:
                self._mask = np.asarray(m)
                # A mask read from a `.msk` sidecar stays there, as GDAL keeps it.
                self._mask_in_sidecar = s.mask_in_sidecar
        self._raster = None
        self._overview_sizes = []

    # Reading back what was written.

    def _read_level(
        self, bands: Sequence[int], window: tuple[int, int, int, int], level: int | None
    ) -> np.ndarray:
        """Reads an integer window of the pixels in memory.

        Args:
            bands: The bands, from 1.
            window: `(x, y, w, h)`.
            level: Unused: the dataset in memory has no overviews.

        Returns:
            numpy.ndarray: `(bands, h, w)`.
        """
        self._check_open()
        x, y, w, h = window
        return self._data[[b - 1 for b in bands], y : y + h, x : x + w]

    def _masks_from(
        self, indexes: Sequence[int], data: np.ndarray | None, window: Window | None, buf: Any
    ) -> np.ndarray:
        """Band masks, including a mask written with `write_mask`.

        Args:
            indexes: The bands.
            data: The bands' pixels, or None.
            window: The window.
            buf: The output shape.

        Returns:
            numpy.ndarray: The masks.
        """
        mask = self._mask
        if mask is not None:
            from terrio.rio._resample import raster_io as rio_

            m = rio_(
                lambda b, w, lv: mask[np.newaxis, w[1] : w[1] + w[3], w[0] : w[0] + w[2]],
                [1],
                (self.width, self.height),
                [],
                (0, 0, self.width, self.height)
                if window is None
                else (window.col_off, window.row_off, window.width, window.height),
                buf,
                0,
                None,
            )
            return np.repeat(m, len(indexes), axis=0)
        return super()._masks_from(indexes, data, window, buf)

    def _mask_flags(self) -> tuple[int, ...]:
        """Mask flags, per-dataset once a mask is written.

        Returns:
            tuple: The flags.
        """
        if self._mask is not None:
            return tuple(MaskFlags.per_dataset.value for _ in self.indexes)
        return super()._mask_flags()

    # Setting properties.

    def _set_crs(self, value: Any) -> None:
        """Sets the CRS.

        Args:
            value: The CRS.
        """
        self._crs = CRS.from_user_input(value)
        self._dirty = True

    def write_transform(self, value: Any) -> None:
        """Sets the transform.

        Args:
            value: An `Affine` or affine coefficients.
        """
        t = value if isinstance(value, Affine) else Affine(*value[:6])
        if [abs(v) for v in t[:6]] == [0, 1, 0, 0, 0, 1]:
            warnings.warn(
                "The given matrix is equal to Affine.identity or its flipped counterpart. GDAL "
                "may ignore this matrix and save no geotransform without raising an error. This "
                "behavior is somewhat driver-specific.",
                NotGeoreferencedWarning,
                stacklevel=2,
            )
        self._transform = t
        self._dirty = True

    def _set_nodatavals(self, vals: Sequence[float | None]) -> None:
        """Sets the nodata values of the bands.

        GeoTIFFs hold one nodata value for all bands (GDAL's `GDAL_NODATA` tag): when the bands'
        values differ, the last band's is written, as GDAL does, with GDAL's warning.

        Args:
            vals: One value per band.
        """
        vals = [None if v is None else float(v) for v in vals]
        if len(set(map(repr, vals))) > 1:
            self._nodatavals = list(vals)
            self._nodata = vals[-1]
        else:
            self._nodatavals = None
            self._nodata = vals[0] if vals else None
        self._dirty = True

    def _set_gcps(self, gcps: Any, crs: Any = None) -> None:
        """Sets ground control points, written as GeoTIFF tie points instead of a transform.

        Args:
            gcps: `GroundControlPoint`s.
            crs: Their CRS (by default, the dataset's).
        """
        self._gcps = list(gcps)
        self._gcp_crs = CRS.from_user_input(crs) if crs is not None else self._crs
        self._dirty = True

    def _set_rpcs(self, value: Any) -> None:
        """Sets rational polynomial coefficients, written to the GeoTIFF RPCCoefficientTag.

        Args:
            value: An `RPC`, or GDAL's RPC items.
        """
        from terrio.rio.rpc import RPC

        self._rpcs = value if isinstance(value, RPC) else RPC.from_gdal(dict(value))
        self._dirty = True

    def _set_all(self, attr: str, value: Sequence[Any], what: str) -> None:
        """Sets a per-band attribute.

        Args:
            attr: The structure attribute.
            value: One value per band.
            what: The attribute's name in messages.

        Raises:
            ValueError: If the number of values is wrong.
        """
        if len(value) != self._count:
            raise ValueError(f"One {what} for each band is required")
        if attr in ("descriptions", "units"):
            value = [v or None for v in value]
        setattr(self.structure, attr, list(value))
        self._dirty = True

    def set_band_description(self, bidx: int, value: str | None) -> None:
        """Sets a band's description.

        Args:
            bidx: The band, from 1.
            value: The description.
        """
        self.structure.descriptions[bidx - 1] = value or None
        self._dirty = True

    def set_band_unit(self, bidx: int, value: str | None) -> None:
        """Sets a band's unit.

        Args:
            bidx: The band, from 1.
            value: The unit.
        """
        self.structure.units[bidx - 1] = value or None
        self._dirty = True

    def update_tags(self, bidx: int = 0, ns: str | None = None, **kwargs: Any) -> None:
        """Adds or replaces metadata items of the dataset or a band.

        Args:
            bidx: The band, from 1, or 0 for the dataset.
            ns: The domain; only the default domain is written.
            **kwargs: Items.
        """
        items = {str(k): str(v) for k, v in kwargs.items()}
        s = self.structure
        if ns:
            s.namespaces.setdefault(ns, {}).update(items)
        elif bidx > 0:
            s.band_tags[bidx - 1].update(items)
        else:
            s.tags.update(items)
        self._dirty = True

    def write_colormap(self, bidx: int, colormap: dict[int, Sequence[int]]) -> None:
        """Sets a band's color table.

        Args:
            bidx: The band (1).
            colormap: Index to `(r, g, b)` or `(r, g, b, a)`.

        Raises:
            ValueError: If the band is not the first one.
        """
        if bidx != 1:
            raise ValueError("terrio writes color tables of the first band only")
        table = {}
        for i, rgba in colormap.items():
            c = [int(v) for v in rgba] + [255]
            table[int(i)] = (c[0], c[1], c[2], c[3])
        self.structure.colormap = table
        self.structure.colorinterp[0] = ColorInterp.palette
        self._dirty = True

    def write_mask(self, mask_array: Any, window: Any = None) -> None:
        """Sets the dataset's validity mask, a per-dataset mask.

        GeoTIFFs save it as GDAL's internal mask (a 1-bit mask image).

        Args:
            mask_array: True/False, or a 2-D array (bool, or uint8 with 0 for invalid).
            window: The window to write.
        """
        if self._mask is None:
            self._mask = self._allocate((self.height, self.width), np.uint8, 255)
        if mask_array is True or mask_array is np.True_:
            vals: Any = 255
        elif mask_array is False or mask_array is np.False_:
            vals = 0
        else:
            arr = np.asarray(mask_array)
            vals = arr.astype(np.uint8) * 255 if arr.dtype == bool else arr
        if window:
            if isinstance(window, tuple):
                window = Window.from_slices(window[0], window[1], self.height, self.width)
            r, c = int(window.row_off), int(window.col_off)
            self._mask[r : r + int(window.height), c : c + int(window.width)] = vals
        else:
            self._mask[...] = vals
        self._dirty = True

    def write(
        self, arr: Any, indexes: Any = None, window: Any = None, masked: bool = False
    ) -> None:
        """Writes an array to bands.

        Args:
            arr: A 3-D array `(bands, rows, cols)`, or 2-D for a single band index.
            indexes: A band, a sequence of bands, or None for all.
            window: Where to write: a `Window` or ranges; the whole raster by default.
            masked: For masked arrays, write the mask (else masked pixels are filled with
                nodata).

        Raises:
            ValueError: If the shape does not match the bands or the window.
            IndexError: If a band does not exist.
        """
        self._check_open()
        if not hasattr(arr, "__array__") and not isinstance(arr, np.ndarray):
            from terrio.rio.errors import InvalidArrayError

            raise InvalidArrayError("Positional argument arr must be an array-like object")
        if isinstance(arr, np.ma.MaskedArray):
            if masked:
                self.write_mask(~np.ma.getmaskarray(arr), window=window)
                arr = arr.data
            else:
                nd = self.nodatavals
                fill = nd[0] if len(set(nd)) == 1 and nd[0] is not None else None
                arr = arr.filled(fill)
        else:
            arr = np.asanyarray(arr)
        if indexes is None:
            idx = list(self.indexes)
        elif isinstance(indexes, (int, np.integer)):
            idx = [int(indexes)]
            arr = arr[np.newaxis]
        else:
            idx = [int(i) for i in indexes]
        if arr.ndim != 3 or arr.shape[0] != len(idx):
            raise ValueError(
                f"Source shape {arr.shape} is inconsistent with given indexes {len(idx)}"
            )
        for b in idx:
            if b not in self.indexes:
                raise IndexError(f"band index {b} out of range (not in {self.indexes})")
        arr = np.require(arr, dtype=self._dtype)
        if window:
            if isinstance(window, tuple):
                window = Window.from_slices(window[0], window[1], self.height, self.width)
            x, y = int(window.col_off), int(window.row_off)
            w, h = int(window.width), int(window.height)
        else:
            x, y, w, h = 0, 0, self.width, self.height
        if x < 0 or y < 0 or x + w > self.width or y + h > self.height:
            raise RasterioIOError("Write failed: the window is outside the raster")
        if arr.shape[1:] != (h, w):
            if arr.shape[1] < h or arr.shape[2] < w:
                raise ValueError(f"Source shape {arr.shape} does not match the window {h}x{w}")
            arr = arr[:, :h, :w]
        self._data[[b - 1 for b in idx], y : y + h, x : x + w] = arr
        self._dirty = True

    def write_band(self, bidx: int, src: Any, window: Any = None) -> None:
        """Writes a 2-D array to a band.

        Args:
            bidx: The band.
            src: The array.
            window: Where to write.
        """
        self.write(src, bidx, window=window)

    def build_overviews(
        self, factors: Sequence[int], resampling: Resampling = Resampling.nearest
    ) -> None:
        """Builds overviews, written with the file.

        Args:
            factors: Decimation factors, such as `[2, 4, 8]`.
            resampling: `nearest`, `average`, `mode`, `min` or `max`.

        Raises:
            ValueError: If the resampling method is not supported.
            OverviewCreationError: If more than one overview would be 1x1.
        """
        try:
            name = _OVERVIEW_RESAMPLING[Resampling(resampling)]
        except (KeyError, ValueError):
            raise ValueError(
                "resampling must be one of: "
                + ", ".join(f"Resampling.{r.name}" for r in _OVERVIEW_RESAMPLING)
            ) from None
        shapes = [(-(-self.height // f), -(-self.width // f)) for f in factors]
        if shapes.count((1, 1)) > 1:
            raise OverviewCreationError("Too many overviews levels of 1x1 dimension were requested")
        self._overview_factors = sorted(set(int(f) for f in factors))
        self._overview_resampling = name
        self._overview_sizes = [
            (-(-self.width // f), -(-self.height // f)) for f in self._overview_factors
        ]
        self._dirty = True

    def overviews(self, bidx: int) -> list[int]:
        """The decimation factors of the overviews.

        Args:
            bidx: The band.

        Returns:
            list: The factors.
        """
        if self.mode in ("w", "w+", "r+") and hasattr(self, "_data"):
            return list(self._overview_factors)
        return super().overviews(bidx)

    def update_stats(self, *, stats: Any = None, indexes: Any = None, approx: bool = False) -> None:
        """Accepted for compatibility: terrio does not store statistics.

        Args:
            stats: Unused.
            indexes: Unused.
            approx: Unused.
        """

    def clear_stats(self) -> None:
        """Accepted for compatibility: terrio does not store statistics."""

    # Closing.

    def close(self) -> None:
        """Writes the file (when anything changed) and closes the dataset."""
        if self._closed:
            return
        try:
            if self._dirty and self.mode in ("w", "w+", "r+"):
                self._flush()
        finally:
            self._release_spill()
            DatasetBase.close(self)

    def _flush(self) -> None:
        """Writes the dataset to its file.

        Raises:
            RasterioIOError: If the file cannot be written.
        """
        s = self.structure
        t = self._transform
        georef = not (t == Affine.identity() or [abs(v) for v in t[:6]] == [0, 1, 0, 0, 0, 1])
        gt = t.to_gdal() if georef else (0.0, 1.0, 0.0, float(self.height), 0.0, -1.0)
        tags = {k: v for k, v in s.tags.items() if k != "AREA_OR_POINT"}
        crs_obj = self._crs if self._crs is not None else (self._gcp_crs if self._gcps else None)
        crs = crs_obj.to_wkt() if crs_obj else None
        if self._nodatavals is not None and len(set(map(repr, self._nodatavals))) > 1:
            warnings.warn(
                f"The TIFFTAG_GDAL_NODATA only support one value per dataset. This value of "
                f"{self._nodata} will be used for all bands on re-opening",
                stacklevel=3,
            )
        streaming = (
            isinstance(self._data, np.memmap)
            and self._out_driver == "GTiff"
            and not self._overview_factors
            and str(self._options.get("COMPRESS", "NONE")).upper()
            in ("NONE", "LZW", "DEFLATE", "ZIP", "ZSTD", "PACKBITS")
        )
        raster: Any = None
        if not streaming:
            try:
                raster = _terrio.Raster.from_array(
                    np.asarray(self._data), gt, crs or None, self._nodata, tags
                )
            except Exception as err:
                raise RasterioIOError(f"Write failed: {err}") from err
        driver = self._out_driver
        opts = dict(self._options)
        compress = opts.pop("COMPRESS", None)
        band_md: list[dict[str, Any]] = []
        defaults = self._new_structure_defaults()
        for b in range(self._count):
            md: dict[str, Any] = {}
            if s.descriptions[b]:
                md["description"] = s.descriptions[b]
            if s.scales[b] != 1.0:
                md["scale"] = float(s.scales[b])
            if s.offsets[b] != 0.0:
                md["offset"] = float(s.offsets[b])
            if s.units[b]:
                md["unit"] = s.units[b]
            if s.band_tags[b]:
                md["items"] = dict(s.band_tags[b])
            ci = s.colorinterp[b]
            if ci != defaults[b] and not (ci == ColorInterp.palette and s.colormap):
                md["color_interp"] = _tiff.COLOR_INTERP_NAMES.get(ci, "Undefined")
            band_md.append(md)
        colormap: list[tuple[int, int, int, int]] | None = None
        if s.colormap:
            n = 65536 if self._dtype == "uint16" else 256
            colormap = [
                s.colormap.get(i, (0, 0, 0, 255)) for i in range(min(n, max(s.colormap) + 1))
            ]
        if not georef:
            opts["GEOREFERENCED"] = "NO"
        extra: dict[str, Any] = {}
        if self._gcps:
            extra["gcps"] = [
                (float(g.col), float(g.row), float(g.x), float(g.y), float(g.z or 0.0))
                for g in self._gcps
            ]
        if self._rpcs is not None:
            extra["rpc"] = self._rpcs.to_tiff_tag()
        # GDAL writes a mask into the GeoTIFF, unless GDAL_TIFF_INTERNAL_MASK says no or the
        # format has no internal masks: then into a `.msk` sidecar.
        # On update, a mask stays where it was (GDAL keeps an internal mask internal).
        sidecar_mask = self._mask is not None and (
            self._mask_in_sidecar
            or driver not in ("GTiff", "COG")
            or (
                self.mode == "w"
                and os.environ.get("GDAL_TIFF_INTERNAL_MASK", "YES").strip().upper()
                in ("NO", "FALSE", "OFF", "0")
            )
        )
        if self._mask is not None and not streaming and not sidecar_mask:
            extra["mask"] = np.ascontiguousarray(self._mask, dtype=np.uint8)
        tmp = self._path + f".tmp-{uuid.uuid4().hex}"
        target = tmp if self.mode == "r+" else self._path
        # The sidecar mask is written first beside its place, then both files are put in place
        # once the dataset is written: a failure while writing leaves the dataset and its mask as
        # they were (only between the two renames could the dataset be new and its mask old).
        msk_tmp: str | None = None
        if streaming:
            try:
                msk_tmp = self._write_mask_sidecar() if sidecar_mask else None
                self._write_streaming(
                    target, gt, crs, tags, compress, opts, band_md, colormap, extra, sidecar_mask
                )
                self._put_in_place(target, msk_tmp)
            except Exception as err:
                for leftover in (tmp, msk_tmp):
                    if leftover and os.path.exists(leftover):
                        os.remove(leftover)
                raise RasterioIOError(f"Write failed: {err}") from err
            self._dirty = False
            return
        try:
            msk_tmp = self._write_mask_sidecar() if sidecar_mask else None
            raster.write(
                target,
                driver=driver,
                compress=compress,
                options=opts,
                band_metadata=band_md if any(band_md) else None,
                colormap=colormap,
                overviews=self._overview_factors or None,
                overview_resampling=self._overview_resampling,
                **extra,
            )
            self._put_in_place(target, msk_tmp)
        except Exception as err:
            for leftover in (tmp, msk_tmp):
                if leftover and os.path.exists(leftover):
                    os.remove(leftover)
            raise RasterioIOError(f"Write failed: {err}") from err
        self._dirty = False

    def _put_in_place(self, target: str, msk_tmp: str | None) -> None:
        """Moves a written dataset, and its sidecar mask, to their paths.

        Args:
            target: Where the dataset was written (its path, or a temporary file beside it).
            msk_tmp: The sidecar mask written beside its path, or None (a sidecar left by an
                earlier dataset is then removed, when one is created).
        """
        if target != self._path:
            os.replace(target, self._path)
        if msk_tmp is not None:
            os.replace(msk_tmp, self._path + ".msk")
        else:
            self._remove_stale_mask_sidecar()

    def _write_mask_sidecar(self) -> str:
        """Writes the mask as GDAL's `.msk` sidecar of the dataset, beside its path.

        The sidecar is a tiled, Deflate-compressed GeoTIFF without georeferencing, its band the
        per-dataset mask (0 or 255) flagged for every band by `INTERNAL_MASK_FLAGS_<n>=2`, with
        the dataset's overviews, as GDAL writes it; a mask held in a file-backed array is written
        in slabs of rows. The caller moves it into place.

        Returns:
            str: The temporary file written.
        """
        assert self._mask is not None
        target = self._path + ".msk"
        tmp = target + f".tmp-{uuid.uuid4().hex}"
        flags = {f"INTERNAL_MASK_FLAGS_{b}": "2" for b in range(1, self._count + 1)}
        opts = {"TILED": "YES", "GEOREFERENCED": "NO"}
        gt = (0.0, 1.0, 0.0, 0.0, 0.0, 1.0)
        try:
            if isinstance(self._mask, np.memmap) and not self._overview_factors:
                w = _terrio.GeoTiffWriter(
                    tmp,
                    self.width,
                    self.height,
                    1,
                    "uint8",
                    gt,
                    None,
                    None,
                    flags,
                    compress="DEFLATE",
                    options=opts,
                )
                try:
                    n = max(1, (16 << 20) // max(1, self.width))
                    for y in range(0, self.height, n):
                        part = np.asarray(self._mask[y : y + n])
                        w.write(np.where(part != 0, np.uint8(255), np.uint8(0))[np.newaxis])
                except BaseException:
                    with contextlib.suppress(Exception):
                        w.close()
                    raise
                w.close()
            else:
                mask = np.where(np.asarray(self._mask) != 0, np.uint8(255), np.uint8(0))
                m = _terrio.Raster.from_array(mask[np.newaxis], gt, None, None, flags)
                m.write(
                    tmp,
                    driver="GTiff",
                    compress="DEFLATE",
                    options=opts,
                    overviews=self._overview_factors or None,
                    overview_resampling="nearest",
                )
        except BaseException:
            if os.path.exists(tmp):
                os.remove(tmp)
            raise
        return tmp

    def _remove_stale_mask_sidecar(self) -> None:
        """Removes the `.msk` sidecar of an earlier dataset at the path, when one is created.

        Only a dataset created (mode `w`) replaces what was there; an `r+` update leaves a sidecar
        it did not load (per-band masks, another size) as it is.
        """
        stale = self._path + ".msk"
        if self.mode == "w" and os.path.isfile(stale):
            os.remove(stale)

    def _write_streaming(
        self,
        target: str,
        gt: Sequence[float],
        crs: str | None,
        tags: dict[str, str],
        compress: str | None,
        opts: dict[str, str],
        band_md: list[dict[str, Any]],
        colormap: list[tuple[int, int, int, int]] | None,
        extra: dict[str, Any],
        sidecar_mask: bool = False,
    ) -> None:
        """Writes a GeoTIFF in rows of blocks from the file-backed pixels.

        Args:
            target: The output path.
            gt: The geotransform.
            crs: The CRS as WKT.
            tags: Dataset metadata.
            compress: The compression.
            opts: Creation options.
            band_md: Band metadata.
            colormap: The color table.
            extra: GCPs and RPCs (the mask is taken from the dataset).
            sidecar_mask: Whether the mask goes to a `.msk` sidecar rather than the file.
        """
        w = _terrio.GeoTiffWriter(
            target,
            self.width,
            self.height,
            self._count,
            self._dtype,
            tuple(gt),
            crs or None,
            self._nodata,
            tags,
            compress=compress,
            options=opts,
            band_metadata=band_md if any(band_md) else None,
            colormap=colormap,
            gcps=extra.get("gcps"),
            rpc=extra.get("rpc"),
            mask=self._mask is not None and not sidecar_mask,
        )
        try:
            for y, n in self._slabs():
                rows = np.ascontiguousarray(self._data[:, y : y + n, :])
                m = (
                    None
                    if self._mask is None or sidecar_mask
                    else np.ascontiguousarray(self._mask[y : y + n])
                )
                w.write(rows, m)
        except BaseException:
            # Closing an incomplete writer fails too ("N rows written of M"): keep the first
            # error, which says what went wrong.
            with contextlib.suppress(Exception):
                w.close()
            raise
        w.close()

    def _new_structure_defaults(self) -> list[ColorInterp]:
        """The color interpretations GDAL derives from the creation options.

        Returns:
            list: One per band.
        """
        n = self._count
        ci = [ColorInterp.gray] + [ColorInterp.undefined] * (n - 1)
        o = self._options
        if o.get("PHOTOMETRIC", "").upper() == "RGB" and n >= 3:
            ci[:3] = [ColorInterp.red, ColorInterp.green, ColorInterp.blue]
        if o.get("ALPHA", "").upper() in ("YES", "NON-PREMULTIPLIED", "PREMULTIPLIED") and n > 1:
            ci[-1] = ColorInterp.alpha
        return ci


class BufferedDatasetWriter(DatasetWriter):
    """A writer for formats written in one go: the same as `DatasetWriter` in terrio."""


def get_writer_for_driver(driver: str) -> type[DatasetWriter]:
    """The writer class for a driver.

    Args:
        driver: The driver's short name.

    Returns:
        type: The writer class.

    Raises:
        DriverCapabilityError: If terrio cannot write the format.
    """
    names = {d["name"].upper(): d for d in _terrio.drivers() if d.get("kind") == "raster"}
    d = names.get(driver.upper())
    if driver.upper() != "COG" and (d is None or not d.get("write", True)):
        raise DriverCapabilityError(f"Writer does not exist for driver: {driver}")
    return DatasetWriter


def get_writer_for_path(path: Any, driver: str | None = None) -> type[DatasetWriter]:
    """The writer class for a path.

    Args:
        path: The path.
        driver: The driver's short name.

    Returns:
        type: The writer class.
    """
    return DatasetWriter


class MemoryFile:
    """A file in memory: bytes to open as a dataset, or a dataset to write into bytes.

    Example::

        with MemoryFile(data) as memfile:
            with memfile.open() as src:
                array = src.read()
    """

    def __init__(
        self,
        file_or_bytes: Any = None,
        dirname: str | None = None,
        filename: str | None = None,
        ext: str = ".tif",
    ) -> None:
        """Creates the file.

        Args:
            file_or_bytes: Initial contents: bytes, or a file object to read them from.
            dirname: Unused.
            filename: The file's name (its extension tells the format).
            ext: The extension when no file name is given.
        """
        if file_or_bytes is not None and hasattr(file_or_bytes, "read"):
            file_or_bytes = file_or_bytes.read()
        self._dir = tempfile.mkdtemp(prefix="terrio-memfile-")
        name = (
            filename or f"{uuid.uuid4().hex}{ext if ext.startswith('.') or not ext else '.' + ext}"
        )
        self.name = os.path.join(self._dir, name)
        self._pos = 0
        self.closed = False
        if file_or_bytes is not None:
            with open(self.name, "wb") as f:
                f.write(bytes(file_or_bytes))

    def exists(self) -> bool:
        """Whether the file has contents.

        Returns:
            bool: True when written.
        """
        return os.path.exists(self.name)

    def __len__(self) -> int:
        """The size in bytes.

        Returns:
            int: The size.
        """
        return os.path.getsize(self.name) if self.exists() else 0

    def getbuffer(self) -> memoryview:
        """The contents.

        Returns:
            memoryview: The bytes.
        """
        with open(self.name, "rb") as f:
            return memoryview(f.read())

    def read(self, size: int = -1) -> bytes:
        """Reads bytes from the current position.

        Args:
            size: The number of bytes, or -1 for the rest.

        Returns:
            bytes: The bytes.
        """
        if not self.exists():
            return b""
        with open(self.name, "rb") as f:
            f.seek(self._pos)
            data = f.read(size)
        self._pos += len(data)
        return data

    def write(self, data: bytes) -> int:
        """Writes bytes at the current position.

        Args:
            data: The bytes.

        Returns:
            int: The number of bytes written.
        """
        mode = "r+b" if self.exists() else "wb"
        with open(self.name, mode) as f:
            f.seek(self._pos)
            f.write(data)
        self._pos += len(data)
        return len(data)

    def seek(self, offset: int, whence: int = 0) -> int:
        """Moves the position.

        Args:
            offset: The offset.
            whence: 0 (start), 1 (current) or 2 (end).

        Returns:
            int: The new position.
        """
        base = {0: 0, 1: self._pos, 2: len(self)}[whence]
        self._pos = base + offset
        return self._pos

    def tell(self) -> int:
        """The current position.

        Returns:
            int: The position.
        """
        return self._pos

    def open(
        self,
        driver: str | None = None,
        width: Any = None,
        height: Any = None,
        count: Any = None,
        crs: Any = None,
        transform: Any = None,
        dtype: Any = None,
        nodata: Any = None,
        sharing: bool = False,
        **kwargs: Any,
    ) -> DatasetReader:
        """Opens the file as a dataset: for reading when it has contents, else for writing.

        Args:
            driver: The format, for writing.
            width: The number of columns, for writing.
            height: The number of rows.
            count: The number of bands.
            crs: The CRS.
            transform: The transform.
            dtype: The data type.
            nodata: The nodata value.
            sharing: Unused.
            **kwargs: Creation options.

        Returns:
            DatasetReader: The dataset (a `DatasetWriter` when writing).

        Raises:
            OSError: If the file is closed.
        """
        if self.closed:
            raise OSError("I/O operation on closed file.")
        if self.exists() and len(self) > 0:
            return DatasetReader(self.name, driver=driver)
        return DatasetWriter(
            self.name,
            "w+",
            driver=driver or driver_from_extension(self.name),
            width=width,
            height=height,
            count=count,
            crs=crs,
            transform=transform,
            dtype=dtype,
            nodata=nodata,
            **kwargs,
        )

    def close(self) -> None:
        """Deletes the file."""
        if not self.closed:
            self.closed = True
            shutil.rmtree(self._dir, ignore_errors=True)

    def __enter__(self) -> MemoryFile:
        """Enters a `with` block.

        Returns:
            MemoryFile: The file.
        """
        return self

    def __exit__(self, *args: object) -> None:
        """Deletes the file.

        Args:
            *args: Exception details.
        """
        self.close()


class ZipMemoryFile(MemoryFile):
    """A zip archive in memory, whose members open as datasets."""

    def __init__(self, file_or_bytes: Any = None) -> None:
        """Creates the archive.

        Args:
            file_or_bytes: The archive's bytes.
        """
        super().__init__(file_or_bytes, ext=".zip")

    def open(  # type: ignore[override]
        self, path: str, driver: str | None = None, sharing: bool = False, **kwargs: Any
    ) -> DatasetReader:
        """Opens a member of the archive.

        Args:
            path: The member's path in the archive.
            driver: Unused.
            sharing: Unused.
            **kwargs: Unused.

        Returns:
            DatasetReader: The dataset.
        """
        return DatasetReader(f"{self.name}!{path.lstrip('/')}")


__all__ = [
    "BufferedDatasetWriter",
    "DatasetReader",
    "DatasetWriter",
    "MemoryFile",
    "Statistics",
    "ZipMemoryFile",
    "check_dtype",
    "driver_from_extension",
    "get_writer_for_driver",
    "get_writer_for_path",
]
