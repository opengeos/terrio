# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/profiles.py

"""Dataset creation profiles."""

from __future__ import annotations

from collections import UserDict
from typing import Any, ClassVar

from terrio.rio.dtypes import uint8


class Profile(UserDict[str, Any]):
    """Keyword arguments for `terrio.rio.open()` in write mode, with defaults."""

    defaults: ClassVar[dict[str, Any]] = {}

    def __init__(self, data: dict[str, Any] | None = None, **kwds: Any) -> None:
        """Creates a profile: the class defaults, updated with `data` and keywords.

        Args:
            data: Initial items.
            **kwds: More items.
        """
        UserDict.__init__(self)
        init = dict(self.defaults)
        init.update(data or {})
        init.update(**kwds)
        self.data.update(init)

    def __getitem__(self, key: str) -> Any:
        """An item.

        Args:
            key: The key.

        Returns:
            The value.
        """
        return self.data[key]

    def __setitem__(self, key: str, val: Any) -> None:
        """Sets an item; the `affine` key is not allowed.

        Args:
            key: The key.
            val: The value.

        Raises:
            TypeError: If the key is `affine`.
        """
        if key == "affine":
            raise TypeError("affine key is prohibited")
        self.data[key] = val


class DefaultGTiffProfile(Profile):
    """A tiled, LZW-compressed Byte GeoTIFF with nodata 0."""

    defaults: ClassVar[dict[str, Any]] = {
        "driver": "GTiff",
        "interleave": "band",
        "tiled": True,
        "blockxsize": 256,
        "blockysize": 256,
        "compress": "lzw",
        "nodata": 0,
        "dtype": uint8,
    }


default_gtiff_profile = DefaultGTiffProfile()

__all__ = ["DefaultGTiffProfile", "Profile", "default_gtiff_profile"]
