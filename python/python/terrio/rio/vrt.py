# SPDX-FileCopyrightText: Copyright (c) 2004, Frank Warmerdam <warmerdam@pobox.com>
# SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2003, Frank Warmerdam <warmerdam@pobox.com>
# SPDX-FileCopyrightText: Copyright (c) 2007-2012, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 1998, 2003, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 2007-2013, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2007, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 2008-2012, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (translated to Python and modified):
#   GDAL v3.12.1:
#     frmts/vrt/vrtwarped.cpp
#     alg/gdalwarpoperation.cpp
#     gcore/rasterio.cpp
#     gcore/gdaldataset.cpp
#     gcore/gdal_misc.cpp
#     gcore/gdalnodatamaskband.cpp
#   rasterio 1.5.2:
#     rasterio/_warp.pyx

"""`WarpedVRT`: a dataset reprojected on the fly, as rasterio's (GDAL's `VRTWarpedDataset`).

Reads follow GDAL 3.12's code paths so that pixels are GDAL's:

- the grid is the one rasterio asks GDAL for (`WarpedVRTReaderBase.__init__`): given, scaled, or
  `GDALSuggestedWarpOutput()`'s;
- implicit overviews are built from the source's as `VRTWarpedDataset::CreateImplicitOverview()`
  builds them, and chosen as `GDALBandGetBestOverviewLevel2()` (dataset reads) and
  `VRTWarpedRasterBand::GetBestOverviewLevel()` (band reads) choose them;
- a read is dispatched as `VRTWarpedDataset::IRasterIO()` dispatches it: a whole-image read of all
  the warped bands is warped straight into the buffer's grid, other reads are assembled from
  warped blocks (`VRTWarpedDataset::ProcessBlock()`, 512 x 128) and resampled as
  `GDALRasterBand::IRasterIO()` does;
- each region (block or read) is one `GDALWarpOperation::WarpRegionToBuffer()`: the source window
  `ComputeSourceWindow()` gives, read from the source level with the source dataset's own window
  reads, warped through GDAL's approximate transformer (`terrio._terrio.RegionWarp`).

The source is never read whole; warped blocks are kept in a bounded cache.
"""

from __future__ import annotations

from collections import OrderedDict
from typing import Any

import numpy as np
from affine import Affine

from terrio import _terrio
from terrio.rio._resample import best_overview, gdal_cast, raster_io
from terrio.rio._tiff import Structure
from terrio.rio.crs import CRS
from terrio.rio.dtypes import dtype_fwd, typename_rev
from terrio.rio.enums import ColorInterp, MaskFlags, Resampling
from terrio.rio.errors import WarpedVRTError, WarpOptionsError
from terrio.rio.io import DatasetReader
from terrio.rio.warp import (
    _WARP_NAMES,
    _crs_text,
    _gcp_args,
    _rpc_args,
    calculate_default_transform,
)

# rasterio's `DEFAULT_NODATA_FLAG`: "the source dataset's nodata value".
_DEFAULT: Any = object()

# Warped blocks kept per VRT, in bytes (GDAL keeps them in its block cache).
_BLOCK_CACHE_BYTES = 64 * 1024 * 1024

# `GDALWarpOptions::dfWarpMemoryLimit` when rasterio passes 0: GDAL's default, 64 MB.
_WARP_MEMORY = 64.0 * 1024 * 1024

# Resampling methods `GDALDataset::IRasterIO()` resamples at the dataset level.
_KERNELS = (Resampling.bilinear, Resampling.cubic, Resampling.cubic_spline, Resampling.lanczos)

# The source types whose warped VRT type `_terrio.warped_vrt_type` resolves, by GDAL name.
_WORKING_TYPES = frozenset(
    ("Byte", "Int8", "UInt16", "Int16", "UInt32", "Int32", "UInt64", "Int64", "Float32", "Float64")
)


class _Grid:
    """A warped grid: the VRT, one of its implicit overviews, or a grid at a read's buffer size.

    Attributes:
        level: The source level it is warped from: None (full resolution) or an overview index.
        gt: The grid's geotransform, in GDAL's order.
        width: The width.
        height: The height.
        ovr: The implicit overview it is (an index), None for the VRT itself or a rescaled grid.
        full: Whether it is the VRT itself (the only grid with overviews).
    """

    __slots__ = ("level", "gt", "width", "height", "ovr", "full", "bx", "by")

    def __init__(
        self,
        level: int | None,
        gt: tuple[float, ...],
        width: int,
        height: int,
        ovr: int | None = None,
        full: bool = False,
    ) -> None:
        self.level, self.gt, self.width, self.height = level, tuple(gt), width, height
        self.ovr, self.full = ovr, full
        # The VRTWarpedDataset constructor's default block size.
        self.bx, self.by = min(width, 512), min(height, 128)

    def key(self) -> tuple[Any, ...]:
        """What identifies the grid's warped pixels.

        Returns:
            tuple: The source level, geotransform and size.
        """
        return (self.level, self.gt, self.width, self.height)

    def rescaled(self, width: int, height: int) -> _Grid:
        """The same warp at another size (`RescaleDstGeoTransform()`).

        Args:
            width: The new width.
            height: The new height.

        Returns:
            _Grid: The grid.
        """
        g = list(self.gt)
        fx = float(self.width) / width
        fy = float(self.height) / height
        g[1] *= fx
        g[2] *= fx
        g[4] *= fy
        g[5] *= fy
        return _Grid(self.level, tuple(g), width, height)


class WarpedVRT(DatasetReader):
    """A read-only view of a dataset reprojected into another CRS or grid, as rasterio's.

    rasterio's `WarpedVRT` is GDAL's warped VRT, whose reads this one follows.
    Pixels are warped when they are read, region by region, from the windows of the source (or
    of its overviews) each region needs.
    """

    def __init__(
        self,
        src_dataset: Any,
        src_crs: Any = None,
        crs: Any = None,
        resampling: Resampling = Resampling.nearest,
        tolerance: float = 0.125,
        src_nodata: Any = _DEFAULT,
        nodata: Any = _DEFAULT,
        width: int | None = None,
        height: int | None = None,
        src_transform: Affine | None = None,
        transform: Affine | None = None,
        init_dest_nodata: bool = True,
        src_alpha: int = 0,
        dst_alpha: int = 0,
        add_alpha: bool = False,
        warp_mem_limit: int = 0,
        dtype: Any = None,
        **warp_extras: Any,
    ) -> None:
        """Creates the view.

        Args:
            src_dataset: The source dataset (opened for reading).
            src_crs: The source CRS, overriding the dataset's.
            crs: The output CRS; by default, the source's.
            resampling: The warp's resampling method.
            tolerance: The approximate transformer's maximum error, in source pixels (0: exact).
            src_nodata: The source nodata value, overriding the dataset's (None: none).
            nodata: The output nodata value; by default, the source nodata value.
            width: The output width (with `height`).
            height: The output height.
            src_transform: The source transform, overriding the dataset's.
            transform: The output transform (with `width` and `height`).
            init_dest_nodata: Initialize the output to the nodata value (else 0).
            src_alpha: A source band to use as alpha (by default, the source's alpha band).
            dst_alpha: An output band to write the alpha to.
            add_alpha: Add an alpha band to the output (and drop the output nodata value).
            warp_mem_limit: The warp's memory limit, in MB (0: GDAL's default, 64 MB).
            dtype: The working and output data type; by default, the source's.
            **warp_extras: GDAL warp options; `init_dest` is honoured, others are ignored.
                GDAL's transformer options too: a source with neither a geotransform nor GCPs
                is warped by its RPCs (`SRC_METHOD="RPC"` forces them), with the `RPC_*`
                options (`RPC_HEIGHT`, `RPC_DEM`...); `MAX_GCP_ORDER` and `SRC_METHOD` with GCPs.

        Raises:
            ValueError: If the resampling method is not supported.
            WarpOptionsError: If an alpha band is added to a source that has one.
            WarpedVRTError: If RPC positions are said to be in a CRS other than WGS 84.
        """
        super(DatasetReader, self).__init__()
        try:
            rs = Resampling(resampling)
            if rs == Resampling.gauss:
                raise ValueError
            self._warp_name = _WARP_NAMES[rs]
        except (ValueError, KeyError):
            raise ValueError(
                "resampling must be one of: "
                + ", ".join(f"Resampling.{r.name}" for r in Resampling if r in _WARP_NAMES)
            ) from None
        self.src_dataset = src_dataset
        self.name = f"WarpedVRT({src_dataset.name})"
        self.mode = "r"
        self.driver = "VRT"
        self.resampling = rs
        self.tolerance = tolerance
        self.src_crs = CRS.from_user_input(src_crs) if src_crs is not None else src_dataset.crs
        self.src_transform = src_transform or src_dataset.transform
        # A source georeferenced by GCPs alone: rasterio takes their CRS when none is given.
        src_gcps, gcp_crs = src_dataset.gcps
        gcp_only = bool(src_gcps) and src_dataset.transform.almost_equals(Affine.identity())
        if (
            self.src_crs is None
            and gcp_only
            and self.src_transform.almost_equals(Affine.identity())
        ):
            self.src_crs = gcp_crs
        self.src_nodata = src_dataset.nodata if src_nodata is _DEFAULT else src_nodata
        self.dst_nodata = self.src_nodata if nodata is _DEFAULT else nodata
        self.dst_width, self.dst_height, self.dst_transform = width, height, transform
        self.warp_extras = dict(warp_extras)
        if init_dest_nodata is True and "init_dest" not in warp_extras:
            self.warp_extras["init_dest"] = "NO_DATA" if self.dst_nodata is not None else "0"
        self.working_dtype = dtype
        self.dst_crs = CRS.from_user_input(crs) if crs is not None else self.src_crs

        # A source georeferenced by RPCs: GDALCreateGenImgProjTransformer2() takes them when the
        # dataset has neither a geotransform nor GCPs (or with SRC_METHOD=RPC), their positions in
        # WGS 84 unless the source CRS is given (SRC_SRS).
        extras = {str(k).upper(): v for k, v in self.warp_extras.items()}
        method = str(extras.get("SRC_METHOD", "")).upper()
        src_rpcs = getattr(src_dataset, "rpcs", None)
        no_geotransform = src_dataset.transform.almost_equals(Affine.identity())
        rpc_georef = bool(src_rpcs) and (
            method == "RPC" or (method == "" and not gcp_only and no_geotransform)
        )
        self._warp_rpc: dict[str, Any] | None = None
        self._rpc_crs: CRS | None = None
        if rpc_georef:
            self._rpc_crs = CRS.from_epsg(4326)

        # Alpha bands, as rasterio sets them up.
        src_count = src_dataset.count
        src_alpha_band = dst_alpha_band = 0
        for b in src_dataset.indexes:
            if src_dataset.colorinterp[b - 1] == ColorInterp.alpha:
                src_alpha_band = dst_alpha_band = b
        if add_alpha:
            if src_alpha_band:
                raise WarpOptionsError(
                    "The VRT already has an alpha band, adding a new one is not supported"
                )
            dst_alpha_band = src_count + 1
            self.dst_nodata = None
        if dst_alpha:
            dst_alpha_band = dst_alpha
        if src_alpha:
            src_alpha_band = src_alpha
        self._src_alpha = src_alpha_band
        self._dst_alpha = dst_alpha_band
        self._src_count = src_count

        # The grid: rasterio's four cases.
        src_gt = self.src_transform
        self._warp_gcps: dict[str, Any] | None = None
        # The RPC transformer's options, as rasterio passes the warp extras: names upper-cased.
        # (rasterio 1.5.0 upper-cases their values too, the path of RPC_DEM included, which a
        # case-sensitive file system then fails to open: the path is kept as given here.)
        rpc_options = {k: v for k, v in extras.items() if k.startswith("RPC_")}
        if transform is None and (not width or not height) and rpc_georef:
            # GDALAutoCreateWarpedVRTEx() on a dataset with RPCs: its transformer, and so the grid
            # (GDALSuggestedWarpOutput(), WGS 84 when no CRS is given), uses them.
            if self.src_crs is not None and self.src_crs.to_epsg() != 4326:
                raise WarpedVRTError("terrio.rio warps RPCs to WGS 84 longitudes and latitudes")
            self._warp_rpc = _rpc_args(src_rpcs, rpc_options)
            t, w, h = calculate_default_transform(
                self._rpc_crs,
                self.dst_crs if self.dst_crs is not None else self._rpc_crs,
                src_dataset.width,
                src_dataset.height,
                rpcs=src_rpcs,
                **rpc_options,
            )
        elif transform is None and (not width or not height) and gcp_only:
            # GDALAutoCreateWarpedVRTEx() on a dataset with GCPs and no geotransform: its
            # transformer, and so the grid, uses the GCPs (src_transform plays no part).
            options = {
                k: v
                for k, v in self.warp_extras.items()
                if k.upper() in ("MAX_GCP_ORDER", "SRC_METHOD")
            }
            self._warp_gcps = _gcp_args(src_gcps, options)
            t, w, h = calculate_default_transform(
                self.src_crs,
                self.dst_crs,
                src_dataset.width,
                src_dataset.height,
                gcps=src_gcps,
                **options,
            )
        elif transform is None and (not width or not height):
            # GDALAutoCreateWarpedVRTEx(): GDALSuggestedWarpOutput() around the source, whose
            # own geotransform (rotation included, unlike its bounds) the transformer uses.
            gt, w, h = _terrio.suggested_grid(
                src_dataset.width,
                src_dataset.height,
                src_dataset.transform.to_gdal(),
                _crs_text(self.src_crs),
                _crs_text(self.dst_crs),
            )
            t = Affine.from_gdal(*gt)
            src_gt = src_dataset.transform
        elif transform is not None and width and height:
            t, w, h = transform, int(width), int(height)
        elif transform is None and self.src_crs == self.dst_crs:
            t = self.src_transform * Affine.scale(
                src_dataset.width / width,
                src_dataset.height / height,  # type: ignore[operator]
            )
            w, h = int(width), int(height)  # type: ignore[arg-type]
        elif transform is None and rpc_georef:
            # rasterio's case 3 with RPCs: the grid from them (with every warp extra as a
            # transformer option), scaled to the size; the warp then uses the source transform.
            t, dw, dh = calculate_default_transform(
                self._rpc_crs,
                self.dst_crs if self.dst_crs is not None else self._rpc_crs,
                src_dataset.width,
                src_dataset.height,
                rpcs=src_rpcs,
                **rpc_options,
            )
            t = t * Affine.scale(dw / width, dh / height)  # type: ignore[operator]
            w, h = int(width), int(height)  # type: ignore[arg-type]
        elif transform is None:
            left, bottom, right, top = src_dataset.bounds
            t, dw, dh = calculate_default_transform(
                self.src_crs,
                self.dst_crs,
                src_dataset.width,
                src_dataset.height,
                left,
                bottom,
                right,
                top,
            )
            t = t * Affine.scale(dw / width, dh / height)  # type: ignore[operator]
            w, h = int(width), int(height)  # type: ignore[arg-type]
        else:
            # A transform without a size (rasterio raises): the suggested size, on that transform.
            left, bottom, right, top = src_dataset.bounds
            _, w, h = calculate_default_transform(
                self.src_crs,
                self.dst_crs,
                src_dataset.width,
                src_dataset.height,
                left,
                bottom,
                right,
                top,
            )
            t = transform
        self._transform = t
        self.width, self.height = int(w), int(h)
        self._crs = self.dst_crs
        self._warp_src_gt = tuple(src_gt.to_gdal())

        # Bands: the source's (`CopyCommonInfoFrom()`), then the alpha band.
        n = max(src_count, dst_alpha_band)
        self._count = n
        self._nodata = None if self.dst_nodata is None else float(self.dst_nodata)
        src_nd = None if self.src_nodata is None else float(self.src_nodata)
        # The nodata values the warp uses (GDAL's `padfSrcNoDataReal` and `padfDstNoDataReal`) and
        # its `INIT_DEST`: rasterio's, but `GDALAutoCreateWarpedVRTEx()` (rasterio's grid from
        # GDAL, its case 4) gives a warp without either and without a source alpha band the source
        # bands' own nodata values, when not every band's type misses its value; and then
        # `INIT_DEST=NO_DATA` unless set. The VRT's bands still get rasterio's nodata value.
        self._warp_src_nodata: list[float | None] = [src_nd] * src_count
        warp_dst: list[float | None] = [self._nodata] * src_count
        self._init_dest = self.warp_extras.get("init_dest")
        auto = transform is None and (not width or not height)
        if auto and src_nd is None and self._nodata is None and not src_alpha_band:
            own = [
                None if v is None else float(v) for v in list(src_dataset.nodatavals)[:src_count]
            ]
            src_type = src_dataset.dtypes[0]
            inexact = sum(1 for v in own if v is not None and not _exact(v, src_type))
            if inexact != src_count and any(v is not None for v in own):
                self._warp_src_nodata = [-1e10 if v is None else v for v in own]
                warp_dst = list(self._warp_src_nodata)
                if self._init_dest is None:
                    self._init_dest = "NO_DATA"
        if dtype is not None:
            self._dtype = np.dtype(dtype).name
        else:
            self._dtype = src_dataset.dtypes[0]
            gdal_type = _gdal(self._dtype)
            if gdal_type in _WORKING_TYPES:
                # rasterio leaves the type to GDAL: `GDALCreateWarpedVRT()` creates the bands in
                # the warp's working type (`GDALWarpResolveWorkingDataType()`), the source's
                # widened to hold the nodata values (Byte with -1: Int16), and the warp drops
                # source nodata values the source's type cannot hold.
                name, self._warp_src_nodata = _terrio.warped_vrt_type(
                    gdal_type, self._warp_src_nodata, warp_dst
                )
                self._dtype = dtype_fwd[typename_rev[name]] or self._dtype
        # One output nodata value for the warp of every band (band 1's).
        self._warp_dst_nodata = warp_dst[0] if warp_dst else self._nodata
        # rasterio reports the nodata value it gave each band (its `_nodatavals`), whether or not
        # the band's type holds it, with its own quirk for three-band VRTs with an alpha band.
        self._nodatavals = [self.dst_nodata] * n
        if dst_alpha and n == 3:
            self._nodatavals[dst_alpha - 1] = None

        def per_band(values: Any, default: Any) -> list[Any]:
            values = list(values)
            return [values[i] if i < len(values) else default for i in range(n)]

        s = Structure(
            count=n,
            width=self.width,
            height=self.height,
            block_shape=(min(128, self.height), min(512, self.width)),
        )
        s.tiled = True
        s.colorinterp = per_band(src_dataset.colorinterp, ColorInterp.undefined)
        if dst_alpha_band:
            s.colorinterp[dst_alpha_band - 1] = ColorInterp.alpha
        s.band_tags = [dict(src_dataset.tags(b + 1)) if b < src_count else {} for b in range(n)]
        s.descriptions = per_band(src_dataset.descriptions, None)
        s.scales = per_band(src_dataset.scales, 1.0)
        s.offsets = per_band(src_dataset.offsets, 0.0)
        s.units = per_band(src_dataset.units, None)
        if n > 1:
            s.namespaces = {"IMAGE_STRUCTURE": {"INTERLEAVE": "PIXEL"}}
        self._structure = s

        # Implicit overviews (VRTWarpedDataset::GetOverviewCount(), GetOverviewSize()).
        src_ovr = list(getattr(src_dataset, "_overview_sizes", []) or [])
        sizes: list[tuple[int, int]] = []
        ratios: list[tuple[float, float]] = []
        for ow, oh in src_ovr:
            target = float(src_dataset.width) / ow
            ovw = int(self.width / target + 0.5)
            ovh = int(self.height / target + 0.5)
            if ovw < 1 or ovh < 1:
                break
            sizes.append((ovw, ovh))
            ratios.append((float(src_dataset.width) / ow, float(src_dataset.height) / oh))
        self._overview_sizes = sizes
        self._src_ratios = ratios
        self._src_levels = src_ovr
        self._grid = _Grid(None, tuple(t.to_gdal()), self.width, self.height, full=True)
        self._ovr_grids: dict[int, _Grid] = {}
        self._warpers: dict[tuple[Any, ...], Any] = {}
        self._blocks: OrderedDict[tuple[Any, ...], np.ndarray] = OrderedDict()
        self._block_bytes = 0
        mem = float(warp_mem_limit) * 1024 * 1024 if warp_mem_limit > 0 else _WARP_MEMORY
        self._warp_memory = mem
        self._init = self._init_values()
        # The value the warp starts each region from (band 1's when bands differ).
        self._init_value = float(self._init[0]) if self._init else 0.0

    # Warp setup.

    def _init_values(self) -> list[Any]:
        """The initial value of each warped band (`InitializeDestinationBuffer()`).

        Returns:
            list: One value per warped band, in the working data type.
        """
        spec = str(self._init_dest if self._init_dest is not None else "0").split(",")
        out = []
        for i in range(self._src_count):
            item = spec[min(i, len(spec) - 1)].strip()
            if item.upper() == "NO_DATA":
                v = 0.0 if self._warp_dst_nodata is None else self._warp_dst_nodata
            else:
                try:
                    v = float(item)
                except ValueError:
                    v = 0.0
            if self._dtype == "uint8":
                out.append(np.uint8(max(0, min(255, int(v)))) if np.isfinite(v) else np.uint8(0))
            else:
                out.append(gdal_cast(np.array([v]), self._dtype)[0])
        return out

    def _ovr_grid(self, i: int) -> _Grid:
        """Implicit overview `i` (`VRTWarpedDataset::CreateImplicitOverview()`).

        Args:
            i: The overview index.

        Returns:
            _Grid: Its grid, warped from source overview `i`.
        """
        g = self._ovr_grids.get(i)
        if g is None:
            ow, oh = self._overview_sizes[i]
            r = self._grid.rescaled(ow, oh)
            g = _Grid(i, r.gt, ow, oh, ovr=i)
            self._ovr_grids[i] = g
        return g

    def _level_grid(self, ovr: int | None) -> _Grid:
        """The VRT's grid or one of its implicit overviews.

        Args:
            ovr: None for the VRT, else an overview index.

        Returns:
            _Grid: The grid.
        """
        return self._grid if ovr is None else self._ovr_grid(ovr)

    def _warper(self, grid: _Grid) -> Any:
        """The region warper of a grid (one per source level and grid).

        Args:
            grid: The grid.

        Returns:
            terrio.RegionWarp: The warper.
        """
        key = grid.key()
        rw = self._warpers.get(key)
        if rw is None and self._warp_rpc is not None:
            rw = self._rpc_warper(grid)
        if rw is None:
            sgt = list(self._warp_src_gt)
            gcps = self._warp_gcps
            if grid.level is None:
                size = (self.src_dataset.width, self.src_dataset.height)
            elif gcps is not None:
                # GDALCreateSimilarGCPTransformer(): the GCPs' pixels and lines scaled.
                rx, ry = self._src_ratios[grid.level]
                points = [(p / rx, li / ry, x, y, z) for p, li, x, y, z in gcps["gcps"]]
                gcps = dict(gcps, gcps=points)
                size = self._src_levels[grid.level]
            else:
                # GDALCreateSimilarGenImgProjTransformer(): the source geotransform scaled.
                rx, ry = self._src_ratios[grid.level]
                if sgt[2] == 0.0 and sgt[4] == 0.0:
                    sgt[1] *= rx
                    sgt[5] *= ry
                else:
                    sgt[1] *= rx
                    sgt[2] *= rx
                    sgt[4] *= rx
                    sgt[5] *= rx
                size = self._src_levels[grid.level]
            # rasterio's cases 1 to 3 warp with GDALCreateGenImgProjTransformer3(), which does not
            # reproject when the source has no CRS (an RPC source's): the source transform maps
            # straight into the grid's CRS.
            src_crs = self.src_crs
            if src_crs is None and self._rpc_crs is not None:
                src_crs = self.dst_crs if self.dst_crs is not None else self._rpc_crs
            rw = _terrio.RegionWarp(
                (int(size[0]), int(size[1])),
                tuple(sgt),
                _crs_text(src_crs),
                _crs_text(self.dst_crs if self.dst_crs is not None else self._rpc_crs),
                grid.gt,
                self._warp_name,
                # rasterio passes the tolerance to GDAL as a C float.
                float(np.float32(self.tolerance)),
                _gdal(self._dtype),
                list(self._warp_src_nodata),
                self._warp_dst_nodata,
                **(
                    {}
                    if gcps is None
                    else {
                        "gcps": gcps["gcps"],
                        "gcp_order": gcps["order"],
                        "gcp_method": gcps["gcp_method"],
                    }
                ),
            )
            if len(self._warpers) > 32:
                self._warpers.clear()
            self._warpers[key] = rw
        return rw

    def _rpc_warper(self, grid: _Grid) -> Any:
        """The region warper of a grid of a source georeferenced by RPCs.

        An implicit overview's is GDALCreateSimilarRPCTransformer()'s: the RPCs' line and sample
        offsets and scales divided by the level's ratios, the options kept but `RPC_DEM_SRS`.

        Args:
            grid: The grid.

        Returns:
            terrio.RegionWarp: The warper.
        """
        assert self._warp_rpc is not None and self._rpc_crs is not None
        rpc = dict(self._warp_rpc["rpc"])
        options = dict(self._warp_rpc["to"])
        if grid.level is None:
            size = (self.src_dataset.width, self.src_dataset.height)
        else:
            rx, ry = self._src_ratios[grid.level]
            if rx != 1.0 or ry != 1.0:
                for name, ratio in (
                    ("LINE_OFF", ry),
                    ("LINE_SCALE", ry),
                    ("SAMP_OFF", rx),
                    ("SAMP_SCALE", rx),
                ):
                    rpc[name] = repr(float(rpc[name]) / ratio)
            options.pop("RPC_DEM_SRS", None)
            size = self._src_levels[grid.level]
        rw = _terrio.RegionWarp(
            (int(size[0]), int(size[1])),
            (0.0, 1.0, 0.0, 0.0, 0.0, 1.0),
            _crs_text(self._rpc_crs),
            _crs_text(self.dst_crs if self.dst_crs is not None else self._rpc_crs),
            grid.gt,
            self._warp_name,
            # rasterio passes the tolerance to GDAL as a C float.
            float(np.float32(self.tolerance)),
            _gdal(self._dtype),
            list(self._warp_src_nodata),
            self._warp_dst_nodata,
            rpc=rpc,
            rpc_options=options,
        )
        if len(self._warpers) > 32:
            self._warpers.clear()
        self._warpers[grid.key()] = rw
        return rw

    def _source_window(
        self, level: int | None, window: tuple[int, int, int, int]
    ) -> tuple[np.ndarray, np.ndarray | None]:
        """Reads a window of a source level and the validity GDAL's warper gives it.

        Args:
            level: The source level (None or an overview index).
            window: `(x, y, w, h)`.

        Returns:
            tuple: The source bands `(count, h, w)` in the working data type, and the per-dataset
            mask (or alpha) the warper uses without nodata values, or None.
        """
        ds = self.src_dataset
        bands = list(range(1, self._src_count + 1))
        data = np.asarray(ds._read_level(bands, window, level))
        mask = None
        if all(v is None for v in self._warp_src_nodata):
            if self._src_alpha:
                # GDALWarpSrcAlphaMasker(): pixels of zero density are left out.
                mask = data[self._src_alpha - 1] != 0
            else:
                flags = ds._mask_flags()[0] if ds.count else MaskFlags.all_valid.value
                if flags & MaskFlags.per_dataset.value and not flags & MaskFlags.alpha.value:
                    # GDALWarpSrcMaskMasker(): the source's per-dataset mask.
                    mask = self._source_mask(level, window)
        if data.dtype != np.dtype(self._dtype):
            # The source is read in the working data type.
            data = gdal_cast(data, self._dtype)
        return np.ascontiguousarray(data), mask

    def _source_mask(self, level: int | None, window: tuple[int, int, int, int]) -> Any:
        """The source's per-dataset mask in a window of a level.

        Args:
            level: The source level.
            window: `(x, y, w, h)`.

        Returns:
            numpy.ndarray | None: Non-zero where valid.
        """
        ds = self.src_dataset
        raster = getattr(ds, "_raster", None)
        if raster is not None and getattr(raster, "has_mask", False):
            m = raster.read_mask(window=window, overview=level)
            return None if m is None else np.asarray(m)
        from terrio.rio.windows import Window

        x, y, w, h = window
        if level is None:
            return np.asarray(ds.read_masks(1, window=Window(x, y, w, h)))
        return None

    def _warp_region(
        self,
        grid: _Grid,
        region: tuple[int, int, int, int],
        win: tuple[int, int, int, int] | None = None,
        direct: bool = False,
    ) -> np.ndarray:
        """Warps a region of a grid (`GDALWarpOperation::WarpRegionToBuffer()`), all bands.

        Args:
            grid: The grid.
            region: `(x, y, w, h)` in the grid.
            win: The region's source window, when already computed.
            direct: Whether the read is warped straight into its buffer
                (`VRTWarpedDataset::IRasterIO()`), whose warp gets no extra source size: its
                kernel scale is the padded window's.

        Returns:
            numpy.ndarray: `(count, h, w)` in the working data type.
        """
        x, y, w, h = region
        rw = self._warper(grid)
        if win is None:
            win = rw.source_window(region)
        dtype = np.dtype(self._dtype)
        out = np.empty((self._count, h, w), dtype)
        for b in range(self._count):
            out[b] = self._init[b] if b < self._src_count else 0
        written = np.zeros((h, w), bool)
        if win is not None and win[2] > 0 and win[3] > 0:
            data, mask = self._source_window(grid.level, win)
            warped, written = rw.warp(region, win, data, mask, self._init_value, direct)
            warped = np.asarray(warped)
            written = np.asarray(written)
            for b in range(self._src_count):
                out[b] = np.where(written, warped[b], out[b])
        if self._dst_alpha:
            # GDALWarpDstAlphaMasker(): the density of warped pixels, scaled to DST_ALPHA_MAX.
            amax = {"int16": 32767, "uint16": 65535}.get(dtype.name, 255)
            out[self._dst_alpha - 1] = np.where(written, amax, 0).astype(dtype)
        return out

    def _working_memory(self, sw: int, sh: int, dw: int, dh: int) -> float:
        """`GDALWarpOperation::GetWorkingMemoryForWindow()`.

        Args:
            sw: The source window's width.
            sh: Its height.
            dw: The destination window's width.
            dh: Its height.

        Returns:
            float: Bytes.
        """
        bits = np.dtype(self._dtype).itemsize * 8 * self._src_count
        src_bits = bits
        ds = self.src_dataset
        if self._src_alpha:
            src_bits += 32
        elif ds.count and ds._mask_flags()[0] & MaskFlags.per_dataset.value:
            src_bits += 1
        if any(v is not None for v in self._warp_src_nodata):
            src_bits += self._src_count
        dst_bits = bits
        if self._warp_dst_nodata is not None:
            dst_bits += self._src_count
        if self._dst_alpha:
            dst_bits += 32
        return (float(src_bits) * sw * sh + float(dst_bits) * dw * dh) / 8.0

    # Blocks.

    def _block(self, grid: _Grid, bx: int, by: int) -> np.ndarray:
        """A warped block (`VRTWarpedDataset::ProcessBlock()`), from the cache when it is there.

        Args:
            grid: The grid.
            bx: The block column.
            by: The block row.

        Returns:
            numpy.ndarray: `(count, h, w)` of the block (partial at the grid's edges).
        """
        key = (*grid.key(), bx, by)
        blk = self._blocks.get(key)
        if blk is not None:
            self._blocks.move_to_end(key)
            return blk
        x, y = bx * grid.bx, by * grid.by
        region = (x, y, min(grid.bx, grid.width - x), min(grid.by, grid.height - y))
        blk = self._warp_region(grid, region)
        self._blocks[key] = blk
        self._block_bytes += blk.nbytes
        while self._block_bytes > _BLOCK_CACHE_BYTES and len(self._blocks) > 1:
            _, old = self._blocks.popitem(last=False)
            self._block_bytes -= old.nbytes
        return blk

    def _grid_window(
        self, grid: _Grid, bands: Any, window: tuple[int, int, int, int]
    ) -> np.ndarray:
        """An integer window of a grid, assembled from its warped blocks.

        Args:
            grid: The grid.
            bands: The bands, from 1.
            window: `(x, y, w, h)`.

        Returns:
            numpy.ndarray: `(bands, h, w)`.
        """
        x, y, w, h = window
        idx = [b - 1 for b in bands]
        out = np.empty((len(idx), h, w), np.dtype(self._dtype))
        for by in range(y // grid.by, (y + h - 1) // grid.by + 1):
            for bx in range(x // grid.bx, (x + w - 1) // grid.bx + 1):
                blk = self._block(grid, bx, by)
                x0, y0 = bx * grid.bx, by * grid.by
                cx0, cy0 = max(x, x0), max(y, y0)
                cx1 = min(x + w, x0 + blk.shape[2])
                cy1 = min(y + h, y0 + blk.shape[1])
                out[:, cy0 - y : cy1 - y, cx0 - x : cx1 - x] = blk[
                    idx, cy0 - y0 : cy1 - y0, cx0 - x0 : cx1 - x0
                ]
        return out

    def _read_level(
        self, bands: Any, window: tuple[int, int, int, int], level: int | None
    ) -> np.ndarray:
        """Reads an integer window of the VRT or of one of its implicit overviews.

        Args:
            bands: The bands, from 1.
            window: `(x, y, w, h)`.
            level: None for the VRT, else an overview index.

        Returns:
            numpy.ndarray: `(bands, h, w)`.
        """
        self._check_open()
        return self._grid_window(self._level_grid(level), bands, window)

    # RasterIO.

    def _io(
        self,
        indexes: Any,
        window: Any,
        buf: tuple[int, int],
        resampling: Resampling,
    ) -> np.ndarray:
        """Reads bands of a window into a buffer, as rasterio's read through GDAL's `RasterIO`.

        Args:
            indexes: The bands.
            window: The window, or None for the whole raster.
            buf: The output `(height, width)`.
            resampling: The resampling method.

        Returns:
            numpy.ndarray: `(bands, height, width)` in the VRT's data type.
        """
        resampling = Resampling(resampling)
        if resampling != Resampling.rms and resampling > 7:
            from terrio.rio.errors import ResamplingAlgorithmError

            raise ResamplingAlgorithmError(
                f"{resampling!r} can be used for warp operations but not for reads and writes"
            )
        if window is None:
            fw: tuple[float, float, float, float] = (0, 0, self.width, self.height)
        else:
            fw = (window.col_off, window.row_off, window.width, window.height)
        if buf[0] <= 0 or buf[1] <= 0:
            return np.zeros((len(indexes), max(buf[0], 0), max(buf[1], 0)), self._dtype)
        if fw[2] <= 0 or fw[3] <= 0:
            return np.zeros((len(indexes), buf[0], buf[1]), self._dtype)
        # rasterio's io_multi_band(): the integer window GDAL gets, with the floating-point one.
        iw = (int(fw[0]), int(fw[1]), int(max(1, fw[2])), int(max(1, fw[3])))
        return self._dataset_io(self._grid, list(indexes), iw, fw, buf, resampling, False)

    def _dataset_io(
        self,
        grid: _Grid,
        idx: list[int],
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: Resampling,
        routed: bool,
    ) -> np.ndarray:
        """`VRTWarpedDataset::IRasterIO()` on a grid.

        Args:
            grid: The grid (the VRT, an implicit overview or a rescaled grid).
            idx: The bands.
            iw: The integer window `(x, y, w, h)`.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The resampling method.
            routed: Whether a band of a one-band VRT routed the read here.

        Returns:
            numpy.ndarray: `(bands, height, width)`.

        Raises:
            RasterioIOError: If the window's corners cannot be transformed.
        """
        nx, ny, nxs, nys = iw
        bh, bw = buf
        whole = iw == (0, 0, grid.width, grid.height)
        if (not whole and (bw <= grid.bx or bh <= grid.by)) or len(idx) < self._count:
            # Small requests and band subsets go through the cached blocks.
            return self._fallback_io(grid, idx, iw, fw, buf, rs, routed)
        if (bw < nxs or bh < nys) and grid.full and self._overview_sizes:
            # GDALDataset::TryOverviewRasterIO(), choosing with GDALBandGetBestOverviewLevel2().
            moved = self._to_overview(grid, iw, fw, buf, rs, True)
            if moved is not None:
                ovr, iw2, fw2 = moved
                return self._dataset_io(self._ovr_grid(ovr), idx, iw2, fw2, buf, rs, False)
        if (bw, bh) != (nxs, nys):
            if not whole or self._warp_gcps is not None:
                # GDALTransformHasFastClone() is false for GCP transformers: the read goes to the
                # bands (warped blocks, resampled) instead of a warp into the buffer.
                return self._fallback_io(grid, idx, iw, fw, buf, rs, routed)
            # A temporary warped VRT of the buffer's size, read whole.
            full = (0, 0, bw, bh)
            return self._dataset_io(grid.rescaled(bw, bh), idx, full, full, buf, rs, False)
        if any(b > self._src_count for b in idx):
            # A band the warp does not write (an added alpha band): read block by block.
            return self._fallback_io(grid, idx, iw, fw, buf, rs, routed)
        win = self._warper(grid).source_window(iw)
        if win is None:
            from terrio.rio.errors import RasterioIOError

            raise RasterioIOError("Too many points failed to transform")
        if (
            self._working_memory(win[2], win[3], nxs, nys) > self._warp_memory
            and nxs >= 2
            and nys >= 2
        ):
            # More working memory than the warp allows: halves, along the longest side.
            if nxs >= nys:
                half = nxs // 2
                parts = [(nx, ny, half, nys), (nx + half, ny, nxs - half, nys)]
                axis = 2
            else:
                half = nys // 2
                parts = [(nx, ny, nxs, half), (nx, ny + half, nxs, nys - half)]
                axis = 1
            return np.concatenate(
                [self._dataset_io(grid, idx, p, p, (p[3], p[2]), rs, routed) for p in parts],
                axis=axis,
            )
        out = self._warp_region(grid, iw, win, direct=True)
        return out[[b - 1 for b in idx]]

    def _fallback_io(
        self,
        grid: _Grid,
        idx: list[int],
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: Resampling,
        routed: bool,
    ) -> np.ndarray:
        """`GDALDataset::IRasterIO()`: the request handed to the bands.

        Args:
            grid: The grid.
            idx: The bands.
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The resampling method.
            routed: See `_dataset_io`.

        Returns:
            numpy.ndarray: `(bands, height, width)`.
        """
        nx, ny, nxs, nys = iw
        bh, bw = buf
        if (
            rs in _KERNELS
            and len(idx) > 1
            and bw < nxs
            and bh < nys
            and grid.full
            and self._overview_sizes
        ):
            moved = self._to_overview(grid, iw, fw, buf, rs, True)
            if moved is not None:
                ovr, iw2, fw2 = moved
                return self._dataset_io(self._ovr_grid(ovr), idx, iw2, fw2, buf, rs, False)
        return self._band_io(grid, idx, iw, fw, buf, rs, routed)

    def _band_io(
        self,
        grid: _Grid,
        idx: list[int],
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: Resampling,
        routed: bool,
    ) -> np.ndarray:
        """`VRTWarpedRasterBand::IRasterIO()`, then `GDALRasterBand::IRasterIO()`.

        Args:
            grid: The grid.
            idx: The bands.
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The resampling method.
            routed: See `_dataset_io`.

        Returns:
            numpy.ndarray: `(bands, height, width)`.
        """
        if self._count == 1 and not routed:
            # A one-band VRT's band hands the read to the dataset.
            return self._dataset_io(grid, idx, iw, fw, buf, rs, True)
        nx, ny, nxs, nys = iw
        bh, bw = buf
        if (bw < nxs or bh < nys) and grid.full and self._overview_sizes:
            moved = self._to_overview(grid, iw, fw, buf, rs, False)
            if moved is not None:
                ovr, iw2, fw2 = moved
                return self._band_io(self._ovr_grid(ovr), idx, iw2, fw2, buf, rs, False)

        def read(bands: Any, window: tuple[int, int, int, int], level: int | None) -> Any:
            return self._grid_window(
                grid if level is None else self._ovr_grid(level), bands, window
            )

        nodata = self.nodatavals[idx[0] - 1] if self._count else None
        overviews = self._overview_sizes if grid.full else []
        return raster_io(read, idx, (grid.width, grid.height), overviews, fw, buf, int(rs), nodata)

    def _to_overview(
        self,
        grid: _Grid,
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: Resampling,
        dataset: bool,
    ) -> tuple[int, tuple[int, int, int, int], tuple[float, float, float, float]] | None:
        """The implicit overview a decimating read uses, and the window in its pixels.

        Dataset reads choose with `GDALBandGetBestOverviewLevel2()`; band reads with
        `VRTWarpedRasterBand::GetBestOverviewLevel()` (a threshold of 1.01 rather than 1.0, no
        epsilon, the last fitting overview).

        Args:
            grid: The VRT's grid.
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The resampling method.
            dataset: Whether the dataset (else a band) chooses.

        Returns:
            tuple | None: The overview and the integer and floating-point windows in its pixels.
        """
        nx, ny, nxs, nys = iw
        bh, bw = buf
        nearest = rs == Resampling.nearest
        if dataset:
            ovr = best_overview(
                self._overview_sizes, grid.width, grid.height, nxs, nys, bw, bh, nearest
            )
        else:
            xr, yr = nxs / bw, nys / bh
            desired = xr if (xr < yr or bh == 1) else yr
            threshold = 1.2 if nearest else 1.0
            if threshold == 1.0:
                threshold += 1e-2
            ovr, best = None, 0.0
            for i, (ow, oh) in enumerate(self._overview_sizes):
                factor = min(grid.width / ow, grid.height / oh)
                if factor >= desired * threshold or factor <= best:
                    continue
                ovr, best = i, factor
        if ovr is None:
            return None
        ow, oh = self._overview_sizes[ovr]
        xf, yf = grid.width / ow, grid.height / oh
        ox = min(ow - 1, int(nx / xf + 0.5))
        oy = min(oh - 1, int(ny / yf + 0.5))
        oxs = max(1, int(nxs / xf + 0.5))
        oys = max(1, int(nys / yf + 0.5))
        oxs = min(oxs, ow - ox)
        oys = min(oys, oh - oy)
        # rasterio always passes a floating-point window: it is scaled to the overview.
        return ovr, (ox, oy, oxs, oys), (fw[0] / xf, fw[1] / yf, fw[2] / xf, fw[3] / yf)

    # Reading API.

    def read(self, indexes: Any = None, *args: Any, **kwargs: Any) -> Any:  # type: ignore[override]
        """Reads bands into an array, as `DatasetReader.read`.

        Boundless reads are refused, as rasterio refuses them.

        Args:
            indexes: The bands.
            *args: `DatasetReader.read`'s arguments.
            **kwargs: `DatasetReader.read`'s keyword arguments.

        Returns:
            numpy.ndarray | numpy.ma.MaskedArray: The pixels.

        Raises:
            ValueError: For a boundless read.
        """
        if kwargs.get("boundless", False):
            raise ValueError("WarpedVRT does not permit boundless reads")
        return super().read(indexes, *args, **kwargs)

    def read_masks(self, indexes: Any = None, *args: Any, **kwargs: Any) -> Any:  # type: ignore[override]
        """Reads band masks, as `DatasetReader.read_masks`; boundless reads are refused.

        Args:
            indexes: The bands.
            *args: `DatasetReader.read_masks`'s arguments.
            **kwargs: `DatasetReader.read_masks`'s keyword arguments.

        Returns:
            numpy.ndarray: The masks.

        Raises:
            ValueError: For a boundless read.
        """
        if kwargs.get("boundless", False):
            raise ValueError("WarpedVRT does not permit boundless reads")
        return super().read_masks(indexes, *args, **kwargs)

    # Read-only: DatasetBase's crs has a setter, the warped VRT's output CRS has none.
    @property  # type: ignore[misc]
    def crs(self) -> Any:
        """The output CRS.

        Returns:
            CRS: The CRS.
        """
        return self.dst_crs

    @property
    def nodatavals(self) -> tuple[Any, ...]:
        """The nodata value of each band, as rasterio reports a warped VRT's: the value it set.

        Returns:
            tuple: The values (None where unset).
        """
        return tuple(self._nodatavals or [])

    def _mask_flags(self) -> tuple[int, ...]:
        """The mask flags of each band, as GDAL's `GetMaskBand()` gives a warped VRT's.

        rasterio sets each band's nodata value as a double: a band masks by it when the value is
        in its type's range (`GDALNoDataMaskBand::IsNoDataInRange()`); an Int64 or UInt64 band,
        whose mask reads only a nodata value set as an integer, has none.

        Returns:
            tuple: The flags.
        """
        alpha = self._mask_alpha()
        out = []
        for b in self.indexes:
            nd = self.nodatavals[b - 1]
            if nd is not None and _masks_by_nodata(float(nd), self._dtype):
                out.append(MaskFlags.nodata.value)
            elif alpha is not None and b != alpha:
                out.append(MaskFlags.per_dataset.value | MaskFlags.alpha.value)
            else:
                out.append(MaskFlags.all_valid.value)
        return tuple(out)

    def close(self) -> None:
        """Closes the view (not the source dataset)."""
        self._blocks.clear()
        self._block_bytes = 0
        self._warpers.clear()
        super(DatasetReader, self).close()

    def start(self) -> None:
        """Opens the view (it is open once created)."""


def _exact(value: float, dtype: str) -> bool:
    """Whether a value converts to a type and back unchanged (`GDALIsValueExactAs()`).

    Args:
        value: The value.
        dtype: A NumPy dtype name.

    Returns:
        bool: NaN is exact in the float types only.
    """
    t = np.dtype(dtype)
    if t.kind == "f":
        if t.itemsize >= 8 or np.isnan(value):
            return True
        with np.errstate(over="ignore"):
            return bool(float(t.type(value)) == value)
    if t.kind in "iu":
        info = np.iinfo(t)
        return bool(info.min <= value <= info.max and float(int(value)) == value)
    return True


def _masks_by_nodata(value: float, dtype: str) -> bool:
    """Whether a band of a type masks by a nodata value set as a double, as GDAL's do.

    Args:
        value: The nodata value.
        dtype: The band's NumPy dtype name.

    Returns:
        bool: False for Int64 and UInt64 bands; else whether the value is in the type's range
        (NaN and infinities in the float types').
    """
    if dtype in ("int64", "uint64"):
        return False
    t = np.dtype(dtype)
    if t.kind == "f":
        if t.itemsize == 8 or np.isnan(value) or np.isinf(value):
            return True
        return bool(-np.finfo(t).max <= value <= np.finfo(t).max)
    if t.kind in "iu":
        return bool(np.iinfo(t).min <= value <= np.iinfo(t).max)
    return True


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
