"""The structure and metadata of a raster as GDAL reports them.

Block shapes, compression, interleaving, color interpretation, color tables, tags, band
descriptions, scales, offsets and units, from a GeoTIFF's tags (and its `.aux.xml` sidecar, as
GDAL reads it).
"""

from __future__ import annotations

import contextlib
import os
import re
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from typing import Any

from terrio import _terrio
from terrio.rio._jpeg_quality import guess_jpeg_quality
from terrio.rio.enums import ColorInterp

# TIFF tags.
NEW_SUBFILE_TYPE = 254
WIDTH = 256
HEIGHT = 257
BITS = 258
COMPRESSION = 259
PHOTOMETRIC = 262
SAMPLES = 277
ROWS_PER_STRIP = 278
PLANAR = 284
COLOR_MAP = 320
JPEG_TABLES = 347
TILE_WIDTH = 322
TILE_LENGTH = 323
EXTRA_SAMPLES = 338
SAMPLE_FORMAT = 339
INK_SET = 332
PIXEL_SCALE = 33550
TIEPOINTS = 33922
MODEL_TRANSFORMATION = 34264
GEO_KEYS = 34735
RPC_COEFFICIENTS = 50844
GDAL_METADATA = 42112
LERC_PARAMETERS = 50674

_COMPRESSIONS = {
    2: "CCITTRLE",
    3: "CCITTFAX3",
    4: "CCITTFAX4",
    5: "LZW",
    6: "OJPEG",
    7: "JPEG",
    8: "DEFLATE",
    32946: "DEFLATE",
    32773: "PACKBITS",
    34712: "JPEG2000",
    34887: "LERC",
    34925: "LZMA",
    50000: "ZSTD",
    50001: "WEBP",
    50002: "JXL",
    52546: "JXL",
}

# TIFF tags GDAL reports as `TIFFTAG_*` metadata, in its order.
_TIFFTAG_ITEMS = [
    (269, "TIFFTAG_DOCUMENTNAME"),
    (270, "TIFFTAG_IMAGEDESCRIPTION"),
    (305, "TIFFTAG_SOFTWARE"),
    (306, "TIFFTAG_DATETIME"),
    (315, "TIFFTAG_ARTIST"),
    (316, "TIFFTAG_HOSTCOMPUTER"),
    (33432, "TIFFTAG_COPYRIGHT"),
    (282, "TIFFTAG_XRESOLUTION"),
    (283, "TIFFTAG_YRESOLUTION"),
    (296, "TIFFTAG_RESOLUTIONUNIT"),
    (280, "TIFFTAG_MINSAMPLEVALUE"),
    (281, "TIFFTAG_MAXSAMPLEVALUE"),
]
_RESOLUTION_UNITS = {1: "1 (unitless)", 2: "2 (pixels/inch)", 3: "3 (pixels/cm)"}

# GDAL's color interpretation names.
COLOR_INTERP_NAMES = {
    ColorInterp.undefined: "Undefined",
    ColorInterp.gray: "Gray",
    ColorInterp.palette: "Palette",
    ColorInterp.red: "Red",
    ColorInterp.green: "Green",
    ColorInterp.blue: "Blue",
    ColorInterp.alpha: "Alpha",
    ColorInterp.hue: "Hue",
    ColorInterp.saturation: "Saturation",
    ColorInterp.lightness: "Lightness",
    ColorInterp.cyan: "Cyan",
    ColorInterp.magenta: "Magenta",
    ColorInterp.yellow: "Yellow",
    ColorInterp.black: "Black",
    ColorInterp.Y: "YCbCr_Y",
    ColorInterp.Cb: "YCbCr_Cb",
    ColorInterp.Cr: "YCbCr_Cr",
    ColorInterp.pan: "Pan",
    ColorInterp.coastal: "Coastal",
    ColorInterp.rededge: "RedEdge",
    ColorInterp.nir: "NIR",
    ColorInterp.swir: "SWIR",
    ColorInterp.mwir: "MWIR",
    ColorInterp.lwir: "LWIR",
    ColorInterp.tir: "TIR",
    ColorInterp.other_ir: "OtherIR",
    ColorInterp.sar_ka: "SAR_Ka",
    ColorInterp.sar_k: "SAR_K",
    ColorInterp.sar_ku: "SAR_Ku",
    ColorInterp.sar_x: "SAR_X",
    ColorInterp.sar_c: "SAR_C",
    ColorInterp.sar_s: "SAR_S",
    ColorInterp.sar_l: "SAR_L",
    ColorInterp.sar_p: "SAR_P",
}
_COLOR_INTERP_BY_NAME = {v.lower(): k for k, v in COLOR_INTERP_NAMES.items()}
_COLOR_INTERP_BY_NAME["grey"] = ColorInterp.gray


def color_interp_from_name(name: str) -> ColorInterp:
    """A color interpretation from its GDAL name.

    Args:
        name: The name, such as `Red`.

    Returns:
        ColorInterp: The color interpretation (`undefined` when unknown).
    """
    return _COLOR_INTERP_BY_NAME.get(name.strip().lower(), ColorInterp.undefined)


@dataclass
class Structure:
    """What GDAL reports about a raster besides its pixels."""

    count: int
    width: int
    height: int
    block_shape: tuple[int, int]
    tiled: bool = False
    compression: str | None = None
    interleave: str | None = None
    source_color_space: str | None = None
    colorinterp: list[ColorInterp] = field(default_factory=list)
    colormap: dict[int, tuple[int, int, int, int]] | None = None
    tags: dict[str, str] = field(default_factory=dict)
    namespaces: dict[str, dict[str, str]] = field(default_factory=dict)
    band_tags: list[dict[str, str]] = field(default_factory=list)
    # Each band's metadata domains other than the default one (JPEG 2000's IMAGE_STRUCTURE).
    band_namespaces: list[dict[str, dict[str, str]]] = field(default_factory=list)
    descriptions: list[str | None] = field(default_factory=list)
    scales: list[float] = field(default_factory=list)
    offsets: list[float] = field(default_factory=list)
    units: list[str | None] = field(default_factory=list)
    nodata_override: float | None = None
    # Per-band nodata values from a sidecar (band index from 0 to value).
    band_nodata: dict[int, float | None] = field(default_factory=dict)
    has_mask: bool = False
    # Whether the per-dataset mask is the `.msk` sidecar's (the file having no internal mask).
    mask_in_sidecar: bool = False
    # Ground control points `(pixel, line, x, y, z)`, as GDAL reads them from tie points.
    gcps: list[tuple[float, float, float, float, float]] = field(default_factory=list)
    # The 92 values of the RPCCoefficientTag, or GDAL's RPC items of a sidecar.
    rpc_tag: list[float] | None = None
    rpc_items: dict[str, str] | None = None

    def alpha_band(self) -> int | None:
        """The band GDAL uses as the alpha of the others (1-based), as `GetMaskFlags` does.

        Returns:
            int | None: Band 2 of a two-band raster or band 4 of a four-band one, when its color
            interpretation is alpha.
        """
        if self.count in (2, 4) and self.colorinterp[self.count - 1] == ColorInterp.alpha:
            return self.count
        return None


def _unescape(s: str) -> str:
    """Undoes the XML escaping GDAL applies to metadata values before serializing them.

    Args:
        s: The text, already unescaped once by the XML parser.

    Returns:
        str: The value.
    """

    def repl(m: re.Match[str]) -> str:
        e = m.group(1)
        if e.startswith("#x"):
            return chr(int(e[2:], 16))
        if e.startswith("#"):
            return chr(int(e[1:]))
        return {"lt": "<", "gt": ">", "amp": "&", "quot": '"', "apos": "'"}.get(e, m.group(0))

    return re.sub(r"&(#x[0-9a-fA-F]+|#[0-9]+|lt|gt|amp|quot|apos);", repl, s)


def _parse_float(s: str | None) -> float | None:
    """Parses a number.

    Args:
        s: The text.

    Returns:
        float | None: The number, or None.
    """
    if s is None:
        return None
    try:
        return float(s.strip())
    except ValueError:
        return None


def _fmt_g(v: float) -> str:
    """Formats a number as GDAL's `%.8g`.

    Args:
        v: The number.

    Returns:
        str: The text.
    """
    return f"{v:.8g}"


def _geokey(tags: dict[int, Any], key: int) -> int | None:
    """A SHORT GeoKey's value.

    Args:
        tags: The image's tags.
        key: The GeoKey.

    Returns:
        int | None: The value.
    """
    keys = tags.get(GEO_KEYS)
    if not isinstance(keys, list) or len(keys) < 4:
        return None
    for i in range(4, len(keys) - 3, 4):
        if keys[i] == key and keys[i + 1] == 0:
            return int(keys[i + 3])
    return None


def _first(v: Any, default: int = 0) -> int:
    """The first value of a tag.

    Args:
        v: The tag's values.
        default: The value when absent.

    Returns:
        int: The value.
    """
    if isinstance(v, list) and v:
        return int(v[0])
    return default


def default_structure(raster: Any) -> Structure:
    """The structure of a raster terrio read whole: one-row blocks, gray then undefined bands.

    Its geolocation arrays are GDAL's `GEOLOCATION` metadata domain.

    Args:
        raster: A `terrio.Raster`.

    Returns:
        Structure: The structure.
    """
    n = raster.count
    s = Structure(count=n, width=raster.width, height=raster.height, block_shape=(1, raster.width))
    s.colorinterp = [ColorInterp.gray] + [ColorInterp.undefined] * (n - 1)
    s.interleave = None
    s.band_tags = [{} for _ in range(n)]
    s.descriptions = [None] * n
    s.scales = [1.0] * n
    s.offsets = [0.0] * n
    s.units = [None] * n
    # Geolocation arrays, as GDAL's `GEOLOCATION` metadata domain.
    geolocation = getattr(raster, "geolocation", None)
    if geolocation:
        s.namespaces["GEOLOCATION"] = dict(geolocation)
    # Descriptions and metadata of formats that report them per band (GRIB).
    for b, md in enumerate((getattr(raster, "band_metadata", None) or [])[:n]):
        s.descriptions[b] = md.get("description")
        s.band_tags[b] = dict(md.get("items") or {})
        if md.get("color_interp"):
            s.colorinterp[b] = ColorInterp[md["color_interp"].lower()]
    # The color table of band 1 (a paletted PNG's), as GDAL reports it.
    table = getattr(raster, "color_table", None)
    if table:
        s.colormap = {i: (r, g, b, a) for i, (r, g, b, a) in enumerate(table)}
    for b, value in enumerate((getattr(raster, "band_nodata", None) or [])[:n]):
        s.band_nodata[b] = value
    # Metadata domains other than the default one, of the dataset (band 0) and of bands, for the
    # formats that report them (JPEG 2000's IMAGE_STRUCTURE).
    s.band_namespaces = [{} for _ in range(n)]
    for band, domain, items in getattr(raster, "metadata_domains", None) or []:
        if band == 0:
            s.namespaces.setdefault(domain, {}).update(items)
        elif band <= n:
            s.band_namespaces[band - 1].setdefault(domain, {}).update(items)
    # JPEG 2000: the block GDAL's JP2OpenJPEG driver gives (the tile, mostly).
    block = getattr(raster, "block_size", None)
    if getattr(raster, "driver", None) == "JP2" and block:
        s.block_shape = (block[1], block[0])
    # The driver's metadata: band items (`BAND_<n>#<key>`), and the dataset's (not subdatasets,
    # which are the `SUBDATASETS` domain).
    per_band_nodata = False
    for key, value in getattr(raster, "metadata", None) or []:
        # terrio's own items: the bands' exact scales and offsets (`TERRIO#BAND_<n>_SCALE`), and
        # the descriptions and nodata values of bands that have their own (GRIB's).
        if key.startswith("TERRIO#BAND_"):
            band_no, _, what = key[len("TERRIO#BAND_") :].partition("_")
            if band_no.isdigit() and 1 <= int(band_no) <= n:
                b = int(band_no) - 1
                if what == "DESCRIPTION":
                    s.descriptions[b] = value or None
                elif what == "NODATA":
                    per_band_nodata = True
                    with contextlib.suppress(ValueError):
                        s.band_nodata[b] = float(value)
                target = s.scales if what == "SCALE" else s.offsets if what == "OFFSET" else None
                if target is not None:
                    with contextlib.suppress(ValueError):
                        target[b] = float(value)
            continue
        band, sep, item = key.partition("#")
        if sep and band.startswith("BAND_") and band[5:].isdigit():
            b = int(band[5:])
            if 1 <= b <= n:
                s.band_tags[b - 1][item] = value
        elif key.startswith("SUBDATASET_"):
            s.namespaces.setdefault("SUBDATASETS", {})[key] = value
        else:
            s.tags[key] = value
    # Bands without a nodata value of their own have none.
    if per_band_nodata:
        for b in range(n):
            s.band_nodata.setdefault(b, None)
    return s


def tiff_structure(path: str, raster: Any) -> Structure | None:
    """The structure of a TIFF, from its tags.

    Args:
        path: The path or URL.
        raster: The `terrio.Raster` opened from it.

    Returns:
        Structure | None: The structure, or None when the file is not a TIFF.
    """
    try:
        ifds = _terrio.tiff_tags(path)
    except Exception:
        return None
    if not ifds:
        return None
    t = ifds[0]
    s = default_structure(raster)
    n = s.count
    if TILE_WIDTH in t and TILE_LENGTH in t:
        s.block_shape = (_first(t[TILE_LENGTH]), _first(t[TILE_WIDTH]))
        s.tiled = True
    else:
        rows = _first(t.get(ROWS_PER_STRIP), s.height)
        s.block_shape = (min(max(rows, 1), s.height), s.width)
        s.tiled = False
    code = _first(t.get(COMPRESSION), 1)
    name = _COMPRESSIONS.get(code)
    if name == "LERC":
        params = t.get(LERC_PARAMETERS)
        lerc_extra = params[1] if isinstance(params, list) and len(params) > 1 else 0
        name = {1: "LERC_DEFLATE", 2: "LERC_ZSTD"}.get(int(lerc_extra), "LERC")
    s.compression = name
    photometric = _first(t.get(PHOTOMETRIC), 1)
    # 8-bit CMYK, which GDAL reads through libtiff's RGBA interface as RGBA (`GTiffRGBABand`), as
    # terrio's reader does (`reads_cmyk_as_rgba` in cog.rs). GDAL reports its SOURCE_COLOR_SPACE
    # then only.
    samples = _first(t.get(SAMPLES), 1)
    cmyk_as_rgba = (
        photometric == 5
        and _first(t.get(BITS), 1) == 8
        and _first(t.get(SAMPLE_FORMAT), 1) == 1
        and _first(t.get(INK_SET), 1) == 1
        and samples >= 4
        and (_first(t.get(PLANAR), 1) != 2 or samples == 4)
        and n == 4
    )
    if photometric == 6 and code == 7:
        s.source_color_space = "YCbCr"
    elif cmyk_as_rgba:
        s.source_color_space = "CMYK"
    s.interleave = "BAND" if n == 1 or _first(t.get(PLANAR), 1) == 2 else "PIXEL"
    image_structure: dict[str, str] = {}
    if name:
        image_structure["COMPRESSION"] = "YCbCr JPEG" if s.source_color_space == "YCbCr" else name
    image_structure["INTERLEAVE"] = s.interleave
    # GDAL 3.12.1 `GTiffDataset::OpenOffset`: WhiteIsZero images, whatever their bits.
    if photometric == 0:
        image_structure["MINISWHITE"] = "YES"
    predictor = _first(t.get(317), 1)
    if predictor > 1:
        image_structure["PREDICTOR"] = str(predictor)
    if s.source_color_space:
        image_structure["SOURCE_COLOR_SPACE"] = s.source_color_space
    ghost = t.get(0)
    if isinstance(ghost, str) and "LAYOUT=IFDS_BEFORE_DATA" in ghost:
        if "KNOWN_INCOMPATIBLE_EDITION=YES" not in ghost:
            image_structure["LAYOUT"] = "COG"
    # libtiff's default BitsPerSample is 1, as the reader's.
    bits = _first(t.get(BITS), 1)
    tables = t.get(JPEG_TABLES)
    if code == 7 and isinstance(tables, (bytes, list)):
        # The JPEG quality and tables mode GDAL guesses from the JPEGTables tag.
        # The JPEGTables tag is UNDEFINED: bytes, or a list of ints.
        jpeg_tables = bytes(tables)  # type: ignore[arg-type]
        quality, _, huffman = guess_jpeg_quality(jpeg_tables, n, bits, photometric)
        if quality > 0:
            image_structure["JPEG_QUALITY"] = str(quality)
            image_structure["JPEGTABLESMODE"] = str(1 | (2 if huffman else 0))
    s.namespaces["IMAGE_STRUCTURE"] = image_structure

    # Color interpretation, from the photometric interpretation and the extra samples.
    extra_tag = t.get(EXTRA_SAMPLES)
    extra: list[Any] = extra_tag if isinstance(extra_tag, list) else []
    ci = [ColorInterp.undefined] * n
    base = 0
    if cmyk_as_rgba:
        ci = [ColorInterp.red, ColorInterp.green, ColorInterp.blue, ColorInterp.alpha]
        base = 4
    elif photometric in (2, 6) and n >= 3:
        ci[:3] = [ColorInterp.red, ColorInterp.green, ColorInterp.blue]
        base = 3
    elif photometric == 5 and n >= 4:
        ci[:4] = [ColorInterp.cyan, ColorInterp.magenta, ColorInterp.yellow, ColorInterp.black]
        base = 4
    elif photometric == 3:
        ci[0] = ColorInterp.palette
        base = 1
    elif n >= 1:
        # GDAL 3.12.1 `GTiffRasterBand`: a WhiteIsZero band is undefined (a palette with the color
        # table it gets below).
        ci[0] = ColorInterp.undefined if photometric == 0 else ColorInterp.gray
        base = 1
    for i in range(base, n):
        k = i - (n - len(extra)) if len(extra) else -1
        if 0 <= k < len(extra) and int(extra[k]) in (1, 2):
            ci[i] = ColorInterp.alpha
    s.colorinterp = ci

    # The color table.
    cmap = t.get(COLOR_MAP)
    if isinstance(cmap, list) and len(cmap) >= 3 and bits <= 16 and photometric not in (2, 6):
        s.colorinterp[0] = ColorInterp.palette
        m = len(cmap) // 3
        hi = max(cmap) if cmap else 0
        if hi <= 255:
            mult = 1
        elif all(v % 257 == 0 for v in cmap):
            mult = 257
        elif all(v % 256 == 0 for v in cmap):
            mult = 256
        else:
            mult = 257
        nodata = raster.nodata
        table: dict[int, tuple[int, int, int, int]] = {}
        for i in range(m):
            alpha = 0 if nodata is not None and nodata == i else 255
            table[i] = (
                int(cmap[i]) // mult,
                int(cmap[m + i]) // mult,
                int(cmap[2 * m + i]) // mult,
                alpha,
            )
        s.colormap = table

    # Samples read as stored (GDAL 3.12.1 `GTiffDataset::OpenOffset`): a WhiteIsZero image of at
    # most 16 bits gets an inverted gray color table, and a one-band bilevel image without a color
    # table GDAL's black and white one (`GTiffBitmapBand`); band 1 is then a palette.
    white_is_zero = photometric == 0 and bits <= 16
    if s.colormap is None and (bits < 8 or white_is_zero) and photometric not in (2, 5, 6):
        if photometric == 0:
            top = (1 << bits) - 1
            s.colormap = {i: ((255 * (top - i)) // top,) * 3 + (255,) for i in range(top + 1)}
        elif bits == 1 and n == 1:
            s.colormap = {0: (0, 0, 0, 255), 1: (255, 255, 255, 255)}
        if s.colormap is not None:
            s.colorinterp[0] = ColorInterp.palette
    # GDAL's band `NBITS` item, for samples narrower than their type (GDAL 3.12.1
    # `GTiffDataset::OpenOffset`).
    dtype = str(getattr(raster, "dtype", ""))
    if (
        (dtype == "uint8" and bits != 8)
        or (dtype == "uint16" and bits != 16)
        or (dtype in ("uint32", "float32") and bits != 32)
    ):
        for b in range(n):
            s.band_namespaces[b].setdefault("IMAGE_STRUCTURE", {})["NBITS"] = str(bits)
    # Metadata: TIFF tags, then GDAL_METADATA items, then AREA_OR_POINT.
    tags: dict[str, str] = {}
    for tag, key in _TIFFTAG_ITEMS:
        if tag not in t:
            continue
        v = t[tag]
        if tag == 296:
            tags[key] = _RESOLUTION_UNITS.get(_first(v), str(_first(v)))
        elif isinstance(v, str):
            tags[key] = v
        elif isinstance(v, list) and v:
            tags[key] = _fmt_g(float(v[0])) if tag in (282, 283) else str(v[0])
    xml = t.get(GDAL_METADATA)
    if isinstance(xml, str):
        _apply_gdal_metadata(s, xml, tags)
    # The unit of a compound CRS's heights is the unit of the bands without one of their own
    # (GDAL 3.12.1 `GTiffRasterBand::GetUnitType`).
    vertical_unit = _vertical_unit(getattr(raster, "crs_wkt", None))
    if vertical_unit:
        s.units = [u or vertical_unit for u in s.units]
    if bits == 1 and n == 1:
        # `GTiffBitmapBand::GetColorInterpretation` is a palette, whatever the file stores.
        s.colorinterp[0] = ColorInterp.palette
    if cmyk_as_rgba:
        # `GTiffRGBABand::GetColorInterpretation` is red, green, blue and alpha, whatever the file
        # stores.
        s.colorinterp = [ColorInterp.red, ColorInterp.green, ColorInterp.blue, ColorInterp.alpha]
    if "AREA_OR_POINT" not in tags and GEO_KEYS in t:
        tags["AREA_OR_POINT"] = "Point" if _geokey(t, 1025) == 2 else "Area"
    s.tags = tags

    # Ground control points: tie points without a pixel scale or a transformation matrix, as GDAL
    # reads them (PixelIsPoint ones shifted by half a pixel).
    ties = t.get(TIEPOINTS)
    if (
        isinstance(ties, list)
        and len(ties) >= 6
        and MODEL_TRANSFORMATION not in t
        and (len(ties) > 6 or PIXEL_SCALE not in t)
    ):
        shift = 0.5 if _geokey(t, 1025) == 2 else 0.0
        s.gcps = [
            (
                float(ties[i]) + shift,
                float(ties[i + 1]) + shift,
                float(ties[i + 3]),
                float(ties[i + 4]),
                float(ties[i + 5]),
            )
            for i in range(0, len(ties) - 5, 6)
        ]
    rpc = t.get(RPC_COEFFICIENTS)
    if isinstance(rpc, list) and len(rpc) == 92:
        s.rpc_tag = [float(v) for v in rpc]

    # A per-dataset mask: a transparency mask image.
    for ifd in ifds[1:]:
        if _first(ifd.get(NEW_SUBFILE_TYPE)) & 4 and _first(ifd.get(PHOTOMETRIC)) == 4:
            if _first(ifd.get(WIDTH)) == s.width and _first(ifd.get(HEIGHT)) == s.height:
                s.has_mask = True
    return s


def _vertical_unit(wkt: str | None) -> str | None:
    """The linear unit of the vertical CRS of a compound CRS, as GDAL names it.

    Args:
        wkt: The CRS's WKT, or None.

    Returns:
        str | None: The unit's name (`metre`), or None for other CRSs and an unknown unit.
    """
    m = re.search(r'(?:VERT_CS|VERTCRS|VERTICALCRS)\[.*?UNIT\["([^"]*)"', wkt or "")
    if m is None or m.group(1).lower() == "unknown":
        return None
    return m.group(1)


def _apply_gdal_metadata(s: Structure, xml: str, tags: dict[str, str]) -> None:
    """Reads a `GDAL_METADATA` document into a structure.

    Args:
        s: The structure to update.
        xml: The document.
        tags: The dataset tags to add items to.
    """
    try:
        root = ET.fromstring(xml)
    except ET.ParseError:
        return
    for item in root.iter("Item"):
        name = item.get("name")
        if name is None:
            continue
        value = _unescape(item.text or "")
        sample = item.get("sample")
        role = item.get("role")
        domain = item.get("domain") or ""
        if sample is None:
            if domain:
                s.namespaces.setdefault(domain, {})[name] = value
            elif role is None:
                tags[name] = value
            continue
        b = int(sample)
        if not 0 <= b < s.count:
            continue
        if role == "description":
            s.descriptions[b] = value or None
        elif role == "scale":
            s.scales[b] = _parse_float(value) or 1.0
        elif role == "offset":
            s.offsets[b] = _parse_float(value) or 0.0
        elif role == "unittype":
            s.units[b] = value or None
        elif role == "colorinterp":
            s.colorinterp[b] = color_interp_from_name(value)
        elif role is None and not domain:
            s.band_tags[b][name] = value


def apply_pam(s: Structure, path: str) -> None:
    """Applies a GDAL `.aux.xml` sidecar (persistent auxiliary metadata) to a structure.

    Args:
        s: The structure to update.
        path: The dataset's path; local files only.
    """
    aux = path + ".aux.xml"
    if not os.path.isfile(aux):
        return
    try:
        root = ET.parse(aux).getroot()
    except (ET.ParseError, OSError):
        return
    for md in root.findall("Metadata"):
        domain = md.get("domain") or ""
        if md.get("format") == "xml":
            continue
        target = s.tags if not domain else s.namespaces.setdefault(domain, {})
        for mdi in md.findall("MDI"):
            key = mdi.get("key")
            if key is not None:
                target[key] = mdi.text or ""
    for band in root.findall("PAMRasterBand"):
        try:
            b = int(band.get("band", "0")) - 1
        except ValueError:
            continue
        if not 0 <= b < s.count:
            continue
        for child in band:
            text = child.text or ""
            if child.tag == "Description":
                s.descriptions[b] = text or None
            elif child.tag == "Scale":
                s.scales[b] = _parse_float(text) or 1.0
            elif child.tag == "Offset":
                s.offsets[b] = _parse_float(text) or 0.0
            elif child.tag == "UnitType":
                s.units[b] = text or None
            elif child.tag == "ColorInterp":
                s.colorinterp[b] = color_interp_from_name(text)
            elif child.tag == "NoDataValue":
                value = _parse_float(text)
                s.band_nodata[b] = value
                if b == 0:
                    s.nodata_override = value
            elif child.tag == "Metadata" and not child.get("domain"):
                for mdi in child.findall("MDI"):
                    key = mdi.get("key")
                    if key is not None:
                        s.band_tags[b][key] = mdi.text or ""
