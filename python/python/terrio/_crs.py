# SPDX-FileCopyrightText: Copyright 2007, Mateusz Loskot
# SPDX-FileCopyrightText: Copyright 2008-2024, Even Rouault <even.rouault at spatialys.com>
# SPDX-FileCopyrightText: Copyright (c) 2007, Mateusz Loskot
# SPDX-FileCopyrightText: Copyright (c) 2008-2017, Even Rouault <even dot rouault at spatialys dot com>
# SPDX-FileCopyrightText: Copyright (c) 2020-2024 Brendan C. Ward and pyogrio contributors
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (translated to Python and modified):
#   GDAL v3.12.1:
#     ogr/ogrlibjsonutils.cpp
#     ogr/ogrsf_frmts/geojson/ogrgeojsonreader.cpp
#   pyogrio 0.13.0:
#     pyogrio/raw.py

"""CRS validation shared by Python dataframe and Arrow exports."""

# The attribution above follows the normalization helper moved from _read.py.
# Malformed-definition fallback and dynamic datum retention are Terrio policy,
# validated with GDAL 3.13.3 and pyproj, rather than translations of GDAL code.

import re

from terrio import _terrio
from terrio import errors as _terrio_errors
from terrio._warnings import warn_user


def dataframe_crs(
    crs: str | None, normalize: bool = True, fallback: str | None = None
) -> str | None:
    """The CRS as pyogrio gives it.

    `EPSG:<code>` when GDAL identifies an EPSG code, otherwise GDAL's WKT1 (here PROJ's
    `WKT1_GDAL` export, which names axes as GDAL does), when it preserves the
    definition. Dynamic and WKT2-only definitions retain their original representation.

    An `EPSG:<code>` that terrio knows is given as it is, without pyproj. A definition pyproj
    cannot parse, an `EPSG:<code>` neither knows included, is `None`, with a warning that quotes
    it, since a GeoDataFrame refuses it; without pyproj, or with one that fails to import, the
    definition is given as it is. With ``normalize=False``, only validate it and
    return its original representation. A stored fallback is tried before warning
    when the preferred authority code or definition cannot be parsed.
    """
    if crs is None:
        return crs
    # terrio identified the code, or a `.prj` names one terrio knows: no need for pyproj. An
    # `EPSG:<digits>` reference is looked up in terrio's database, never read as a file name.
    if re.fullmatch(r"EPSG:[0-9]{1,9}", crs):
        try:
            _terrio.crs_info(crs)
        except _terrio_errors.CRSError:
            pass
        else:
            return crs
    try:
        from pyproj import CRS
    except Exception:  # noqa: BLE001
        return crs
    candidates = (crs, fallback) if fallback and fallback != crs else (crs,)
    for candidate in candidates:
        try:
            parsed = CRS.from_user_input(candidate)
        except Exception:  # noqa: BLE001
            continue
        crs = candidate
        break
    else:
        # Report one failure only after every available definition was rejected.
        quoted = "".join(c if c.isprintable() else " " for c in crs.strip()[:80])
        warn_user(
            f"the layer's CRS definition cannot be parsed ('{quoted}'): it is read without a CRS",
        )
        return None
    if not normalize or crs.startswith("EPSG:"):
        return crs
    try:
        datum = parsed.datum
        if datum is not None and datum.type_name.startswith("Dynamic"):
            # WKT1 can silently discard a dynamic datum's frame reference epoch.
            return crs
        return parsed.to_wkt("WKT1_GDAL") or crs
    except Exception:  # noqa: BLE001
        # A valid WKT2-only CRS remains usable even if WKT1 cannot express it.
        return crs
