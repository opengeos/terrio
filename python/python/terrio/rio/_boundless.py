# SPDX-FileCopyrightText: Copyright (c) 2001, Frank Warmerdam <warmerdam@pobox.com>
# SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 1998, 2003, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys dot com>
# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (translated to Python and modified):
#   GDAL v3.12.1:
#     frmts/vrt/vrtsources.cpp
#     frmts/vrt/vrtsourcedrasterband.cpp
#     frmts/vrt/vrtdataset.cpp
#     gcore/gdaldataset.cpp
#     gcore/rasterio.cpp
#     gcore/gdalrescaledalphaband.cpp
#   rasterio 1.5.0:
#     rasterio/vrt.py
#     rasterio/_io.pyx

"""Boundless reads as rasterio 1.5 makes them: through a VRT read by GDAL 3.12.

rasterio's `DatasetReaderBase.read(boundless=True)` wraps the dataset in a VRT
(`rasterio.vrt._boundless_vrt_doc`): one band per band, each with a `ComplexSource` placing the
whole dataset at the window's offset (with the read's resampling, and the fill value as nodata),
plus the dataset's per-dataset mask as the VRT's mask band. It reads that VRT with nearest
neighbour. This module follows GDAL's code for that read:

- `GDALDataset::IRasterIO()` reads the bands one by one (`BandBasedRasterIO()`), each through
  `VRTSourcedRasterBand::IRasterIO()`;
- the first band to decimate builds the VRT's virtual overviews (`BuildVirtualOverviews()`) and
  reads through them when one fits (`GDALBandGetBestOverviewLevel2()`); the bands after it do
  not look at them, and read the source at full resolution;
- a band forwards the read to its source (`CanIRasterIOBeForwardedToEachSource()`):
  `VRTComplexSource::RasterIO()` maps the window to the source (`GetSrcDstWindow()`) and reads
  it as GDAL's `RasterIO` does, over the background (the VRT's nodata value, or 0);
- or, resampling with another method than nearest neighbour over a masked band, it reads the VRT
  at full resolution and resamples it with the mask (`GDALRasterBand::IRasterIO()`);
- masks come from the VRT's mask band (the source's mask, mapped the same way), its nodata
  value, or its alpha band.
"""

from __future__ import annotations

import math
from collections.abc import Sequence
from typing import Any

import numpy as np

from terrio.rio._resample import (
    GDAL_KERNELS,
    RESAMPLING_NAMES,
    best_overview,
    gdal_cast,
    is_nodata,
    raster_io,
)
from terrio.rio.enums import ColorInterp, MaskFlags

# GDAL's default VRT block size, below which no virtual overview is made without a source match.
_DEFAULT_BLOCK_SIZE = 128


def _round_if_close(v: float) -> float:
    """`RoundIfCloseToInt()`: a value within 1e-3 of an integer becomes it.

    Args:
        v: The value.

    Returns:
        float: The value.
    """
    c = math.floor(v + 0.5)
    return float(c) if abs(v - c) < 1e-3 else v


class _Source:
    """The window geometry of a `VRTSimpleSource` (or `VRTComplexSource`)."""

    def __init__(
        self,
        src: tuple[float, float, float, float],
        dst: tuple[float, float, float, float],
        exact: bool = False,
    ) -> None:
        f = (lambda v: v) if exact else _round_if_close
        self.sx, self.sy, self.sw, self.sh = (f(float(v)) for v in src)
        self.dx, self.dy, self.dw, self.dh = (f(float(v)) for v in dst)

    def scaled(self, rx: float, ry: float) -> _Source:
        """The source of a virtual overview (the `VRTSimpleSource` ratio constructor).

        Args:
            rx: The overview's x ratio.
            ry: The overview's y ratio.

        Returns:
            _Source: The source.
        """
        return _Source(
            (self.sx, self.sy, self.sw, self.sh),
            (self.dx * rx, self.dy * ry, self.dw * rx, self.dh * ry),
            exact=True,
        )

    def window(
        self,
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        nearest: bool,
        band_size: tuple[int, int],
    ) -> Any:
        """`VRTSimpleSource::GetSrcDstWindow()`.

        Args:
            fw: The requested window in the VRT `(x, y, w, h)` (floating point).
            buf: The buffer `(height, width)`.
            nearest: Whether the source is read with nearest neighbour.
            band_size: The source band's `(width, height)`.

        Returns:
            tuple | None: The floating-point and integer source windows and the window in the
            buffer `(x, y, w, h)`, or None when the source does not contribute.
        """
        x_off, y_off, x_size, y_size = fw
        bh, bw = buf
        bwid, bhei = band_size
        if self.sw == 0 or self.sh == 0 or self.dw == 0 or self.dh == 0:
            return None
        if (
            x_off >= self.dx + self.dw
            or y_off >= self.dy + self.dh
            or x_off + x_size <= self.dx
            or y_off + y_size <= self.dy
        ):
            return None
        out = [0, 0, bw, bh]
        mod_x = mod_y = False
        rx, ry, rxs, rys = x_off, y_off, x_size, y_size
        if rx < self.dx:
            rxs = rxs + rx - self.dx
            rx = self.dx
            mod_x = True
        if ry < self.dy:
            rys = rys + ry - self.dy
            ry = self.dy
            mod_y = True
        if rx + rxs > self.dx + self.dw:
            rxs = self.dx + self.dw - rx
            mod_x = True
        if ry + rys > self.dy + self.dh:
            rys = self.dy + self.dh - ry
            mod_y = True
        scale_x = self.sw / self.dw
        scale_y = self.sh / self.dh
        qx = (rx - self.dx) * scale_x + self.sx
        qy = (ry - self.dy) * scale_y + self.sy
        qxs = rxs * scale_x
        qys = rys * scale_y
        if not all(map(math.isfinite, (qx, qy, qxs, qys))) or qxs < 0 or qys < 0:
            return None
        if qx < 0:
            qxs += qx
            qx = 0.0
            mod_x = True
        if qy < 0:
            qys += qy
            qy = 0.0
            mod_y = True
        eps10 = 1e-10
        if nearest and (
            abs(self.sx - round(self.sx)) > eps10
            or abs(self.sy - round(self.sy)) > eps10
            or abs(self.dx - round(self.dx)) > eps10
            or abs(self.dy - round(self.dy)) > eps10
        ):
            nx = int(qx + 0.5 + eps10)
            ny = int(qy + 0.5 + eps10)
        else:
            nx = int(qx)
            ny = int(qy)
        eps = 1e-3
        if qx - nx > 1.0 - eps:
            nx += 1
            qx = float(nx)
        if qy - ny > 1.0 - eps:
            ny += 1
            qy = float(ny)
        nxs = int(math.floor(qxs + 0.5))
        nys = int(math.floor(qys + 0.5))
        nxs = max(nxs, 1)
        nys = max(nys, 1)
        if nx + nxs > bwid:
            nxs = bwid - nx
            mod_x = True
        if qx + qxs > bwid:
            qxs = bwid - qx
            mod_x = True
        if ny + nys > bhei:
            nys = bhei - ny
            mod_y = True
        if qy + qys > bhei:
            qys = bhei - qy
            mod_y = True
        if nx >= bwid or ny >= bhei or nxs <= 0 or nys <= 0:
            return None
        if mod_x or mod_y:
            ulx = ((qx - self.sx) / self.sw) * self.dw + self.dx
            uly = ((qy - self.sy) / self.sh) * self.dh + self.dy
            lrx = ((qx + qxs - self.sx) / self.sw) * self.dw + self.dx
            lry = ((qy + qys - self.sy) / self.sh) * self.dh + self.dy
            if mod_x:
                s = bw / x_size
                ox = (ulx - x_off) * s
                out[0] = 0 if ox <= 0 else int(ox + eps)
                d = (ox - out[0]) / s / self.dw * self.sw
                qx -= d
                qxs += d
                orx = (lrx - x_off) * s
                if orx < ox:
                    return None
                nr = int(math.ceil(orx - eps))
                out[2] = nr - out[0]
                if out[0] + out[2] > bw:
                    out[2] = bw - out[0]
                qxs += (nr - orx) / s / self.dw * self.sw
            if mod_y:
                s = bh / y_size
                oy = (uly - y_off) * s
                out[1] = 0 if oy <= 0 else int(oy + eps)
                d = (oy - out[1]) / s / self.dh * self.sh
                qy -= d
                qys += d
                ory = (lry - y_off) * s
                if ory < oy:
                    return None
                nr = int(math.ceil(ory - eps))
                out[3] = nr - out[1]
                if out[1] + out[3] > bh:
                    out[3] = bh - out[1]
                qys += (nr - ory) / s / self.dh * self.sh
            if out[2] < 1 or out[3] < 1:
                return None
        fq = tuple(_round_if_close(v) for v in (qx, qy, qxs, qys))
        return fq, (nx, ny, nxs, nys), tuple(out)


class _VRT:
    """rasterio's boundless VRT (or one of its virtual overviews) over a dataset."""

    def __init__(
        self,
        ds: Any,
        size: tuple[int, int],
        source: _Source,
        mask_source: _Source | None,
        resampling: int,
        nodata: float | None,
        src_nodata: float | None,
        constant: bool,
        cache: dict[Any, Any] | None = None,
        bands: Sequence[int] = (),
    ) -> None:
        # Source reads of one boundless read, shared by its bands and virtual overviews: GDAL
        # reads each band on its own, but the pixels of a window are the same.
        self.cache: dict[Any, Any] = {} if cache is None else cache
        self.bands = list(bands)
        self.ds = ds
        # The VRT bands' color interpretations: `_boundless_vrt_doc()` copies the source's.
        self.colorinterp = list(ds.colorinterp)
        self.width, self.height = size
        self.source = source
        self.mask_source = mask_source
        self.resampling = resampling
        self.nodata = nodata
        self.src_nodata = src_nodata
        self.constant = constant
        self.dst_to_src = source.dw / source.sw
        self._overviews: list[_VRT] | None = None

    # Virtual overviews.

    def overviews(self) -> list[_VRT]:
        """`VRTDataset::BuildVirtualOverviews()`: one VRT per fitting source overview.

        Returns:
            list: The overviews (empty when the source has none).
        """
        if self._overviews is not None:
            return self._overviews
        ds = self.ds
        src_ovr = list(ds._overview_sizes)
        out: list[_VRT] = []
        if src_ovr:
            sizes = {(ds.width, ds.height), *map(tuple, src_ovr)}
            for ow, oh in src_ovr:
                rx, ry = ow / ds.width, oh / ds.height
                if rx >= self.source.dw / self.source.sw or ry >= self.source.dh / self.source.sh:
                    continue
                vw = int(0.5 + self.width * rx)
                vh = int(0.5 + self.height * ry)
                match = next(
                    (s for s in sorted(sizes) if abs(s[0] - vw) <= 1 and abs(s[1] - vh) <= 1),
                    None,
                )
                if match is None and (vw < _DEFAULT_BLOCK_SIZE or vh < _DEFAULT_BLOCK_SIZE):
                    break
                if match is not None:
                    vw, vh = match
                ms = None if self.mask_source is None else self.mask_source.scaled(rx, ry)
                # The overview's sources are copies without the `resampling` attribute (the
                # VRTComplexSource copy constructor leaves m_osResampling empty): they read with
                # the request's method, nearest neighbour for rasterio's VRT read.
                out.append(
                    _VRT(
                        ds,
                        (vw, vh),
                        self.source.scaled(rx, ry),
                        ms,
                        0,
                        self.nodata,
                        self.src_nodata,
                        self.constant,
                        self.cache,
                        self.bands,
                    )
                )
        # Built once only when it made overviews (GDAL leaves the list empty otherwise).
        if out:
            self._overviews = out
        return out

    def built(self) -> bool:
        """Whether the virtual overviews were built.

        Returns:
            bool: True once built.
        """
        return self._overviews is not None

    # Masks.

    def mask_flags(self, count: int, colorinterp: Sequence[ColorInterp], band: int) -> int:
        """The VRT band's GDAL mask flags.

        Args:
            count: The number of bands.
            colorinterp: The bands' color interpretations.
            band: The band.

        Returns:
            int: The flags.
        """
        if self.mask_source is not None:
            return MaskFlags.per_dataset.value
        if self.nodata is not None:
            return MaskFlags.nodata.value
        if self.alpha_band(count, colorinterp) is not None and band != count:
            return MaskFlags.alpha.value | MaskFlags.per_dataset.value
        return MaskFlags.all_valid.value

    def alpha_band(self, count: int, colorinterp: Sequence[ColorInterp]) -> int | None:
        """The band GDAL takes as the other bands' alpha mask (`GetMaskBand()`).

        Args:
            count: The number of bands.
            colorinterp: The bands' color interpretations.

        Returns:
            int | None: Band 2 of two or band 4 of four, when its color interpretation is alpha
            and its type Byte or UInt16.
        """
        if count in (2, 4) and colorinterp[count - 1] == ColorInterp.alpha:
            if np.dtype(self.ds._dtype) in (np.dtype(np.uint8), np.dtype(np.uint16)):
                return count
        return None

    # Reading.

    def alpha_mask_io(
        self,
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        dtype: Any,
    ) -> np.ndarray:
        """Reads the alpha band as the other bands' mask band (`GDALRasterBand::GetMaskBand()`).

        A Byte alpha band is the mask band. A UInt16 one is wrapped in a `GDALRescaledAlphaBand`,
        which has no overviews: read at the window's integer size, it takes the alpha band's rows
        at nominal resolution (`GDALRescaledAlphaBand::IRasterIO()`); any other read goes
        through `GDALRasterBand::IRasterIO()`, nearest neighbour over the alpha band at nominal
        resolution.

        Args:
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            dtype: The bands' data type.

        Returns:
            numpy.ndarray: uint8 mask values.
        """
        alpha = self.alpha_band(self.ds.count, self.colorinterp)
        assert alpha is not None
        valid = MaskFlags.all_valid.value
        if np.dtype(dtype) == np.uint8:
            return self.band_io(alpha, iw, fw, buf, 0, dtype, valid)

        def read(bands: Any, w: tuple[int, int, int, int], level: int | None) -> Any:
            # GDAL reads the alpha band row by row (or block by block); at nominal resolution a
            # pixel's value depends on its position only, so the window is read at once.
            full = (float(w[0]), float(w[1]), float(w[2]), float(w[3]))
            return _alpha_mask(self.band_io(alpha, w, full, (w[3], w[2]), 0, dtype, valid))[
                np.newaxis
            ]

        if buf == (iw[3], iw[2]):
            return np.asarray(read(None, iw, None))[0]
        return np.asarray(
            raster_io(read, [1], (self.width, self.height), [], fw, buf, 0, None, iwin=iw)
        )[0]

    def _source_read(
        self,
        band: int | None,
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        resampling: int,
    ) -> np.ndarray:
        """A source band (or its mask, `band` None) read as GDAL's `RasterIO` reads it.

        Args:
            band: The band, or None for the per-dataset mask.
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            resampling: The resampling method.

        Returns:
            numpy.ndarray: `(height, width)`.
        """
        ds = self.ds
        key = (band is None, iw, fw, buf, resampling)
        hit = self.cache.get(key)
        if hit is not None:
            return hit[0] if band is None else hit[self.bands.index(band)]
        if band is None:
            raster = ds._raster

            def read(bands: Any, w: tuple[int, int, int, int], level: int | None) -> Any:
                m = raster.read_mask(window=w, overview=level)
                if m is None:
                    return np.full((1, w[3], w[2]), 255, np.uint8)
                return np.asarray(m)[np.newaxis]

            got = np.asarray(
                raster_io(
                    read,
                    [1],
                    (ds.width, ds.height),
                    ds._overview_sizes,
                    fw,
                    buf,
                    resampling,
                    None,
                    iwin=iw,
                )
            )
            self.cache[key] = got
            return got[0]
        bands = self.bands if band in self.bands else [band]
        nodata = ds.nodatavals[band - 1]
        got = np.asarray(
            raster_io(
                ds._read_level,
                bands,
                (ds.width, ds.height),
                ds._overview_sizes,
                fw,
                buf,
                resampling,
                nodata,
                iwin=iw,
                masks=ds._mask_reader(),
                block_width=ds.block_shapes[0][1],
            )
        )
        if bands is self.bands:
            self.cache[key] = got
        return got[bands.index(band)]

    def _background(self, buf: tuple[int, int], dtype: Any, mask_band: bool = False) -> np.ndarray:
        """The buffer before the sources are drawn: the VRT band's nodata value, else 0.

        The VRT's mask band (`<MaskBand>`) has no nodata value of its own: it starts at 0
        (invalid) whatever the fill value the data bands carry.

        Args:
            buf: The buffer `(height, width)`.
            dtype: The buffer's data type.
            mask_band: Whether the buffer is the mask band's.

        Returns:
            numpy.ndarray: The buffer.
        """
        v = 0.0 if self.nodata is None or mask_band else float(self.nodata)
        return np.full(buf, gdal_cast(np.array([v]), dtype)[0], dtype)

    def band_io(
        self,
        band: int | None,
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: int,
        dtype: Any,
        flags: int,
    ) -> np.ndarray:
        """`VRTSourcedRasterBand::IRasterIO()` on a band (None: the mask band).

        Args:
            band: The band, or None for the VRT's mask band.
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The request's resampling method.
            dtype: The band's data type.
            flags: The band's mask flags.

        Returns:
            numpy.ndarray: `(height, width)`.
        """
        nx, ny, nxs, nys = iw
        bh, bw = buf
        if not self.built() and (bw < nxs or bh < nys) and self.overviews():
            got = self._overview_io(band, iw, fw, buf, rs, dtype, flags)
            if got is not None:
                return got
        source = self.source if band is not None else self.mask_source
        assert source is not None
        src_rs = self.resampling if band is not None else rs
        is_alpha = band is not None and self.colorinterp[band - 1] == ColorInterp.alpha
        if band is not None and not self._forwardable(iw, fw, buf, rs, flags, is_alpha):
            return self._generic(band, iw, fw, buf, src_rs, dtype, flags)
        out = self._background(buf, dtype, band is None)
        nearest = src_rs == 0
        got = source.window(fw, buf, nearest, (self.ds.width, self.ds.height))
        if got is None:
            return out
        fq, nq, o = got
        ox, oy, ow, oh = o
        if band is not None and self.constant:
            vals = np.full((oh, ow), 255.0)
            valid = np.ones((oh, ow), bool)
            if self.src_nodata is not None:
                raw = self._source_read(band, nq, fq, (oh, ow), src_rs)
                valid = ~is_nodata(raw, self.src_nodata)
        else:
            raw = self._source_read(band, nq, fq, (oh, ow), src_rs)
            vals = raw
            valid = np.ones(raw.shape, bool)
            if band is not None and self.src_nodata is not None:
                valid = ~is_nodata(raw, self.src_nodata)
        region = out[oy : oy + oh, ox : ox + ow]
        region[valid] = gdal_cast(np.asarray(vals)[valid], dtype)
        return out

    def _overview_io(
        self,
        band: int | None,
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: int,
        dtype: Any,
        flags: int,
    ) -> Any:
        """`GDALRasterBand::OverviewRasterIO()` through the virtual overviews.

        Args:
            band: The band (None: the mask band).
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The request's resampling method.
            dtype: The band's data type.
            flags: The band's mask flags.

        Returns:
            numpy.ndarray | None: The pixels, or None without a fitting overview.
        """
        ovs = self.overviews()
        sizes = [(o.width, o.height) for o in ovs]
        nx, ny, nxs, nys = iw
        bh, bw = buf
        i = best_overview(sizes, self.width, self.height, nxs, nys, bw, bh, rs == 0)
        if i is None:
            return None
        ow, oh = sizes[i]
        xf, yf = self.width / ow, self.height / oh
        ox = min(ow - 1, int(nx / xf + 0.5))
        oy = min(oh - 1, int(ny / yf + 0.5))
        oxs = max(1, int(nxs / xf + 0.5))
        oys = max(1, int(nys / yf + 0.5))
        oxs = min(oxs, ow - ox)
        oys = min(oys, oh - oy)
        fw2 = (fw[0] / xf, fw[1] / yf, fw[2] / xf, fw[3] / yf)
        return ovs[i].band_io(band, (ox, oy, oxs, oys), fw2, buf, rs, dtype, flags)

    def _forwardable(
        self,
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: int,
        flags: int,
        is_alpha: bool = False,
    ) -> bool:
        """`VRTSourcedRasterBand::CanIRasterIOBeForwardedToEachSource()`.

        Args:
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The request's resampling method.
            flags: The band's mask flags.
            is_alpha: Whether the band's color interpretation is alpha, which makes it a mask
                band to `VRTRasterBand::IsMaskBand()`.

        Returns:
            bool: Whether the read goes to the source.
        """
        nx, ny, nxs, nys = iw
        bh, bw = buf
        if (bw, bh) == (nxs, nys) or (rs == 0 and self.resampling == 0):
            return True
        if self.resampling != 0 and (
            flags not in (MaskFlags.all_valid.value, MaskFlags.nodata.value) or is_alpha
        ):
            return False
        got = self.source.window(fw, buf, rs == 0, (self.ds.width, self.ds.height))
        if got is None:
            return True
        _, _, o = got
        if self.nodata is not None:
            nd = self.ds.nodata
            if nd is None or nd != self.nodata:
                return False
        down = bw < nxs and bh < nys
        if down and not self.ds._overview_sizes and o != (0, 0, bw, bh):
            return False
        return True

    def _generic(
        self,
        band: int,
        iw: tuple[int, int, int, int],
        fw: tuple[float, float, float, float],
        buf: tuple[int, int],
        rs: int,
        dtype: Any,
        flags: int = MaskFlags.all_valid.value,
    ) -> np.ndarray:
        """`GDALRasterBand::IRasterIO()` on the VRT band: the VRT resampled at full resolution.

        Pixels its mask (or nodata value) invalidates are left out.

        Args:
            band: The band.
            iw: The integer window.
            fw: The floating-point window.
            buf: The buffer `(height, width)`.
            rs: The resampling method.
            dtype: The band's data type.
            flags: The band's mask flags.

        Returns:
            numpy.ndarray: `(height, width)`.
        """
        vrt = self
        alpha = self.alpha_band(self.ds.count, self.colorinterp)

        def alpha_mask(bands: Any, w: tuple[int, int, int, int], level: int | None) -> Any:
            # The VRT's alpha band, at nominal resolution, as the mask band of the others.
            full = (float(w[0]), float(w[1]), float(w[2]), float(w[3]))
            mkey = ("generic alpha", w)
            m = vrt.cache.get(mkey)
            if m is None:
                m = vrt.alpha_mask_io(w, full, (w[3], w[2]), dtype)
                vrt.cache[mkey] = m
            return np.asarray(m)[np.newaxis]

        def mask(bands: Any, w: tuple[int, int, int, int], level: int | None) -> Any:
            full = (float(w[0]), float(w[1]), float(w[2]), float(w[3]))
            mkey = ("generic mask", w)
            m = vrt.cache.get(mkey)
            if m is None:
                m = vrt.band_io(None, w, full, (w[3], w[2]), 0, np.uint8, 0)
                vrt.cache[mkey] = m
            return np.asarray(m)[np.newaxis]

        if RESAMPLING_NAMES.get(rs) in GDAL_KERNELS:
            # `GDALRasterBand::RasterIOResampled()` on the VRT band: its pixels read at nominal
            # resolution, its mask band (else its nodata value) as the validity, its blocks
            # (`VRTRasterBand`'s default, 128 pixels wide).
            def values(bands: Any, w: tuple[int, int, int, int], level: int | None) -> Any:
                full = (float(w[0]), float(w[1]), float(w[2]), float(w[3]))
                return vrt.band_io(
                    band, w, full, (w[3], w[2]), 0, dtype, MaskFlags.all_valid.value
                )[np.newaxis]

            return np.asarray(
                raster_io(
                    values,
                    [1],
                    (self.width, self.height),
                    [],
                    fw,
                    buf,
                    rs,
                    self.nodata,
                    iwin=iw,
                    masks=mask
                    if self.mask_source is not None
                    else (alpha_mask if flags & MaskFlags.alpha.value and alpha else None),
                    block_width=min(_DEFAULT_BLOCK_SIZE, self.width),
                )
            )[0]

        def read(bands: Any, w: tuple[int, int, int, int], level: int | None) -> Any:
            full = (float(w[0]), float(w[1]), float(w[2]), float(w[3]))
            vals = vrt.band_io(band, w, full, (w[3], w[2]), 0, dtype, MaskFlags.all_valid.value)
            if vrt.mask_source is not None:
                m = mask(bands, w, level)[0]
                if vals.dtype.kind in "iu" and np.dtype(vals.dtype).itemsize < 4:
                    wide = vals.astype(np.int32)
                    wide[m == 0] = _MASKED
                    return wide[np.newaxis]
            return vals[np.newaxis]

        nodata = self.nodata
        if self.mask_source is not None and np.dtype(dtype).kind in "iu":
            nodata = float(_MASKED)
        out = np.asarray(
            raster_io(read, [1], (self.width, self.height), [], fw, buf, rs, nodata, iwin=iw)
        )[0]
        if out.dtype != np.dtype(dtype):
            out = gdal_cast(np.where(out == _MASKED, 0, out), dtype)
        return out


# A value no 8- or 16-bit pixel takes: marks masked pixels while resampling.
_MASKED = -(2**30)


def _alpha_mask(a: np.ndarray) -> np.ndarray:
    """An alpha band's values as GDAL's mask band gives them.

    A Byte alpha band is its own mask; a UInt16 one is rescaled to 0..255, non-zero values
    staying non-zero (`GDALRescaledAlphaBand::IRasterIO()`).

    Args:
        a: The alpha values.

    Returns:
        numpy.ndarray: uint8 mask values.
    """
    if a.dtype == np.uint8:
        return a
    v = a.astype(np.int64)
    return np.where((v > 0) & (v < 257), 1, (v * 255) // 65535).astype(np.uint8)


def _vrt(ds: Any, win: Any, nodataval: Any, constant: bool, bands: Sequence[int]) -> _VRT:
    """The VRT of rasterio's `_boundless_vrt_doc()` for a window, as GDAL parses it.

    Args:
        ds: The dataset.
        win: The window.
        nodataval: The fill value, else the read bands' nodata value.
        constant: Whether the sources are replaced by 255 (the mask VRT).
        bands: The bands read.

    Returns:
        _VRT: The VRT.
    """
    t = ds.window_transform(win)
    st = ds.transform
    # XML attributes are written with str() and parsed back with atoi/CPLAtof.
    width = int(float(str(max(ds.width, win.width) + 1)))
    height = int(float(str(max(ds.height, win.height) + 1)))
    dst = (
        float(str((st.xoff - t.xoff) / t.a)),
        float(str((st.yoff - t.yoff) / t.e)),
        float(str(ds.width * st.a / t.a)),
        float(str(ds.height * st.e / t.e)),
    )
    source = _Source((0, 0, ds.width, ds.height), dst)
    mask_source = None
    if (
        all(f & MaskFlags.per_dataset.value for f in ds._mask_flags())
        and getattr(ds, "_raster", None) is not None
    ):
        mask_source = _Source((0, 0, ds.width, ds.height), (dst[0], dst[1], ds.width, ds.height))
    nodata = nodataval if nodataval is not None else ds.nodata
    vrt_nodata = None
    if nodataval is not None or nodata is not None:
        vrt_nodata = float(str(nodataval or nodata))
    return _VRT(
        ds,
        (width, height),
        source,
        mask_source,
        0,
        vrt_nodata,
        None if nodata is None else float(str(nodata)),
        constant,
        None,
        bands,
    )


def boundless_read(
    ds: Any,
    idx: list[int],
    win: Any,
    buf: tuple[int, int],
    resampling: int,
    fill_value: Any,
    ndv: Any,
    masked: bool,
) -> tuple[np.ndarray, Any]:
    """A boundless read as rasterio's `read(boundless=True)` makes it.

    Args:
        ds: The dataset.
        idx: The bands.
        win: The window (any offset and size).
        buf: The output `(height, width)`.
        resampling: The `Resampling` value.
        fill_value: The read's fill value.
        ndv: The last read band's (clamped) nodata value.
        masked: Whether the mask is wanted.

    Returns:
        tuple: The pixels `(bands, h, w)` in the dataset's data type, and the mask (True where
        invalid) or None.
    """
    nodataval = fill_value if fill_value is not None else ndv
    vrt = _vrt(ds, win, nodataval, False, idx)
    vrt.resampling = int(resampling)
    dtype = np.dtype(ds._dtype)
    # rasterio reads Window(0, 0, w, h) of the VRT with nearest neighbour.
    fw = (0.0, 0.0, float(win.width), float(win.height))
    iw = (0, 0, int(max(1, win.width)), int(max(1, win.height)))
    count = ds.count
    ci = list(ds.colorinterp)
    flags = [vrt.mask_flags(count, ci, b) for b in idx]
    pairs = zip(idx, flags, strict=True)
    data = np.stack([vrt.band_io(b, iw, fw, buf, 0, dtype, f) for b, f in pairs])
    if not masked:
        return data, None
    all_valid = all(f == MaskFlags.all_valid.value for f in ds._mask_flags())
    if all_valid:
        mvrt = _vrt(ds, win, nodataval, True, idx)
        mvrt.resampling = int(resampling)
        mvals = np.stack(
            [mvrt.band_io(b, iw, fw, buf, 0, dtype, MaskFlags.all_valid.value) for b in idx]
        )
        return data, ~mvals.astype(bool)
    masks = []
    dataset_mask = None
    for b, f in zip(idx, flags, strict=True):
        if f & MaskFlags.per_dataset.value and vrt.mask_source is not None:
            if dataset_mask is None:
                dataset_mask = vrt.band_io(None, iw, fw, buf, 0, np.uint8, 0)
            m = dataset_mask
        elif f & MaskFlags.nodata.value:
            vals = vrt.band_io(b, iw, fw, buf, 0, dtype, f)
            m = np.where(is_nodata(vals, float(vrt.nodata)), 0, 255)  # type: ignore[arg-type]
        elif f & MaskFlags.alpha.value:
            m = vrt.alpha_mask_io(iw, fw, buf, dtype)
        else:
            m = np.full(buf, 255)
        masks.append(np.asarray(m) == 0)
    return data, np.stack(masks)


__all__ = ["boundless_read"]
