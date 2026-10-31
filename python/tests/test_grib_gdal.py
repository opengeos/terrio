"""GRIB1 and GRIB2 read by terrio and by GDAL (rasterio): band count, size, geotransform, CRS,
pixels, nodata, masks and band metadata must be GDAL's. Remote files are read by range, within a
ceiling of bytes, and allocations sized by headers fail cleanly.

The files are written by GDAL's GRIB driver at test time, one per data encoding it writes
(simple and complex packing, IEEE, PNG, JPEG 2000), with and without nodata, and on latitude/
longitude, Lambert conformal and polar stereographic grids. GDAL never writes a bitmap (section
6), so the bitmap cases are GDAL's simple packing files rewritten with one, including a message
of two fields where the second reuses the first's bitmap (indicator 254); a simple packing file is
rewritten too with scales whose values single and double precision unpack differently. Skipped
when rasterio is not installed.
"""

import os
import struct
import time
from pathlib import Path

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")
from rasterio.crs import CRS as RioCRS  # noqa: E402
from rasterio.io import MemoryFile  # noqa: E402
from rasterio.shutil import copy as rio_copy  # noqa: E402
from rasterio.transform import Affine, from_origin  # noqa: E402
from rasterio.windows import Window  # noqa: E402

import terrio  # noqa: E402
import terrio.rio as trio  # noqa: E402
from terrio.rio.shutil import copy as trio_copy  # noqa: E402

with rasterio.Env() as _env:
    if "GRIB" not in _env.drivers():
        pytest.skip("GDAL has no GRIB driver", allow_module_level=True)

# The metadata items compared with GDAL's.
TAGS = (
    "GRIB_COMMENT",
    "GRIB_DISCIPLINE",
    "GRIB_ELEMENT",
    "GRIB_FORECAST_SECONDS",
    "GRIB_IDS",
    "GRIB_PDS_PDTN",
    "GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES",
    "GRIB_PDS_TEMPLATE_NUMBERS",
    "GRIB_REF_TIME",
    "GRIB_SHORT_NAME",
    "GRIB_UNIT",
    "GRIB_VALID_TIME",
)

NODATA = -9999.0
# Temperature at 2 m above ground (product template 4.0, category 0, number 0): degrib converts
# it from Kelvin to Celsius.
TEMPERATURE = "0 0 2 0 96 0 0 0 1 0 0 0 6 103 0 0 0 0 2 255 0 0 0 0 0"


def field(height, width, bands=1, seed=0, nodata=False):
    """Smooth Float32 fields with noise, and nodata pixels in a block and a few scattered ones."""
    rng = np.random.default_rng(seed)
    y, x = np.mgrid[0:height, 0:width]
    out = []
    for b in range(bands):
        v = 280 + 25 * np.sin(y / 5.0 + b) * np.cos(x / 7.0) + rng.normal(0, 0.7, (height, width))
        v = v.astype(np.float32)
        if nodata:
            v[2:5, 3:9] = NODATA
            v[rng.integers(0, height, 6), rng.integers(0, width, 6)] = NODATA
        out.append(v)
    return np.stack(out)


def write_grib(path, data, crs, transform, nodata=None, **options):
    """Writes a GRIB2 file with GDAL (the driver only copies, so through an in-memory GeoTIFF)."""
    count, height, width = data.shape
    profile = dict(driver="GTiff", width=width, height=height, count=count, dtype=data.dtype.name, crs=crs, transform=transform)
    if nodata is not None:
        profile["nodata"] = nodata
    with MemoryFile() as mem:
        with mem.open(**profile) as dst:
            dst.write(data)
        with mem.open() as src:
            rio_copy(src, str(path), driver="GRIB", **options)
    return str(path)


def assert_same(path, tags=True, epsg=True):
    """terrio reads the file as GDAL does (with `epsg` false, all but the EPSG code GDAL
    identifies for the CRS, which terrio's CRS module does not identify for every CRS)."""
    with rasterio.open(path) as g, trio.open(path) as t:
        assert (t.count, t.width, t.height) == (g.count, g.width, g.height)
        assert t.dtypes == g.dtypes
        gt, tt = np.array(g.transform.to_gdal()), np.array(t.transform.to_gdal())
        np.testing.assert_allclose(tt, gt, rtol=1e-9, atol=1e-12)
        if g.crs is None:
            assert t.crs is None
        else:
            assert t.crs is not None
            assert RioCRS.from_wkt(t.crs.to_wkt()) == g.crs, (t.crs.to_wkt(), g.crs.to_wkt())
            if epsg:
                assert t.crs.to_epsg() == g.crs.to_epsg()
        a, b = g.read(), t.read()
        if not np.array_equal(a, b, equal_nan=True):
            d = np.abs(a - b)
            raise AssertionError(f"pixels differ: {np.count_nonzero(d)} of {d.size}, max |d| {np.nanmax(d)}")
        assert t.nodatavals == g.nodatavals
        assert np.array_equal(t.read_masks(), g.read_masks())
        assert np.array_equal(t.read(masked=True).mask, g.read(masked=True).mask)
        if g.count > 1 and g.width > 4 and g.height > 4:
            w = (1, 2, g.width - 3, g.height - 4)
            idx = [g.count, 1]
            ours = t.read(indexes=idx, window=trio.windows.Window(*w))
            assert np.array_equal(ours, g.read(indexes=idx, window=Window(*w)), equal_nan=True)
        assert t.descriptions == g.descriptions
        assert t.colorinterp == g.colorinterp
        if tags:
            for b in range(1, g.count + 1):
                gtags = {k: v for k, v in g.tags(b).items() if k in TAGS}
                ttags = {k: v for k, v in t.tags(b).items() if k in TAGS}
                assert ttags == gtags, b


LATLON = dict(crs="EPSG:4326", transform=from_origin(-20.0, 60.0, 0.5, 0.5))
ENCODINGS = [
    ("SIMPLE_PACKING", {}),
    ("SIMPLE_PACKING", {"DECIMAL_SCALE_FACTOR": "2"}),
    ("SIMPLE_PACKING", {"NBITS": "7"}),
    ("COMPLEX_PACKING", {}),
    ("COMPLEX_PACKING", {"SPATIAL_DIFFERENCING_ORDER": "1"}),
    ("COMPLEX_PACKING", {"SPATIAL_DIFFERENCING_ORDER": "2", "DECIMAL_SCALE_FACTOR": "1"}),
    ("IEEE_FLOATING_POINT", {}),
    ("PNG", {}),
    ("PNG", {"NBITS": "16", "DECIMAL_SCALE_FACTOR": "1"}),
    ("JPEG2000", {}),
    ("JPEG2000", {"NBITS": "12"}),
    ("JPEG2000", {"COMPRESSION_RATIO": "10"}),
]


@pytest.mark.parametrize("nodata", [False, True], ids=["no-nodata", "nodata"])
@pytest.mark.parametrize("encoding,options", ENCODINGS, ids=[f"{e}-{'-'.join(f'{k}={v}' for k, v in o.items())}" for e, o in ENCODINGS])
def test_encodings(tmp_path, encoding, options, nodata):
    data = field(41, 57, bands=3, seed=len(encoding), nodata=nodata)
    path = write_grib(tmp_path / "f.grib2", data, nodata=NODATA if nodata else None, DATA_ENCODING=encoding, **options, **LATLON)
    assert_same(path)


def test_temperature_kelvin_to_celsius(tmp_path):
    # GDAL writes Celsius as Kelvin for temperatures and reads them back as Celsius.
    data = field(20, 30, bands=2, seed=3, nodata=True) - 273.0
    data[data < -9000] = NODATA
    for encoding in ("SIMPLE_PACKING", "COMPLEX_PACKING", "IEEE_FLOATING_POINT"):
        path = write_grib(
            tmp_path / f"t_{encoding}.grib2", data, nodata=NODATA, DATA_ENCODING=encoding,
            PDS_PDTN="0", PDS_TEMPLATE_NUMBERS=TEMPERATURE, **LATLON
        )
        assert_same(path)
        with rasterio.open(path) as g:
            assert g.tags(1)["GRIB_UNIT"] == "[C]"


def test_missing_value_inside_the_data(tmp_path):
    # A missing value within the range of valid values: degrib moves the missing points above the
    # data (`f_readjust`) while GDAL reports the file's missing value as nodata.
    data = field(19, 23, bands=2, seed=19)
    data[:, 3:6, 4:9] = 280.0
    path = write_grib(tmp_path / "m.grib2", data, nodata=280.0, DATA_ENCODING="COMPLEX_PACKING", **LATLON)
    assert_same(path)


# Product templates whose names, units and conversions degrib builds from their contents.
T0 = "0 0 2 0 96 0 0 0 1 0 0 0 6 103 0 0 0 0 2 255 0 0 0 0 0"
INTERVAL = "7 234 1 1 6 0 0 1 0 0 0 0 1 2 1 0 0 0 6 255 0 0 0 0"
PRODUCTS = {
    "4.5-probability": ("5", T0 + " 0 1 1 0 0 0 0 0 2 0 0 106 179"),
    "4.6-percentile": ("6", T0 + " 90"),
    "4.8-accumulation": ("8", "1 8 2 0 96 0 0 0 1 0 0 0 0 1 0 0 0 0 0 255 0 0 0 0 0 " + INTERVAL),
    "4.9-probability-interval": ("9", "1 8 2 0 96 0 0 0 1 0 0 0 0 1 0 0 0 0 0 255 0 0 0 0 0 0 1 1 0 0 0 0 0 3 0 0 0 254 " + INTERVAL),
}


@pytest.mark.parametrize("product", PRODUCTS, ids=list(PRODUCTS))
def test_product_templates(tmp_path, product):
    """Catches names, valid times and unit conversions of products other than template 4.0
    (temperature probabilities are not converted to Celsius, percentiles are)."""
    pdtn, numbers = PRODUCTS[product]
    data = field(11, 13, bands=1, seed=29)
    path = write_grib(tmp_path / "p.grib2", data, DATA_ENCODING="SIMPLE_PACKING", PDS_PDTN=pdtn, PDS_TEMPLATE_NUMBERS=numbers, **LATLON)
    assert_same(path)


@pytest.mark.parametrize(
    "origin,res,shape",
    [
        ((0.0, 90.0), 5.0, (37, 72)),  # global, 0 to 355: GDAL moves the east half west
        ((-180.0, 90.0), 2.5, (73, 144)),  # global from the antimeridian
        ((150.0, 10.0), 1.0, (8, 60)),  # across the antimeridian
        ((-100.0, 50.0), 0.25, (9, 13)),  # western hemisphere only
    ],
    ids=["global-0-360", "global-180", "antimeridian", "west"],
)
def test_longitude_ranges(tmp_path, origin, res, shape):
    data = field(*shape, bands=2, seed=5)
    path = write_grib(
        tmp_path / "g.grib2", data, crs="EPSG:4326", transform=from_origin(origin[0], origin[1], res, res),
        DATA_ENCODING="COMPLEX_PACKING", SPATIAL_DIFFERENCING_ORDER="2",
    )
    assert_same(path)


@pytest.mark.parametrize(
    "crs",
    [
        "+proj=longlat +a=6371229 +b=6371229 +no_defs",
        "+proj=longlat +ellps=GRS80 +no_defs",
        "+proj=longlat +a=6378160 +b=6356775 +no_defs",
        "+proj=lcc +lat_1=25 +lat_2=25 +lat_0=25 +lon_0=-95 +x_0=0 +y_0=0 +a=6371229 +b=6371229 +units=m +no_defs",
        "+proj=lcc +lat_1=33 +lat_2=45 +lat_0=40 +lon_0=-97 +x_0=0 +y_0=0 +ellps=WGS84 +units=m +no_defs",
        "+proj=stere +lat_0=90 +lat_ts=60 +lon_0=-105 +x_0=0 +y_0=0 +a=6371229 +b=6371229 +units=m +no_defs",
        "+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +x_0=0 +y_0=0 +ellps=WGS84 +units=m +no_defs",
        "+proj=merc +lon_0=0 +k=1 +x_0=0 +y_0=0 +ellps=WGS84 +units=m +no_defs",
        "+proj=merc +lat_ts=20 +lon_0=0 +x_0=0 +y_0=0 +a=6371229 +b=6371229 +units=m +no_defs",
    ],
    ids=["sphere", "grs80", "iau1965", "lcc-sphere", "lcc-wgs84", "polar-north-sphere", "polar-south-wgs84", "mercator-wgs84", "mercator-2sp-sphere"],
)
def test_grids_and_earth_shapes(tmp_path, crs):
    data = field(25, 31, bands=2, seed=7)
    if "longlat" in crs:
        transform = from_origin(-10.0, 50.0, 0.25, 0.25)
    else:
        transform = from_origin(-1_500_000.0, 1_200_000.0, 12_000.0, 12_000.0)
    path = write_grib(tmp_path / "p.grib2", data, crs=crs, transform=transform, DATA_ENCODING="SIMPLE_PACKING")
    assert_same(path)


# Bitmaps: GDAL's simple packing files rewritten with a bitmap.


def sections(message):
    """The sections of a GRIB2 message: `[(number, bytes)]` from section 1, without section 8."""
    out, pos = [], 16
    while message[pos : pos + 4] != b"7777":
        length = int.from_bytes(message[pos : pos + 4], "big")
        out.append((message[pos + 4], message[pos : pos + length]))
        pos += length
    return out


def messages(data):
    out, pos = [], 0
    while (i := data.find(b"GRIB", pos)) >= 0:
        total = int.from_bytes(data[i + 8 : i + 16], "big")
        out.append(data[i : i + total])
        pos = i + total
    return out


def build(head, secs):
    body = b"".join(secs)
    total = 16 + len(body) + 4
    return head[:8] + total.to_bytes(8, "big") + body + b"7777"


def section(number, body):
    return (5 + len(body)).to_bytes(4, "big") + bytes([number]) + body


def repack(sect5, sect7, keep):
    """Simple packing keeping only the points `keep` (a boolean array in file order)."""
    nbits = sect5[19]
    count = int.from_bytes(sect5[5:9], "big")
    bits = np.unpackbits(np.frombuffer(sect7[5:], np.uint8))[: count * nbits].reshape(count, nbits)
    kept = bits[keep].reshape(-1)
    new5 = sect5[:5] + int(keep.sum()).to_bytes(4, "big") + sect5[9:]
    return new5, section(7, np.packbits(kept).tobytes())


def bitmap_section(keep):
    return section(6, bytes([0]) + np.packbits(keep.astype(np.uint8)).tobytes())


def file_order_mask(mask):
    """A raster mask (north up) in the file's order: GDAL writes rows south to north."""
    return mask[::-1].reshape(-1)


def masked(height, width, seed):
    rng = np.random.default_rng(seed)
    mask = rng.random((height, width)) < 0.3
    mask[:, :4] = True
    return mask


def test_bitmap(tmp_path):
    data = field(23, 29, bands=2, seed=11)
    src = write_grib(tmp_path / "s.grib2", data, DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    out = b""
    for k, msg in enumerate(messages(open(src, "rb").read())):
        keep = ~file_order_mask(masked(23, 29, k))
        s = dict(sections(msg))
        new5, new7 = repack(s[5], s[7], keep)
        out += build(msg[:16], [s[1], s[3], s[4], new5, bitmap_section(keep), new7])
    path = tmp_path / "bitmap.grib2"
    path.write_bytes(out)
    assert_same(str(path))
    with rasterio.open(path) as g:
        assert g.nodatavals == (9999.0, 9999.0)


def test_bitmap_reused_by_second_field(tmp_path):
    # One message of two fields: the second's section 6 is indicator 254 (the previous bitmap).
    data = field(23, 29, bands=2, seed=13)
    src = write_grib(tmp_path / "s.grib2", data, DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    keep = ~file_order_mask(masked(23, 29, 1))
    first, second = (dict(sections(m)) for m in messages(open(src, "rb").read()))
    a5, a7 = repack(first[5], first[7], keep)
    b5, b7 = repack(second[5], second[7], keep)
    msg = build(messages(open(src, "rb").read())[0][:16], [
        first[1], first[3], first[4], a5, bitmap_section(keep), a7,
        second[4], b5, section(6, bytes([254])), b7,
    ])
    path = tmp_path / "bitmap254.grib2"
    path.write_bytes(msg)
    assert_same(str(path))


def sign_magnitude(v, octets):
    return ((1 << (8 * octets - 1)) | -v if v < 0 else v).to_bytes(octets, "big")


def with_packing(message, reference, e, d, nbits, ints):
    """A simple packing message with its scaling and values replaced."""
    s = dict(sections(message))
    s5 = bytearray(s[5])
    s5[11:15] = struct.pack(">f", reference)
    s5[15:17] = sign_magnitude(e, 2)
    s5[17:19] = sign_magnitude(d, 2)
    s5[19] = nbits
    bits = ((ints[:, None] >> np.arange(nbits - 1, -1, -1)) & 1).astype(np.uint8).reshape(-1)
    return build(message[:16], [s[1], s[3], s[4], bytes(s5), s[6], section(7, np.packbits(bits).tobytes())])


def packing_arithmetic(src, out):
    """GDAL's simple packing rewritten with a reference value, binary and decimal scales whose
    sums are inexact in single precision: g2clib's float arithmetic, `((i * 2^E) + R) * 10^-D` in
    single precision, then differs from double precision's in about a third of the values."""
    message = messages(open(src, "rb").read())[0]
    count = int.from_bytes(dict(sections(message))[5][5:9], "big")
    ints = np.random.default_rng(23).integers(0, 1 << 24, count)
    out.write_bytes(with_packing(message, 1234.5678, -7, 2, 24, ints))
    return str(out)


def test_packing_arithmetic(tmp_path):
    """Catches unpacking in double precision, which GDAL's own files cannot tell apart."""
    src = write_grib(tmp_path / "s.grib2", field(23, 29, seed=21), DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    assert_same(packing_arithmetic(src, tmp_path / "arithmetic.grib2"))


def test_open_raster_and_info(tmp_path):
    data = field(12, 16, bands=2, seed=17, nodata=True)
    path = write_grib(tmp_path / "x.grb2", data, nodata=NODATA, DATA_ENCODING="COMPLEX_PACKING", **LATLON)
    r = terrio.open_raster(path)
    assert r.driver == "GRIB"
    assert r.count == 2 and r.dtype == "float64"
    with rasterio.open(path) as g:
        assert np.array_equal(np.asarray(r.read()), g.read())
    # Detected by content without a GRIB extension, as GDAL does.
    renamed = tmp_path / "noext"
    renamed.write_bytes(open(path, "rb").read())
    assert terrio.open_raster(str(renamed)).driver == "GRIB"


REMOTE = os.environ.get("TERRIO_TEST_REMOTE")


@pytest.mark.skipif(not REMOTE, reason="TERRIO_TEST_REMOTE is not set")
@pytest.mark.parametrize("url", [u for u in (os.environ.get("TERRIO_TEST_GRIB_URLS") or "").split(",") if u])
def test_remote_products(url):
    assert_same(url)


# GRIB1: GDAL writes only GRIB2, so the messages are built here (simple packing, as WMO's
# FM 92 edition 1 lays it out) and read by GDAL and terrio.


def ibm(v):
    """`v` as an IBM single precision number (exact for the integers used as reference values)."""
    if v == 0:
        return bytes(4)
    sign, v, e = (0x80 if v < 0 else 0), abs(float(v)), 64
    while v >= 1:
        v, e = v / 16, e + 1
    while v < 1 / 16:
        v, e = v * 16, e - 1
    return bytes([sign | e]) + int(v * (1 << 24)).to_bytes(3, "big")


def g1_section(body):
    return (3 + len(body)).to_bytes(3, "big") + body


def g1_pds(param=11, level=(105, 2), center=7, table=2, subcenter=0, process=96, d=1, bitmap=False,
           when=(2024, 5, 17, 6, 0), unit=1, p1=6, p2=0, time_range=0):
    year, month, day, hour, minute = when
    flags = 0x80 | (0x40 if bitmap else 0)
    body = bytes([table, center, process, 255, flags, param, level[0]]) + level[1].to_bytes(2, "big")
    body += bytes([year % 100 or 100, month, day, hour, minute, unit, p1, p2, time_range]) + bytes(3)
    body += bytes([(year - 1) // 100 + 1, subcenter]) + sign_magnitude(d, 2)
    assert len(body) == 25
    return g1_section(body)


def g1_latlon(ni, nj, la1, lo1, la2, lo2, di, dj, scan=0, typ=0, res=0x80, rotation=None):
    body = bytes([0, 255, typ]) + ni.to_bytes(2, "big") + nj.to_bytes(2, "big") + sign_magnitude(la1, 3) + sign_magnitude(lo1, 3)
    body += bytes([res]) + sign_magnitude(la2, 3) + sign_magnitude(lo2, 3) + di.to_bytes(2, "big") + dj.to_bytes(2, "big")
    body += bytes([scan]) + bytes(4)
    if rotation is not None:
        south_lat, south_lon, angle = rotation
        body += sign_magnitude(south_lat, 3) + sign_magnitude(south_lon, 3) + ibm(angle)
    return g1_section(body)


def g1_polar(nx, ny, la1, lo1, lov, dx, south=False, scan=0x40, res=0x08):
    body = bytes([0, 255, 5]) + nx.to_bytes(2, "big") + ny.to_bytes(2, "big") + sign_magnitude(la1, 3) + sign_magnitude(lo1, 3)
    body += bytes([res]) + sign_magnitude(lov, 3) + sign_magnitude(dx, 3) + sign_magnitude(dx, 3)
    body += bytes([0x80 if south else 0, scan]) + bytes(4)
    return g1_section(body)


def g1_lambert(nx, ny, la1, lo1, lov, dx, latin1, latin2, scan=0x40, res=0x08):
    body = bytes([0, 255, 3]) + nx.to_bytes(2, "big") + ny.to_bytes(2, "big") + sign_magnitude(la1, 3) + sign_magnitude(lo1, 3)
    body += bytes([res]) + sign_magnitude(lov, 3) + sign_magnitude(dx, 3) + sign_magnitude(dx, 3) + bytes([0, scan])
    body += sign_magnitude(latin1, 3) + sign_magnitude(latin2, 3) + sign_magnitude(-90000, 3) + bytes(3) + bytes(2)
    return g1_section(body)


def g1_mercator(ni, nj, la1, lo1, la2, lo2, latin, di, dj, scan=0x40, res=0x80):
    body = bytes([0, 255, 1]) + ni.to_bytes(2, "big") + nj.to_bytes(2, "big") + sign_magnitude(la1, 3) + sign_magnitude(lo1, 3)
    body += bytes([res]) + sign_magnitude(la2, 3) + sign_magnitude(lo2, 3) + sign_magnitude(latin, 3) + bytes([0, scan])
    body += sign_magnitude(di, 3) + sign_magnitude(dj, 3) + bytes(8)
    return g1_section(body)


def g1_message(values, pds, gds, d=1, e=0, nbits=16, keep=None):
    """A message of `values` (in the file's scanning order) packed `(R + X * 2^E) / 10^D`, the
    points `keep` leaves out (when given) masked by a bitmap."""
    v = np.asarray(values, np.float64)
    if keep is not None:
        v = v[keep]
    scaled = v * 10.0**d
    ref = float(np.floor(scaled.min())) if v.size else 0.0
    ints = np.round((scaled - ref) / 2.0**e).astype(np.int64)
    assert ints.min() >= 0 and (nbits == 0 or ints.max() < 1 << nbits), (ints.min(), ints.max())
    bits = ((ints[:, None] >> np.arange(nbits - 1, -1, -1)) & 1).astype(np.uint8).reshape(-1) if nbits else np.zeros(0, np.uint8)
    data = np.packbits(bits).tobytes()
    if (11 + len(data)) % 2:
        data += b"\0"
    unused = len(data) * 8 - bits.size
    bds = g1_section(bytes([unused]) + sign_magnitude(e, 2) + ibm(ref) + bytes([nbits]) + data)
    bms = b""
    if keep is not None:
        packed = np.packbits(np.asarray(keep, np.uint8)).tobytes()
        bms = g1_section(bytes([len(packed) * 8 - len(keep)]) + bytes(2) + packed)
    body = pds + gds + bms + bds
    return b"GRIB" + (8 + len(body) + 4).to_bytes(3, "big") + b"\x01" + body + b"7777"


def g1_field(ny, nx, seed, base=280.0, amplitude=25.0):
    """A smooth field, as a north-up raster."""
    rng = np.random.default_rng(seed)
    y, x = np.mgrid[0:ny, 0:nx]
    return base + amplitude * np.sin(y / 5.0) * np.cos(x / 7.0) + rng.normal(0, 0.7, (ny, nx))


def latlon_file(tmp_path, name, messages):
    path = tmp_path / name
    path.write_bytes(b"".join(messages))
    return str(path)


def test_grib1_latlon_two_messages(tmp_path):
    """Catches GRIB1 values, nodata, georeferencing and names: temperature in Celsius from Kelvin,
    and geopotential height on an isobaric level."""
    ny, nx = 21, 33
    t = g1_field(ny, nx, 1)
    z = g1_field(ny, nx, 2, base=5500.0, amplitude=120.0)
    grid = g1_latlon(nx, ny, 60000, -20000, 40000, -4000, 500, 1000)
    msgs = [
        g1_message(t.reshape(-1), g1_pds(), grid),
        g1_message(z.reshape(-1), g1_pds(param=7, level=(100, 500), d=0, p1=12, time_range=4, p2=18), grid, d=0, e=-2, nbits=20),
    ]
    path = latlon_file(tmp_path, "two.grb", msgs)
    assert_same(path)
    with rasterio.open(path) as g:
        assert g.tags(1)["GRIB_UNIT"] == "[C]" and g.tags(2)["GRIB_ELEMENT"] == "HGT"


@pytest.mark.parametrize("case", ["temperature.grib2", "ds.mint.bin", "grib2_bitmap", "grib1"])
def test_units_not_normalized_as_gdal(tmp_path, case):
    """Catches GRIB_NORMALIZE_UNITS=NO ignored: GDAL then keeps temperatures in Kelvin (values,
    GRIB_UNIT and GRIB_COMMENT), GRIB2 and GRIB1 alike; the default still converts."""
    if case == "grib1":
        ny, nx = 21, 33
        grid = g1_latlon(nx, ny, 60000, -20000, 40000, -4000, 500, 1000)
        message = g1_message(g1_field(ny, nx, 1).reshape(-1), g1_pds(), grid)
        path = latlon_file(tmp_path, "t.grb", [message])
    elif case == "grib2_bitmap":
        data = field(20, 30, bands=2, seed=3, nodata=True) - 273.0
        data[data < -9000] = NODATA
        path = write_grib(
            tmp_path / "t.grib2", data, nodata=NODATA, PDS_PDTN="0",
            PDS_TEMPLATE_NUMBERS=TEMPERATURE, **LATLON
        )
    else:
        path = str(FIXTURES / case)

    def raw(lib):
        with lib.Env(GRIB_NORMALIZE_UNITS="NO"):
            with lib.open(path) as ds:
                tags = [{k: v for k, v in ds.tags(b).items() if k in TAGS} for b in ds.indexes]
                return ds.read(), ds.nodatavals, tags

    g, t = raw(rasterio), raw(trio)
    assert np.array_equal(t[0], g[0], equal_nan=True)
    assert t[1] == g[1]
    assert t[2] == g[2]
    assert "[K]" in g[2][0]["GRIB_UNIT"]
    assert_same(path)


@pytest.mark.parametrize("scan", [0x00, 0x40, 0x80, 0x20, 0xC0], ids=["0000", "0100", "1000", "0010", "1100"])
def test_grib1_scanning_modes(tmp_path, scan):
    ny, nx = 9, 14
    v = g1_field(ny, nx, 3).reshape(-1)
    path = latlon_file(tmp_path, "s.grb", [g1_message(v, g1_pds(), g1_latlon(nx, ny, 10000, 100000, 2000, 113000, 1000, 1000, scan=scan))])
    assert_same(path)


@pytest.mark.parametrize("d,e,nbits", [(2, 0, 16), (0, -3, 18), (1, 2, 12), (-1, 0, 10), (3, -4, 32), (1, 0, 0)])
def test_grib1_scales(tmp_path, d, e, nbits):
    """Catches a decimal (D) or binary (E) scale factor applied wrongly, and constant fields."""
    v = g1_field(12, 17, 4, base=1000.0, amplitude=300.0).reshape(-1)
    if nbits == 0:
        v[:] = 12.5
    grid = g1_latlon(17, 12, 50000, 0, 39000, 16000, 1000, 1000)
    path = latlon_file(tmp_path, "d.grb", [g1_message(v, g1_pds(param=1, level=(1, 0), d=d), grid, d=d, e=e, nbits=nbits)])
    assert_same(path)


@pytest.mark.parametrize("case", ["below-9999", "straddles-9999", "up-to-1e21"])
def test_grib1_bitmap(tmp_path, case):
    """Catches GDAL's missing value for masked points: 9999 when the data is clear of it, else
    9.999e20 or the maximum plus one (both need the data decoded)."""
    ny, nx = 15, 19
    if case == "up-to-1e21":
        v, d = np.linspace(0.0, 1.2e21, ny * nx), -17
    else:
        base, amplitude, d = (280.0, 25.0, 1) if case == "below-9999" else (10000.0, 6000.0, 0)
        v = g1_field(ny, nx, 5, base=base, amplitude=amplitude).reshape(-1)
    keep = ~masked(ny, nx, 3).reshape(-1)
    grid = g1_latlon(nx, ny, 30000, 10000, 16000, 28000, 1000, 1000)
    path = latlon_file(tmp_path, "b.grb", [
        g1_message(v, g1_pds(param=2, level=(102, 0), d=d, bitmap=True), grid, d=d, keep=keep),
        g1_message(v, g1_pds(param=11, d=d, bitmap=True), grid, d=d, keep=keep),
    ])
    assert_same(path)


GRIB1_GRIDS = {
    "gaussian": g1_latlon(36, 18, 85761, 0, -85761, 350000, 10000, 9, typ=4),
    "rotated": g1_latlon(30, 20, -10000, -15000, 9000, 14000, 1000, 1000, typ=10, rotation=(-40000, 10000, 0.0)),
    "rotated-angle": g1_latlon(30, 20, -10000, -15000, 9000, 14000, 1000, 1000, typ=10, rotation=(-35000, 15000, 12.5)),
    "polar-north": g1_polar(30, 20, 30000, -135000, -105000, 47625),
    "polar-south": g1_polar(30, 20, -40000, 10000, 0, 25000, south=True),
    "lambert": g1_lambert(30, 20, 12190, -133459, -95000, 40635, 25000, 25000),
    "lambert-oblate": g1_lambert(30, 20, 20000, -120000, -100000, 30000, 33000, 45000, res=0x48),
    "mercator": g1_mercator(30, 20, -20000, 100000, 20000, 150000, 20000, 25000, 25000),
    "latlon-oblate": g1_latlon(30, 20, 60000, 0, 41000, 29000, 1000, 1000, res=0xC0),
    "latlon-0-360": g1_latlon(72, 20, 90000, 0, -5000, 355000, 5000, 5000),
}


@pytest.mark.parametrize("grid", GRIB1_GRIDS, ids=list(GRIB1_GRIDS))
@pytest.mark.parametrize("center", [7, 98], ids=["ncep", "ecmwf"])
def test_grib1_grids(tmp_path, grid, center):
    """Catches GRIB1 grid definitions and shapes of the earth (NCEP's sphere is 6371.2 km, others'
    6367.47 km or IAU 1965's spheroid), with ECMWF's table 128 names."""
    gds = GRIB1_GRIDS[grid]
    nx, ny = int.from_bytes(gds[6:8], "big"), int.from_bytes(gds[8:10], "big")
    v = g1_field(ny, nx, 6).reshape(-1)
    table, param = (128, 167) if center == 98 else (2, 11)
    pds = g1_pds(param=param, center=center, table=table)
    path = latlon_file(tmp_path, "g.grb", [g1_message(v, pds, gds), g1_message(v[::-1], g1_pds(param=param, center=center, table=table, p1=12), gds)])
    assert_same(path)


@pytest.mark.parametrize("pds", [
    dict(param=61, level=(1, 0), unit=1, p1=0, p2=6, time_range=4),
    dict(param=11, level=(112, 10 * 256 + 40), unit=0, p1=30),
    dict(param=33, level=(100, 850), unit=254, p1=200, time_range=10),
    dict(param=250, level=(105, 10), table=2),
    dict(param=191, center=7, subcenter=2, level=(1, 0)),
    dict(param=11, center=78, table=2, level=(105, 2)),
    dict(param=11, center=7, table=129, level=(105, 2)),
], ids=["accumulation", "two-part-level", "seconds-p1-p2", "undefined-param", "ensemble", "dwd", "ncep-129"])
def test_grib1_names_and_times(tmp_path, pds):
    v = g1_field(8, 9, 7).reshape(-1)
    grid = g1_latlon(9, 8, 10000, 0, 3000, 8000, 1000, 1000)
    path = latlon_file(tmp_path, "n.grb", [g1_message(v, g1_pds(**pds), grid)])
    assert_same(path)


# GRIB2 grid templates 3.1 (rotated latitude/longitude) and 3.12 (transverse Mercator).


def with_grid(message, template, extra, edit=None):
    """A message with its grid definition's template number replaced and octets appended."""
    s = dict(sections(message))
    s3 = bytearray(s[3])
    s3[12:14] = template.to_bytes(2, "big")
    if edit:
        edit(s3)
    s3 += extra
    s3[0:4] = len(s3).to_bytes(4, "big")
    secs = [s[1], bytes(s3), s[4], s[5], s[6], s[7]]
    return build(message[:16], secs)


@pytest.mark.parametrize("pole", [(-40.0, 10.0, 0.0), (-31.758312, 87.597225, 0.0), (-90.0, 0.0, 25.0)], ids=["europe", "rdpa", "angle"])
@pytest.mark.parametrize("crs", ["EPSG:4326", "+proj=longlat +a=6371229 +b=6371229 +no_defs"], ids=["wgs84", "sphere"])
def test_rotated_latlon_grid(tmp_path, pole, crs):
    """Catches template 3.1: the rotated grid's extent and GDAL's derived geographic CRS."""
    src = write_grib(tmp_path / "s.grib2", field(9, 13, bands=2, seed=31), crs=crs, transform=from_origin(-20.0, 10.0, 0.5, 0.5), DATA_ENCODING="SIMPLE_PACKING")
    lat, lon, angle = pole
    extra = sign_magnitude(round(lat * 1e6), 4) + (round(lon % 360 * 1e6)).to_bytes(4, "big") + round(angle * 1e6).to_bytes(4, "big")
    out = b"".join(with_grid(m, 1, extra) for m in messages(open(src, "rb").read()))
    path = tmp_path / "rotated.grib2"
    path.write_bytes(out)
    assert_same(str(path))


@pytest.mark.parametrize("crs", ["EPSG:32611", "EPSG:32733", "+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +ellps=airy +units=m +no_defs"], ids=["utm11n", "utm33s", "british-grid"])
def test_transverse_mercator_grid(tmp_path, crs):
    """Catches template 3.12, which GDAL writes for UTM (a tile server's own GRIB test asset)."""
    transform = from_origin(500000.0, 4100000.0, 30.0, 30.0)
    path = write_grib(tmp_path / "tm.grib2", field(10, 10, bands=2, seed=37), crs=crs, transform=transform, DATA_ENCODING="COMPLEX_PACKING")
    with rasterio.open(path) as g:
        assert g.crs is not None
    assert_same(path)


def test_negative_longitudes_sign_and_magnitude(tmp_path):
    """Catches longitudes read unsigned where g2clib's map reads them signed (3.0's La1/Lo1/Lo2)."""
    src = write_grib(tmp_path / "s.grib2", field(9, 13, seed=41), crs="EPSG:4326", transform=from_origin(-20.0, 10.0, 0.5, 0.5), DATA_ENCODING="SIMPLE_PACKING")

    def negative(s3):
        s3[50:54] = sign_magnitude(-19_750_000, 4)
        s3[59:63] = sign_magnitude(-13_750_000, 4)

    path = tmp_path / "neg.grib2"
    path.write_bytes(b"".join(with_grid(m, 0, b"", negative) for m in messages(open(src, "rb").read())))
    assert_same(str(path))


# Remote files read by range.


def serve(data, ranges=True):
    """Serves `data` over HTTP on a loopback port, honouring range requests (or, with `ranges`
    false, ignoring them as some servers do: the whole file every time): the URL and a log of
    `(range header, bytes sent)`."""
    import http.server
    import threading

    log = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            rng = self.headers.get("Range")
            partial = ranges and rng and rng.startswith("bytes=")
            if partial:
                a, b = rng[6:].split("-")
                a = int(a)
                b = min(int(b) if b else len(data) - 1, len(data) - 1)
                body = data[a : b + 1]
            else:
                body = data
            # Logged before anything is sent: a client that refuses the answer on its headers
            # may look at the log before this thread runs on, and the request must count.
            log.append((rng, len(body)))
            if partial:
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {a}-{b}/{len(data)}")
            else:
                self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}", log


BLOCK = 16 * 1024


def message_spans(data):
    out, pos = [], 0
    while (i := data.find(b"GRIB", pos)) >= 0:
        n = int.from_bytes(data[i + 8 : i + 16], "big") if data[i + 7] == 2 else int.from_bytes(data[i + 4 : i + 7], "big")
        out.append((i, n))
        pos = i + n
    return out


@pytest.mark.parametrize("band", [1, 7, 12])
def test_remote_reads_by_range(tmp_path, band):
    """Catches a remote GRIB file downloaded whole: reading a band receives at most its message
    plus one block per message (the headers), and gives GDAL's values."""
    data = field(200, 250, bands=12, seed=43)
    path = write_grib(tmp_path / "r.grib2", data, DATA_ENCODING="IEEE_FLOATING_POINT", **LATLON)
    raw = open(path, "rb").read()
    spans = message_spans(raw)
    server, url, log = serve(raw)
    try:
        with trio.open(f"{url}/r.grib2") as t:
            ours = t.read(band)
        received = sum(n for _, n in log)
        ceiling = spans[band - 1][1] + len(spans) * BLOCK
        assert received <= ceiling, (received, ceiling, len(raw), log)
        with rasterio.open(path) as g:
            assert np.array_equal(ours, g.read(band))
    finally:
        server.shutdown()


def grib1_remote_file(tmp_path):
    """Six GRIB1 messages of 160 x 120 points with bitmaps, each larger than a block: geopotential
    heights, every other one straddling 9999 (GDAL's nodata is then 9.999e20, which depends on
    the data's maximum, so the message is read whole at open), the others clear of it."""
    ny, nx = 120, 160
    grid = g1_latlon(nx, ny, 60000, -20000, 500, 59500, 500, 500)
    msgs = []
    for k in range(6):
        keep = ~masked(ny, nx, 20 + k).reshape(-1)
        if k % 2 == 0:
            values, d = g1_field(ny, nx, k, base=10000.0, amplitude=5000.0), 0
        else:
            values, d = g1_field(ny, nx, k, base=2000.0, amplitude=500.0), 1
        pds = g1_pds(param=7, level=(100, 500), p1=k, d=d, bitmap=True)
        msgs.append(g1_message(values.reshape(-1), pds, grid, d=d, keep=keep))
    return msgs, latlon_file(tmp_path, "r.grb", msgs)


@pytest.mark.parametrize("band", [1, 4])
def test_remote_grib1_by_range(tmp_path, band):
    """Catches a remote GRIB1 file read beyond its ceiling: reading a band
    receives at most its message plus one block per message, although every message has a
    bitmap and three are read whole for their nodata; the nodata and pixels are GDAL's. The
    whole file is larger than the ceiling, so downloading it whole is caught too."""
    msgs, path = grib1_remote_file(tmp_path)
    raw = b"".join(msgs)
    spans = message_spans(raw)
    ceiling = spans[band - 1][1] + len(spans) * BLOCK
    assert len(raw) > ceiling
    server, url, log = serve(raw)
    try:
        with trio.open(f"{url}/r.grb") as t, rasterio.open(path) as g:
            assert t.nodatavals == g.nodatavals
            assert 9.999e20 in g.nodatavals and 9999.0 in g.nodatavals
            assert np.array_equal(t.read(band), g.read(band))
            received = sum(n for _, n in log)
            assert received <= ceiling, (received, ceiling, len(raw), len(log))
            assert [t.tags(b)["GRIB_FORECAST_SECONDS"] for b in (1, 6)] == [g.tags(b)["GRIB_FORECAST_SECONDS"] for b in (1, 6)]
    finally:
        server.shutdown()


def grib1_bitmaps_larger_than_a_block(tmp_path):
    """Twelve GRIB1 messages of 400 x 400 points whose bitmaps (20,000 bytes) are each larger
    than a block. The packing range of the first spans 9999, so it is read whole at open: its
    nodata depends on its data, as NARR's 26 such messages."""
    ny, nx = 400, 400
    grid = g1_latlon(nx, ny, 60000, -20000, 150, 59800, 200, 150)
    msgs = []
    for k in range(12):
        keep = ~masked(ny, nx, 40 + k).reshape(-1)
        if k == 0:
            values, d = g1_field(ny, nx, k, base=10000.0, amplitude=5000.0), 0
        else:
            values, d = g1_field(ny, nx, k, base=2000.0, amplitude=500.0), 1
        pds = g1_pds(param=7, level=(100, 500), p1=k, d=d, bitmap=True)
        msgs.append(g1_message(values.reshape(-1), pds, grid, d=d, e=6, nbits=8, keep=keep))
    return msgs, latlon_file(tmp_path, "b.grb", msgs)


@pytest.mark.parametrize("band", [1, 7])
def test_remote_grib1_headers_skip_bitmaps(tmp_path, band):
    """Catches GRIB1 bitmap bodies read at open: here each is larger than the block per
    message the ceiling allows, so reading them breaks it; so does reading whole a message whose
    nodata does not depend on its data. Pixels and nodata are GDAL's."""
    msgs, path = grib1_bitmaps_larger_than_a_block(tmp_path)
    raw = b"".join(msgs)
    spans = message_spans(raw)
    ceiling = spans[band - 1][1] + len(spans) * BLOCK
    server, url, log = serve(raw)
    try:
        with trio.open(f"{url}/b.grb") as t, rasterio.open(path) as g:
            assert t.nodatavals == g.nodatavals
            assert np.array_equal(t.read(band), g.read(band))
            received = sum(n for _, n in log)
            assert received <= ceiling, (received, ceiling, len(raw), len(log))
    finally:
        server.shutdown()


def test_remote_grib_from_a_server_that_ignores_ranges(tmp_path):
    """A one-message GRIB file under 10 blocks (160 KiB) from a server that ignores range
    requests reads as GDAL reads it: the first response holds the whole file, and the band's
    message is taken from it."""
    msgs, path = grib1_remote_file(tmp_path)
    raw = msgs[1]
    assert BLOCK < len(raw) < 10 * BLOCK
    one = tmp_path / "one.grb"
    one.write_bytes(raw)
    server, url, log = serve(raw, ranges=False)
    try:
        with trio.open(f"{url}/one.grb") as t, rasterio.open(str(one)) as g:
            assert t.nodatavals == g.nodatavals
            assert np.array_equal(t.read(1), g.read(1))
    finally:
        server.shutdown()


def test_remote_grib_too_large_for_a_server_that_ignores_ranges(tmp_path):
    """The other side of the test above: a file larger than 10 blocks from a server that
    ignores range requests is still refused, after one bounded response, rather than downloaded
    whole."""
    msgs, _ = grib1_remote_file(tmp_path)
    raw = b"".join(msgs) * 2
    assert len(raw) > 20 * BLOCK
    server, url, log = serve(raw, ranges=False)
    try:
        with pytest.raises(Exception, match="range"):
            with trio.open(f"{url}/r.grb") as t:
                t.read(1)
        assert len(log) == 1
    finally:
        server.shutdown()


def test_mask_flags_scale_linearly_with_bands(tmp_path):
    """The mask calls of a many-band file cost GDAL's order of time: building every band's
    mask flags is linear in the band count (quadratic, it would take hundreds of seconds on a NARR
    file of 423 bands, mask_flag_enums 20 times). Each call on a newly opened dataset."""

    def cost(n):
        data = field(2, 3, bands=n, seed=3, nodata=True)
        path = write_grib(
            tmp_path / f"m{n}.grib2", data, nodata=NODATA, DATA_ENCODING="SIMPLE_PACKING", **LATLON
        )
        best = float("inf")
        for _ in range(5):
            with trio.open(path) as t:
                t0 = time.perf_counter()
                _ = t.mask_flag_enums
                best = min(best, time.perf_counter() - t0)
        return best

    small, large = cost(100), cost(800)
    # 8 times the bands: linear is about 8 times the time, quadratic 64 times.
    assert large < 24 * small, (small, large)


# Allocation sized by a header: an error, never an abort (a subprocess under RLIMIT_AS).

HUGE = r"""
import resource, sys
resource.setrlimit(resource.RLIMIT_AS, (4 << 30, 4 << 30))
import terrio.rio as rio
try:
    with rio.open(sys.argv[1]) as d:
        d.read(1, window=rio.windows.Window(0, 0, 4, 4))
    print("READ")
except Exception as e:
    print("ERROR", type(e).__name__, e)
"""


def huge_grib2(tmp_path):
    """A constant GRIB2 field of 65536 x 65535 points (simple packing of 0 bits), the file padded
    past degrib's 1000-points-a-byte guard."""
    src = write_grib(tmp_path / "s.grib2", field(4, 4, seed=1), DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    msg = messages(open(src, "rb").read())[0]
    s = dict(sections(msg))
    nx, ny = 65536, 65535
    s3 = bytearray(s[3])
    s3[6:10] = (nx * ny).to_bytes(4, "big")
    s3[30:34], s3[34:38] = nx.to_bytes(4, "big"), ny.to_bytes(4, "big")
    s5 = bytearray(s[5])
    s5[5:9] = (nx * ny).to_bytes(4, "big")
    s5[19] = 0
    return build(msg[:16], [s[1], bytes(s3), s[4], bytes(s5), s[6], section(7, b"")]) + bytes(nx * ny // 1000 + 1)


def huge_grib1():
    nx = ny = 65534
    grid = g1_latlon(nx, ny, 60000, 0, 0, 60000, 1, 1)
    return g1_message(np.zeros(1), g1_pds(), grid, nbits=0) + bytes(nx * ny // 1000 + 1)


@pytest.mark.parametrize("edition", [1, 2])
def test_huge_grid_is_an_error_not_an_abort(tmp_path, edition):
    """Catches an allocation sized by a header that aborts the process when it fails (a Rust
    `vec!` of 17 or 34 GB) instead of raising."""
    import subprocess
    import sys

    path = tmp_path / f"huge{edition}.grib"
    path.write_bytes(huge_grib1() if edition == 1 else huge_grib2(tmp_path))
    p = subprocess.run([sys.executable, "-c", HUGE, str(path)], capture_output=True, text=True, timeout=300)
    assert p.returncode == 0, (p.returncode, p.stdout[-500:], p.stderr[-500:])
    assert p.stdout.startswith("ERROR"), p.stdout


def test_allow_list_finds_grib_after_a_header(tmp_path):
    """Catches content sniffing that misses a GRIB message after a WMO bulletin header (GDAL
    finds "GRIB" and an edition in the first kilobyte): the allow-list would refuse the file."""
    src = write_grib(tmp_path / "s.grib2", field(6, 7, seed=47), DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    path = tmp_path / "bulletin"
    path.write_bytes(b"\x01\r\r\n001 \r\r\nYTXA98 KWBC 170600\r\r\n" + open(src, "rb").read())
    terrio.check_drivers(str(path), ["GRIB"])
    with pytest.raises(Exception):
        terrio.check_drivers(str(path), ["GTiff"])
    assert_same(str(path))


def test_tile_server_test_asset(tmp_path):
    """A tile server's tests write this GRIB2: UTM zone 11N, which GDAL encodes as grid template
    3.12."""
    import affine

    transform = affine.Affine(14.25739965507221, 0, 305827.93, 0, -14.25739965507221, 5223236.6)
    dest = tmp_path / "data.grib2"
    with rasterio.open(dest, "w", driver="GRIB", height=10, width=10, count=1, dtype="float32", crs=RioCRS.from_epsg(32611), transform=transform) as dst:
        dst.write(np.ones((1, 10, 10), dtype=np.float32))
    assert_same(str(dest))



# CCSDS packing (template 5.42). GDAL decodes it with libaec, terrio with the pure-Rust rust-aec.
# The file is ecCodes 2.50's (pip eccodes + eccodeslib, libaec inside), zlib-compressed: 9
# messages of 24 x 13 points, at 8, 10, 12, 16, 24 and 32 bits a value, blocks of 8, 16, 32 and
# 64 samples, reference sample intervals of 2 to 128 blocks, the preprocessor on (flags 14) and
# off (flags 6), two with a bitmap.
CCSDS = (
    "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"
)


def test_ccsds_packing(tmp_path):
    """Catches template 5.42 decoded unlike libaec: a wrong reference sample interval, block
    size, sample width or preprocessor (the intervals of 2 to 16 blocks restart within a field)."""
    import base64
    import zlib

    path = tmp_path / "ccsds.grib2"
    path.write_bytes(zlib.decompress(base64.b64decode(CCSDS)))
    assert_same(str(path))


@pytest.mark.parametrize("template", [0, 1, 40])
@pytest.mark.parametrize("missing", ["La1", "Lo1", "La2", "Lo2", "Di", "Dj", "subdivision-0", "no-parallels"])
def test_missing_grid_points(tmp_path, template, missing):
    """GDAL reads a lat/lon grid whose first or last point (or increment) is missing (all ones),
    whose subdivision is 0 or whose Gaussian grid has no parallels: degrib's `ParseSect3` stops
    there, GDAL ignores the error and keeps what was parsed, the rest zeros."""
    if missing == "no-parallels" and template != 40:
        pytest.skip("Gaussian grids only")
    src = write_grib(tmp_path / "s.grib2", field(9, 13, seed=41), DATA_ENCODING="SIMPLE_PACKING", **LATLON)
    where = {"La1": 46, "Lo1": 50, "La2": 55, "Lo2": 59, "Di": 63, "Dj": 67}
    rotation = sign_magnitude(-40_000_000, 4) + (10_000_000).to_bytes(4, "big") + bytes(4) if template == 1 else b""

    def edit(s3):
        if template == 40:
            s3[67:71] = (9).to_bytes(4, "big")
        if missing in where:
            s3[where[missing] : where[missing] + 4] = b"\xff" * 4
        elif missing == "subdivision-0":
            s3[38:42], s3[42:46] = (1).to_bytes(4, "big"), bytes(4)
        else:
            s3[67:71] = bytes(4)

    path = tmp_path / "m.grib2"
    path.write_bytes(b"".join(with_grid(m, template, rotation, edit) for m in messages(open(src, "rb").read())))
    assert_same(str(path))


@pytest.mark.parametrize("grid", ["grib2-wgs84", "grib2-sphere-angle", "grib1-rotated-angle"])
def test_rotated_grid_reprojects_as_gdal(tmp_path, grid):
    """Catches a rotated-pole CRS transformed unlike PROJ (a tile server reprojects every asset to
    WebMercatorQuad): bounds, warped grids and nearest-neighbour pixels in EPSG:3857 and 4326."""
    import rasterio.vrt
    import rasterio.warp

    import terrio.rio.vrt  # noqa: F401
    import terrio.rio.warp  # noqa: F401

    if grid == "grib1-rotated-angle":
        gds = GRIB1_GRIDS["rotated-angle"]
        path = latlon_file(tmp_path, "r.grb", [g1_message(g1_field(20, 30, 6).reshape(-1), g1_pds(), gds)])
    else:
        crs, pole = ("EPSG:4326", (-40.0, 10.0, 0.0)) if grid == "grib2-wgs84" else ("+proj=longlat +a=6371229 +b=6371229 +no_defs", (-35.0, 15.0, 25.0))
        src = write_grib(tmp_path / "s.grib2", field(9, 13, seed=31), crs=crs, transform=from_origin(-20.0, 10.0, 0.5, 0.5), DATA_ENCODING="SIMPLE_PACKING")
        lat, lon, angle = pole
        extra = sign_magnitude(round(lat * 1e6), 4) + round(lon % 360 * 1e6).to_bytes(4, "big") + round(angle * 1e6).to_bytes(4, "big")
        path = tmp_path / "rotated.grib2"
        path.write_bytes(b"".join(with_grid(m, 1, extra) for m in messages(open(src, "rb").read())))
        path = str(path)
    with rasterio.open(path) as g, trio.open(path) as t:
        for dst in ("EPSG:3857", "EPSG:4326"):
            ours = trio.warp.transform_bounds(t.crs, dst, *t.bounds)
            np.testing.assert_allclose(ours, rasterio.warp.transform_bounds(g.crs, dst, *g.bounds), rtol=1e-12, atol=1e-6)
            with rasterio.vrt.WarpedVRT(g, crs=dst) as gv, trio.vrt.WarpedVRT(t, crs=dst) as tv:
                assert tv.shape == gv.shape
                np.testing.assert_allclose(tv.transform.to_gdal(), gv.transform.to_gdal(), rtol=1e-12, atol=1e-6)
                assert np.array_equal(tv.read(1), gv.read(1))


# The GRIB fixtures of #232 (GDAL's test files and files GDAL wrote), kept as
# corpus under the same names and bytes.

FIXTURES = Path(__file__).resolve().parents[2] / "crates" / "terrio" / "tests" / "data" / "grib"
GRIB_FIXTURES = [
    "MRMS_EchoTop_18_00.50_20161015-133230.grib2",
    "Sample_QuikSCAT.grb",
    "complex_missing.grib2",
    "complex_sd2_missing.grib2",
    "gfs.t06z.pgrb2.1p0.grib2",
    "ieee754_single.grb2",
    "jpeg2000_lcc.grib2",
    "jpeg2000_nbits_zero_decimal_scaled.grb2",
    "lambert_conformal_conic.grb2",
    "png_nbits_zero_decimal_scaled.grb2",
    "png_vis.grib2",
    "polar_stereographic.grb2",
    "simple_packing_nbits_zero_decimal_scaled.grb2",
    "spatial_differencing_order_1.grb2",
    "subgrids_reuse_bitmap.grib2",
    "two_bands_with_bitmap.grib2",
]


@pytest.mark.parametrize("name", GRIB_FIXTURES)
def test_grib_fixtures_read_as_gdal(name):
    """Every check of `assert_same` (pixels, nodata, masks, georeferencing, CRS, descriptions,
    color interpretation, GRIB_* tags) on the fixtures of #232. The two in Clarke 1866 transverse
    Mercator are checked but for the EPSG code (see `test_epsg_identified_as_gdal`)."""
    assert_same(str(FIXTURES / name), epsg=name not in UTM_CLARKE)


UTM_CLARKE = (
    "subgrids_reuse_bitmap.grib2",
    "two_bands_with_bitmap.grib2",
    "parameter_in_wmo_tables_only.grb2",
)


@pytest.mark.xfail(
    reason="terrio's CRS module (crs group, rio/crs.py) does not identify EPSG:26711 for GDAL's "
    "own WKT of these grids; the GRIB driver writes GDAL's WKT text",
    strict=False,
)
@pytest.mark.parametrize("name", UTM_CLARKE)
def test_epsg_identified_as_gdal(name):
    with rasterio.open(str(FIXTURES / name)) as g, trio.open(str(FIXTURES / name)) as t:
        assert t.crs.to_wkt() == g.crs.to_wkt()
        assert t.crs.to_epsg() == g.crs.to_epsg() == 26711


def test_truncated_grib_fixture_is_refused():
    """`rotated_pole.grb2.begin`, the start of a message that GDAL's tests cut: refused, as GDAL
    refuses it."""
    path = str(FIXTURES / "rotated_pole.grb2.begin")
    with pytest.raises(rasterio.errors.RasterioIOError):
        rasterio.open(path)
    with pytest.raises(Exception, match="truncated"):
        trio.open(path)


# GDAL's own test files (GDAL v3.12.2 autotest/gdrivers/data/grib, MIT) for what the GRIB driver
# of #232 read as GDAL and this one did not.
GDAL_AUTOTEST = [
    # Albers equal area (template 3.31) and Lambert azimuthal equal area (3.140).
    "albers_equal_area.grb2",
    "lambert_azimuthal_equal_area.grb2",
    "laea_with_negative_longitudes_issue_7456.grib2",
    # A shape of the earth without its radius: GDAL reads the grid without georeferencing.
    "byte_wrong_earth_shape.grib2",
    # JMA's MANAL products: a Lambert grid registered by its northern-most point (GDAL's heuristic).
    "MANAL_2023030103_fake_wrong_grid_origin_latitude.grb2",
    # Names: a parameter only in the WMO's tables (its long name for a short one), NDFD's names
    # (MinT), templates 4.48 and 4.32 (assembled values; 4.32 has no fixed surfaces), and a
    # template degrib does not name (4.65535: "unknown").
    "parameter_in_wmo_tables_only.grb2",
    "ds.mint.bin",
    "template_4_48.grb2",
    "template_4_65535.grb2",
    "twenty-se27w.2017102006.hwrfsat.core.0p02.f000_truncated.grb2",
]


# GDAL identifies EPSG:26711 (NAD27 / UTM zone 11N) for these grids, terrio's CRS module does not
# (see `test_epsg_identified_as_gdal`): every other check holds.
EPSG_NOT_IDENTIFIED = ("parameter_in_wmo_tables_only.grb2",)


@pytest.mark.parametrize("name", GDAL_AUTOTEST)
def test_gdal_autotest_grids_read_as_gdal(name):
    assert_same(str(FIXTURES / name), epsg=name not in EPSG_NOT_IDENTIFIED)


def test_grib1_reads_at_gdal_speed(tmp_path):
    """NARR-like GRIB1 (many messages with bitmaps) read whole at least as fast as GDAL: it was
    twice as slow (NARR, 423 bands: 7.5 s against 3.6 s), the parameter's tables looked up for
    every value. The best of three reads each, in the same process."""
    ny, nx = 150, 200
    grid = g1_latlon(nx, ny, 60000, -20000, 400, 59600, 400, 400)
    keep = ~masked(ny, nx, 7).reshape(-1)
    msgs = [
        g1_message(g1_field(ny, nx, k).reshape(-1), g1_pds(p1=k, bitmap=True), grid, keep=keep)
        for k in range(60)
    ]
    path = latlon_file(tmp_path, "many.grb", msgs)

    def best(open_):
        t = float("inf")
        for _ in range(3):
            t0 = time.perf_counter()
            with open_(path) as ds:
                ds.read()
            t = min(t, time.perf_counter() - t0)
        return t

    ours, gdal = best(trio.open), best(rasterio.open)
    assert ours <= gdal, (ours, gdal)


# GRIB2 writing: terrio.rio.open(..., "w", driver="GRIB") and terrio.rio.shutil.copy against
# GDAL's GRIB driver. The same raster and options give the same bytes, so GDAL (and terrio) read
# terrio's file as GDAL's own: values, georeferencing, CRS and every GRIB_* item.

# A tile server's test data: 10 x 10 Float32 ones in UTM zone 11N (grid template 3.12).
TILE_SERVER_DATA = dict(
    crs="EPSG:32611",
    transform=Affine(14.25739965507221, 0, 305827.93, 0, -14.25739965507221, 5223236.6),
)
GEOGRAPHIC = dict(crs="EPSG:4326", transform=from_origin(-20.0, 60.0, 0.5, 0.5))


def smooth(bands, height, width, dtype, base=280.0, amplitude=25.0):
    rng = np.random.default_rng(bands * 1000 + height)
    y, x = np.mgrid[0:height, 0:width]
    noise = rng.normal(0, 0.7, (bands, height, width))
    out = base + amplitude * np.sin(y / 5.0 + np.arange(bands)[:, None, None]) * np.cos(x / 7.0)
    return (out + noise).astype(dtype)


def projected(crs, x, y, res):
    return dict(crs=crs, transform=from_origin(x, y, res, res))


SIMPLE = {"DATA_ENCODING": "SIMPLE_PACKING"}
WRITES = {
    "tile-server-data": (np.ones((1, 10, 10), np.float32), TILE_SERVER_DATA, {}),
    "tile-server-data-simple": (np.ones((1, 10, 10), np.float32), TILE_SERVER_DATA, SIMPLE),
    "float32": (smooth(1, 20, 30, np.float32), GEOGRAPHIC, {}),
    "float64": (smooth(1, 20, 30, np.float64), GEOGRAPHIC, {}),
    "float32-simple": (smooth(1, 20, 30, np.float32), GEOGRAPHIC, SIMPLE),
    "float32-simple-wide": (smooth(1, 20, 30, np.float32, amplitude=900.0), GEOGRAPHIC, SIMPLE),
    "float32-simple-decimal": (
        smooth(1, 20, 30, np.float32, amplitude=900.0),
        GEOGRAPHIC,
        {**SIMPLE, "DECIMAL_SCALE_FACTOR": "1"},
    ),
    "float32-simple-nbits": (smooth(1, 20, 30, np.float32), GEOGRAPHIC, {**SIMPLE, "NBITS": "12"}),
    "uint8": ((np.arange(600) % 251).reshape(1, 20, 30).astype(np.uint8), GEOGRAPHIC, {}),
    "int16": (smooth(1, 20, 30, np.float32, -500.0, 3000.0).astype(np.int16), GEOGRAPHIC, {}),
    "int32": (smooth(1, 20, 30, np.float64, amplitude=1e6).astype(np.int32), GEOGRAPHIC, {}),
    "uint16-3-bands": (smooth(3, 20, 30, "float32", 900.0, 800.0).astype("uint16"), GEOGRAPHIC, {}),
    "global-longitudes": (
        smooth(1, 18, 36, np.float32),
        dict(crs="EPSG:4326", transform=from_origin(-180.0, 90.0, 10.0, 10.0)),
        {},
    ),
    "lambert-conformal": (
        smooth(1, 20, 30, np.float32), projected("EPSG:2154", 300000.0, 6900000.0, 1000.0), {}
    ),
    "albers": (smooth(1, 20, 30, np.float32), projected("EPSG:5070", -5e5, 2e6, 1000.0), {}),
    "transverse-mercator-airy": (
        smooth(1, 20, 30, np.float32), projected("EPSG:27700", 400000.0, 300000.0, 100.0), {}
    ),
    "sphere": (
        smooth(1, 20, 30, np.float32),
        dict(crs="+proj=longlat +R=6371229 +no_defs", transform=from_origin(-20.0, 60.0, 0.5, 0.5)),
        {},
    ),
    "temperature-celsius": (
        smooth(1, 20, 30, np.float32, base=10.0),
        GEOGRAPHIC,
        {"PDS_PDTN": "0", "PDS_TEMPLATE_NUMBERS": TEMPERATURE},
    ),
    "products": (
        smooth(1, 20, 30, np.float32),
        GEOGRAPHIC,
        {
            "DISCIPLINE": "10",
            "IDS": "CENTER=7 SUBCENTER=4 SIGNF_REF_TIME=1 REF_TIME=2024-05-06T12:00:00Z "
            "PROD_STATUS=0 TYPE=1",
        },
    ),
}


def write_both(tmp_path, data, georef, options):
    """The raster written by terrio and by GDAL: the paths."""
    paths = tmp_path / "ours.grib2", tmp_path / "gdal.grib2"
    bands, height, width = data.shape
    for lib, path in zip((trio, rasterio), paths, strict=True):
        with lib.open(
            path, "w", driver="GRIB", count=bands, height=height, width=width,
            dtype=data.dtype.name, **georef, **options,
        ) as dst:
            dst.write(data)
    return paths


@pytest.mark.parametrize("case", WRITES)
def test_grib_written_as_gdal_writes_it(tmp_path, case):
    """Catches the GRIB2 writer departing from GDAL's: grid templates 3.0, 3.12, 3.30 and 3.31,
    the earth's shape, simple packing (with GDAL's bits, binary and decimal scales) and IEEE,
    integer and floating-point bands, products, temperatures in Celsius written in Kelvin, and
    global grids rewrapped from 0 degrees east: the same bytes as GDAL's. Lossless encodings read
    back as written (in single precision: GDAL decodes GRIB values as floats). EPSG:5070 read back
    is not identified by terrio's CRS module (a crs-group gap), so its EPSG code is not compared."""
    data, georef, options = WRITES[case]
    ours, gdal = write_both(tmp_path, data, georef, options)
    assert ours.read_bytes() == gdal.read_bytes()
    assert_same(str(ours), epsg=case != "albers")
    lossless = options.get("DATA_ENCODING") != "SIMPLE_PACKING" or data.dtype.kind in "iu"
    if lossless and "PDS_TEMPLATE_NUMBERS" not in options:
        with rasterio.open(ours) as g:
            assert np.array_equal(g.read(), data.astype(np.float32).astype(np.float64))


def test_grib_simple_packing_keeps_a_power_of_two_range(tmp_path):
    """Catches g2clib's bit count for values spanning exactly 2^n, one bit short: GDAL's own
    writer wraps the largest values to the smallest (256 to 0 here); terrio writes the bit, and
    GDAL reads every value back."""
    data = np.arange(257, dtype=np.uint16).reshape(1, 1, 257)
    path = tmp_path / "range.grib2"
    georef = dict(crs="EPSG:4326", transform=from_origin(0.0, 1.0, 0.1, 0.1))
    profile = dict(driver="GRIB", count=1, height=1, width=257, dtype="uint16")
    with trio.open(path, "w", **profile, **georef) as dst:
        dst.write(data)
    with rasterio.open(path) as g:
        assert np.array_equal(g.read(), data.astype(np.float64))


@pytest.mark.parametrize("name", ["temperature.grib2", "simple.grib2", "ieee.grib2", "lcc.grib2"])
def test_grib_copied_to_grib_as_gdal(tmp_path, name):
    """Catches a copy to GRIB losing the source's products: as GDAL's, the copy writes each band's
    GRIB_DISCIPLINE, GRIB_IDS, GRIB_PDS_PDTN and GRIB_PDS_TEMPLATE_NUMBERS (and temperatures
    back in Kelvin): the same bytes as GDAL's copy."""
    src = str(FIXTURES / name)
    ours, gdal = tmp_path / "ours.grib2", tmp_path / "gdal.grib2"
    trio_copy(src, ours, driver="GRIB")
    rio_copy(src, gdal, driver="GRIB")
    assert ours.read_bytes() == gdal.read_bytes()
    assert_same(str(ours))


def test_grib_writer_refuses_what_it_does_not_write(tmp_path):
    """Nodata pixels (GDAL writes them with complex packing) and projections terrio does not
    write are refused, not written differently."""
    data = smooth(1, 20, 30, np.float32)
    data[0, 0, :5] = -9999.0
    with pytest.raises(Exception, match="COMPLEX_PACKING"):
        with trio.open(
            tmp_path / "n.grib2", "w", driver="GRIB", count=1, height=20, width=30,
            dtype="float32", nodata=-9999.0, **GEOGRAPHIC,
        ) as dst:
            dst.write(data)
    with pytest.raises(Exception, match="projection"):
        with trio.open(
            tmp_path / "m.grib2", "w", driver="GRIB", count=1, height=20, width=30,
            dtype="float32", **projected("EPSG:3857", 0.0, 0.0, 1000.0),
        ) as dst:
            dst.write(data)

