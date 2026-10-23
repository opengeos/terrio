"""`WarpedVRT`: a dataset reprojected on the fly, as rasterio's warped VRT."""

from __future__ import annotations

from typing import Any

import numpy as np
from affine import Affine

from terrio import _terrio
from terrio.rio._tiff import Structure
from terrio.rio.crs import CRS
from terrio.rio.enums import ColorInterp, Resampling
from terrio.rio.errors import WarpedVRTError
from terrio.rio.io import DatasetReader
from terrio.rio.warp import (
    _WARP_NAMES,
    _crs_text,
    _gcp_args,
    _rpc_args,
    calculate_default_transform,
)

# Warped grids up to this many pixels (all bands) are warped whole and cached.
_CACHE_PIXELS = 100_000_000


class WarpedVRT(DatasetReader):
    """A read-only view of a dataset reprojected into another CRS or grid.

    Pixels are warped with terrio's `warp` when they are read, window by window.
    """

    def __init__(
        self,
        src_dataset: Any,
        src_crs: Any = None,
        crs: Any = None,
        resampling: Resampling = Resampling.nearest,
        tolerance: float = 0.125,
        src_nodata: Any = None,
        nodata: Any = None,
        width: int | None = None,
        height: int | None = None,
        src_transform: Affine | None = None,
        transform: Affine | None = None,
        init_dest_nodata: bool = True,
        src_alpha: int = 0,
        add_alpha: bool = False,
        warp_mem_limit: int = 0,
        dtype: Any = None,
        **warp_extras: Any,
    ) -> None:
        """Creates the view.

        Args:
            src_dataset: The source dataset (a `DatasetReader`).
            src_crs: The source CRS, overriding the dataset's.
            crs: The output CRS; by default, the source's.
            resampling: The resampling method.
            tolerance: The approximate transformer's maximum error, in pixels.
            src_nodata: The source nodata value, overriding the dataset's.
            nodata: The output nodata value; by default, the source's.
            width: The output width (with `height`; with `transform`, the grid itself).
            height: The output height.
            src_transform: The source transform, overriding the dataset's.
            transform: The output transform.
            init_dest_nodata: Unused: pixels outside the source are nodata (or 0).
            src_alpha: Unsupported (0).
            add_alpha: Unsupported (False).
            warp_mem_limit: Unused.
            dtype: The output data type; by default, the source's.
            **warp_extras: GDAL warp and transformer options: a dataset without a transform is
                warped by its GCPs, else by its RPCs, as GDAL picks them (`SRC_METHOD="RPC"`
                forces the RPCs), with the `RPC_*` options (`RPC_HEIGHT`, `RPC_DEM`...) and
                `MAX_GCP_ORDER`; `SOURCE_EXTRA` and `SAMPLE_STEPS`. Others are unused.

        Raises:
            WarpedVRTError: If the options are not supported.
        """
        super(DatasetReader, self).__init__()
        if src_alpha or add_alpha:
            raise WarpedVRTError("terrio.rio does not support alpha bands in WarpedVRT")
        if (width is None) != (height is None):
            raise WarpedVRTError("width and height must be given together")
        self.src_dataset = src_dataset
        self.name = f"WarpedVRT({src_dataset.name})"
        self.mode = "r"
        self.driver = "VRT"
        self.resampling = Resampling(resampling)
        self.tolerance = tolerance
        self.src_crs = CRS.from_user_input(src_crs) if src_crs is not None else src_dataset.crs
        self.src_transform = src_transform or src_dataset.transform
        # A dataset without a transform: warped by its GCPs, else its RPCs, as GDAL picks them.
        opts = {str(k).upper(): v for k, v in warp_extras.items()}
        method = str(opts.get("SRC_METHOD", opts.get("METHOD", ""))).upper()
        self._georef: dict[str, Any] = {}
        if src_transform is None and not getattr(src_dataset, "_georeferenced", True):
            gcps, gcp_crs = src_dataset.gcps
            rpcs = src_dataset.rpcs
            if gcps and method in ("", "GCP_POLYNOMIAL", "GCP_TPS", "GCP_HOMOGRAPHY"):
                self.src_crs = self.src_crs or gcp_crs
                self._georef = _gcp_args(gcps, warp_extras)
            elif rpcs and method in ("", "RPC"):
                self._georef = _rpc_args(rpcs, warp_extras)
                if self.src_crs is None and gcps:
                    self.src_crs = gcp_crs
        self.dst_crs = CRS.from_user_input(crs) if crs is not None else self.src_crs
        if self.dst_crs is None and "rpc" in self._georef:
            self.dst_crs = CRS.from_epsg(4326)
        self.src_nodata = src_nodata if src_nodata is not None else src_dataset.nodata
        self.dst_nodata = nodata if nodata is not None else self.src_nodata
        self.working_dtype = src_dataset.dtypes[0]
        self.warp_extras = warp_extras
        if transform is not None and width is not None:
            t, w, h = transform, int(width), int(height)  # type: ignore[arg-type]
        else:
            kw: dict[str, Any] = {}
            if width is not None:
                kw = {"dst_width": width, "dst_height": height}
            if "rpc" in self._georef:
                t, w, h = calculate_default_transform(
                    self.src_crs,
                    self.dst_crs,
                    src_dataset.width,
                    src_dataset.height,
                    rpcs=src_dataset.rpcs,
                    **kw,
                    **{k: v for k, v in warp_extras.items() if str(k).upper().startswith("RPC_")},
                )
            elif "gcps" in self._georef:
                t, w, h = calculate_default_transform(
                    self.src_crs,
                    self.dst_crs,
                    src_dataset.width,
                    src_dataset.height,
                    gcps=src_dataset.gcps[0],
                    **kw,
                    **warp_extras,
                )
            else:
                left, bottom, right, top = src_dataset.bounds
                t, w, h = calculate_default_transform(
                    self.src_crs,
                    self.dst_crs,
                    src_dataset.width,
                    src_dataset.height,
                    left,
                    bottom,
                    right,
                    top,
                    **kw,
                )
            if transform is not None:
                t = transform
        self._transform = t
        self.width, self.height = int(w), int(h)
        self._count = src_dataset.count
        self._dtype = np.dtype(dtype).name if dtype is not None else src_dataset.dtypes[0]
        self._nodata = None if self.dst_nodata is None else float(self.dst_nodata)
        self._crs = self.dst_crs
        self._overview_sizes = []
        n = self._count
        s = Structure(
            count=n,
            width=self.width,
            height=self.height,
            block_shape=(min(128, self.height), min(512, self.width)),
        )
        s.tiled = True
        s.colorinterp = (
            list(src_dataset.colorinterp) if src_dataset.count == n else [ColorInterp.gray] * n
        )
        s.band_tags = [{} for _ in range(n)]
        s.descriptions = list(src_dataset.descriptions)
        s.scales = list(src_dataset.scales)
        s.offsets = list(src_dataset.offsets)
        s.units = list(src_dataset.units)
        s.tags = {"AREA_OR_POINT": "Area"}
        self._structure = s
        self._source: Any = None
        self._full: np.ndarray | None = None

    def _src_raster(self) -> Any:
        """The whole source in memory, read on first use.

        Returns:
            terrio.Raster: The source.
        """
        if self._source is None:
            ds = self.src_dataset
            arr = ds.read()
            self._source = _terrio.Raster.from_array(
                arr,
                self.src_transform.to_gdal(),
                _crs_text(self.src_crs if self.src_crs is not None else "EPSG:4326"),
                None if self.src_nodata is None else float(self.src_nodata),
            )
        return self._source

    def _src(self) -> Any:
        """The source to warp, read only where the output needs it when it can be.

        The dataset's own raster when it is read on demand, from which the warp reads only the
        windows the output needs; else the whole source in memory.

        Returns:
            terrio.Raster: The source.
        """
        ds = self.src_dataset
        raster = getattr(ds, "_raster", None)
        if (
            getattr(ds, "mode", None) == "r"
            and raster is not None
            and raster.is_lazy
            and self.src_transform == ds.transform
        ):
            return raster
        return self._src_raster()

    def _read_level(
        self, bands: Any, window: tuple[int, int, int, int], level: int | None
    ) -> np.ndarray:
        """Warps an integer window of the output grid.

        Args:
            bands: The bands, from 1.
            window: `(x, y, w, h)` in the output grid.
            level: Unused.

        Returns:
            numpy.ndarray: `(bands, h, w)`.
        """
        self._check_open()
        x, y, w, h = window
        if self.width * self.height * self._count <= _CACHE_PIXELS:
            # Warp the whole grid once, as GDAL caches the warped blocks: windows then read the
            # same pixels as a full read.
            if self._full is None:
                self._full = self._warp(0, 0, self.width, self.height)
            return self._full[[b - 1 for b in bands], y : y + h, x : x + w]
        return self._warp(x, y, w, h)[[b - 1 for b in bands]]

    def _warp(self, x: int, y: int, w: int, h: int) -> np.ndarray:
        """Warps a window of the output grid, every band.

        Args:
            x: The first column.
            y: The first row.
            w: The number of columns.
            h: The number of rows.

        Returns:
            numpy.ndarray: `(count, h, w)`.
        """
        t = self._transform * Affine.translation(x, y)
        nd: Any = (
            "none"
            if self.dst_nodata is None
            else ("nan" if np.isnan(self.dst_nodata) else float(self.dst_nodata))
        )
        from terrio.rio.warp import _warp_extras

        georef: dict[str, Any] = dict(self._georef)
        if "rpc" not in georef:
            georef["s_srs"] = _crs_text(self.src_crs)
        out = _terrio.warp(
            self._src(),
            t_srs=_crs_text(self.dst_crs),
            dst_transform=t.to_gdal(),
            ts=(w, h),
            resampling=_WARP_NAMES[self.resampling],
            srcnodata="none" if self.src_nodata is None else float(self.src_nodata),
            dstnodata=nd,
            ot=_gdal(self._dtype),
            et=self.tolerance,
            wo=_warp_extras(self.warp_extras),
            **georef,
        )
        return np.asarray(out.read())

    def close(self) -> None:
        """Closes the view (not the source dataset)."""
        self._source = None
        self._full = None
        super(DatasetReader, self).close()

    def start(self) -> None:
        """Opens the view (it is open once created)."""


def _gdal(dtype: str) -> str:
    """The GDAL name of a data type.

    Args:
        dtype: A NumPy dtype name.

    Returns:
        str: The GDAL name.
    """
    from terrio.rio.dtypes import _gdal_typename

    return _gdal_typename(dtype)


__all__ = ["WarpedVRT"]
