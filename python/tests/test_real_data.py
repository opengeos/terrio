"""Tests on real-world files, with arrays and values checked against GDAL's Python bindings.

They run when `TERRIO_TEST_DATA` names the directory filled by `scripts/fetch_test_data.sh`,
and compare with GDAL when `osgeo.gdal` is installed. Tests reading public cloud files run when
`TERRIO_TEST_REMOTE` is set.
"""

import os

import numpy as np
import pytest

import terrio

RASTERS = [
    "dem.tif",
    "cog.tif",
    "landsat.tif",
    "landsat7.tif",
    "srtm90.tif",
    "srtm_sub.tif",
    "nlcd_knoxville.tif",
    "las_vegas_dem.tif",
    "OPERA_L3_DSWx_WTR.tif",
    "ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif",
    "USGS_1_n40w106.tif",
    "lv_dem.asc",
    "lv_dem.img",
    "lv_dem.envi",
    "lv_dem.jp2",
    "srtm_sub.bil",
    "srtm_sub.ers",
    "srtm_sub.grd",
    "srtm_sub.rst",
    "srtm_sub.sdat",
    "srtm_sub.nc",
    "srtm_sub_v3.zarr",
    "N37W120.hgt",
]

VECTORS = [
    "countries.gpkg",
    "countries.geojson",
    "countries.fgb",
    "countries.parquet",
    "ne_countries.zip",
    "TN_Counties.geojson",
    "UScounties.fgb",
    "las_vegas_roads.geojson",
]

REMOTE_COG = (
    "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/"
    "USGS_1_n40w106.tif"
)


def gdal_array(gdal, path, window=None, overview=None):
    """Reads every band of a raster with GDAL.

    Args:
        gdal: The `osgeo.gdal` module.
        path: The raster's path.
        window: `(col_off, row_off, width, height)`, or `None` for the whole image.
        overview: The overview to read, or `None` for full resolution.

    Returns:
        numpy.ndarray: The pixels, `(bands, rows, cols)`.
    """
    ds = gdal.Open(str(path))
    bands = []
    for i in range(1, ds.RasterCount + 1):
        band = ds.GetRasterBand(i)
        if overview is not None:
            band = band.GetOverview(overview)
        args = window if window is not None else (0, 0, band.XSize, band.YSize)
        bands.append(band.ReadAsArray(*args))
    return np.stack(bands)


def assert_same_pixels(actual, expected):
    """Asserts that two arrays have the same shape, dtype and values (NaN equal to NaN).

    Args:
        actual: terrio's array.
        expected: GDAL's array.
    """
    assert actual.shape == expected.shape
    assert actual.dtype == expected.dtype
    np.testing.assert_array_equal(actual, expected)


def raster_path(data_dir, name):
    """The path of a test raster, skipping the test when it is missing.

    Args:
        data_dir: The test data directory.
        name: The file name.

    Returns:
        str: The path.
    """
    path = data_dir / name
    if not path.exists():
        pytest.skip(f"{name} is not in the test data")
    return str(path)


@pytest.mark.parametrize("name", RASTERS)
def test_raster_matches_gdal(data_dir, gdal, name):
    path = raster_path(data_dir, name)
    r = terrio.open_raster(path)
    ds = gdal.Open(path)
    assert (r.width, r.height, r.count) == (ds.RasterXSize, ds.RasterYSize, ds.RasterCount)
    assert r.transform == pytest.approx(ds.GetGeoTransform(), rel=1e-12, abs=1e-9)
    nodata = ds.GetRasterBand(1).GetNoDataValue()
    if nodata is None or np.isnan(nodata):
        assert r.nodata is None or np.isnan(r.nodata) or nodata is None
    else:
        assert r.nodata == nodata
    assert_same_pixels(r.read(), gdal_array(gdal, path))


@pytest.mark.parametrize("name", [n for n in RASTERS if isinstance(n, str) and n.endswith(".tif")])
def test_lazy_windows_match_gdal(data_dir, gdal, name):
    path = raster_path(data_dir, name)
    r = terrio.open_raster(path, lazy=True)
    assert r.is_lazy
    rng = np.random.default_rng(42)
    for _ in range(5):
        w = int(rng.integers(1, min(300, r.width) + 1))
        h = int(rng.integers(1, min(300, r.height) + 1))
        x = int(rng.integers(0, r.width - w + 1))
        y = int(rng.integers(0, r.height - h + 1))
        assert_same_pixels(r.read(window=(x, y, w, h)), gdal_array(gdal, path, (x, y, w, h)))
    ds = gdal.Open(path)
    overviews = ds.GetRasterBand(1).GetOverviewCount()
    assert len(r.overviews) == overviews
    for i in range(overviews):
        ov = ds.GetRasterBand(1).GetOverview(i)
        assert r.overviews[i] == (ov.XSize, ov.YSize)
        assert_same_pixels(r.read(overview=i), gdal_array(gdal, path, overview=i))


def test_bytes_match_gdal(data_dir, gdal):
    path = raster_path(data_dir, "srtm_sub.tif")
    with open(path, "rb") as f:
        data = f.read()
    expected = gdal_array(gdal, path)
    assert_same_pixels(terrio.open_bytes(data, "srtm_sub.tif").read(), expected)
    lazy = terrio.open_bytes(data, "srtm_sub.tif", lazy=True)
    assert_same_pixels(lazy.read(window=(3, 5, 40, 30)), expected[:, 5:35, 3:43])


def test_written_rasters_read_by_gdal(data_dir, gdal, tmp_path):
    src = terrio.open_raster(raster_path(data_dir, "landsat.tif"))
    expected = src.read()
    for name, options in [("plain.tif", {}), ("deflate.tif", {"compress": "DEFLATE"})]:
        out = tmp_path / name
        src.write(str(out), **options)
        assert_same_pixels(gdal_array(gdal, out), expected)
        assert gdal.Open(str(out)).GetGeoTransform() == pytest.approx(src.transform)
    out = tmp_path / "from_array.tif"
    terrio.Raster.from_array(expected, src.transform, crs=src.crs, nodata=0).write(str(out))
    ds = gdal.Open(str(out))
    assert ds.GetRasterBand(1).GetNoDataValue() == 0
    assert ds.GetSpatialRef().GetAuthorityCode(None) == src.crs.split(":")[1]
    assert_same_pixels(gdal_array(gdal, out), expected)


def test_gpkg_matches_ogr_properties(data_dir, ogr):
    path = data_dir / "countries.gpkg"
    if not path.exists():
        pytest.skip("countries.gpkg is not in the test data")
    columns = terrio.open_vector(str(path))[0].columns()
    src = ogr.Open(str(path))
    for i, feature in enumerate(src.GetLayer(0)):
        for key, values in columns.items():
            expected = feature.GetField(key)
            if isinstance(expected, float):
                assert values[i] == pytest.approx(expected)
            else:
                assert values[i] == expected


@pytest.mark.parametrize("name", VECTORS)
def test_vector_matches_ogr(data_dir, ogr, name):
    path = data_dir / name
    if not path.exists():
        pytest.skip(f"{name} is not in the test data")
    ds = terrio.open_vector(str(path))
    gdal_path = f"/vsizip/{path}" if name.endswith(".zip") else str(path)
    src = ogr.Open(gdal_path)
    ogr_layers = {src.GetLayer(i).GetName(): src.GetLayer(i) for i in range(src.GetLayerCount())}
    for layer in ds:
        ogr_layer = ogr_layers.get(layer.name) or next(iter(ogr_layers.values()))
        assert len(layer) == ogr_layer.GetFeatureCount()
        defn = ogr_layer.GetLayerDefn()
        ogr_fields = [defn.GetFieldDefn(i).GetName() for i in range(defn.GetFieldCount())]
        assert set(ogr_fields) <= set(layer.fields)
        extent = ogr_layer.GetExtent()  # (min_x, max_x, min_y, max_y)
        bounds = layer.bounds
        expected = (extent[0], extent[2], extent[1], extent[3])
        assert bounds == pytest.approx(expected, rel=1e-9, abs=1e-9)
        # Geometries: compare the first features' areas and lengths through OGR.
        wkbs = layer.to_wkb()
        ogr_layer.ResetReading()
        for i, feature in enumerate(ogr_layer):
            if i >= 50:
                break
            mine, theirs = wkbs[i], feature.GetGeometryRef()
            if mine is None or theirs is None:
                assert mine is None and theirs is None
                continue
            g = ogr.CreateGeometryFromWkb(mine)
            assert g.GetArea() == pytest.approx(theirs.GetArea(), rel=1e-9, abs=1e-12)
            assert g.Length() == pytest.approx(theirs.Length(), rel=1e-9, abs=1e-12)


def test_geojson_matches_ogr_properties(data_dir, ogr):
    path = data_dir / "countries.geojson"
    if not path.exists():
        pytest.skip("countries.geojson is not in the test data")
    fc = terrio.open_vector(str(path))[0].to_geojson()
    src = ogr.Open(str(path))
    layer = src.GetLayer(0)
    for feature, ogr_feature in zip(fc["features"], layer, strict=True):
        for key, value in feature["properties"].items():
            expected = ogr_feature.GetField(key)
            if isinstance(expected, float):
                assert value == pytest.approx(expected)
            else:
                assert value == expected


def test_grid_transformations_match_osr(data_dir, gdal):
    """Datum shift grids and geoid models give GDAL's results when both use the same grids."""
    osr = pytest.importorskip("osgeo.osr")
    grids = data_dir / "grids"
    if not (grids / "us_noaa_conus.tif").exists():
        pytest.skip("the grids are missing: run scripts/fetch_test_data.sh")
    paths = osr.GetPROJSearchPaths()
    proj_db = next((p for p in paths if os.path.exists(os.path.join(p, "proj.db"))), None)
    if proj_db is None:
        pytest.skip("PROJ's database is not found")
    # GDAL uses the same grids as terrio, and only them (conda turns PROJ's network access on).
    network = osr.GetPROJEnableNetwork()
    osr.SetPROJSearchPaths([str(grids), proj_db])
    osr.SetPROJEnableNetwork(False)
    terrio.set_grid_search_paths([grids])
    terrio.set_network_enabled(False)
    try:
        rng = np.random.default_rng(1)
        cases = [
            ("EPSG:4267", "EPSG:4269", (-124, 25, -67, 49), 0.0),
            ("EPSG:4314", "EPSG:4258", (6, 47.5, 15, 55), 0.0),
            ("EPSG:4979", "EPSG:4326+5773", (-180, -80, 180, 80), 100.0),
            ("EPSG:6318+5703", "EPSG:6319", (-124, 25, -67, 49), 100.0),
        ]
        for src_crs, dst_crs, (w, s, e, n), h in cases:
            lon, lat = rng.uniform(w, e, 50), rng.uniform(s, n, 50)
            z = np.full(50, h)
            xs, ys, zs = terrio.transform_points(lon, lat, src_crs, dst_crs, z)
            src, dst = osr.SpatialReference(), osr.SpatialReference()
            src.SetFromUserInput(src_crs)
            dst.SetFromUserInput(dst_crs)
            for sr in (src, dst):
                sr.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            t = osr.CoordinateTransformation(src, dst)
            expected = np.array([t.TransformPoint(*p) for p in zip(lon, lat, z, strict=True)])
            np.testing.assert_allclose(
                xs, expected[:, 0], atol=1e-9, err_msg=f"{src_crs} to {dst_crs}"
            )
            np.testing.assert_allclose(
                ys, expected[:, 1], atol=1e-9, err_msg=f"{src_crs} to {dst_crs}"
            )
            np.testing.assert_allclose(
                zs, expected[:, 2], atol=1e-6, err_msg=f"{src_crs} to {dst_crs}"
            )
    finally:
        osr.SetPROJSearchPaths(paths)
        osr.SetPROJEnableNetwork(network)
        terrio.set_grid_search_paths(None)
        terrio.set_network_enabled(None)


def test_transform_points_match_osr(gdal):
    osr = pytest.importorskip("osgeo.osr")
    rng = np.random.default_rng(0)
    # Within UTM zone 11, where both projection engines agree to the millimetre.
    lon = rng.uniform(-120, -114, 200)
    lat = rng.uniform(32, 42, 200)
    xs, ys = terrio.transform_points(lon, lat, "EPSG:4326", "EPSG:32611")
    src, dst = osr.SpatialReference(), osr.SpatialReference()
    src.ImportFromEPSG(4326)
    dst.ImportFromEPSG(32611)
    for s in (src, dst):
        s.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    t = osr.CoordinateTransformation(src, dst)
    expected = np.array(t.TransformPoints(np.column_stack([lon, lat])))
    np.testing.assert_allclose(xs, expected[:, 0], atol=1e-3)
    np.testing.assert_allclose(ys, expected[:, 1], atol=1e-3)


def test_rasterize_matches_gdal(data_dir, gdal, tmp_path):
    path = data_dir / "TN_Counties.geojson"
    if not path.exists():
        pytest.skip("TN_Counties.geojson is not in the test data")
    mine = terrio.rasterize(str(path), burn=1, tr=(0.01, 0.01), ot="Byte").read(1)
    out = tmp_path / "gdal.tif"
    ds = gdal.Rasterize(
        str(out), str(path), burnValues=[1], xRes=0.01, yRes=0.01, outputType=gdal.GDT_Byte
    )
    assert_same_pixels(mine, ds.GetRasterBand(1).ReadAsArray())


@pytest.mark.skipif(
    not os.environ.get("TERRIO_TEST_REMOTE"), reason="TERRIO_TEST_REMOTE is not set"
)
def test_remote_cog_window_matches_gdal(gdal):
    r = terrio.open_raster(REMOTE_COG, lazy=True)
    assert r.is_lazy and r.overviews
    window = (2000, 3000, 256, 200)
    gdal.SetConfigOption("GDAL_DISABLE_READDIR_ON_OPEN", "EMPTY_DIR")
    expected = gdal_array(gdal, f"/vsicurl/{REMOTE_COG}", window)
    assert_same_pixels(r.read(window=window), expected)
    expected = gdal_array(gdal, f"/vsicurl/{REMOTE_COG}", (0, 0, 64, 64), overview=1)
    assert_same_pixels(r.read(window=(0, 0, 64, 64), overview=1), expected)


def test_translate_matches_gdal(data_dir, gdal, tmp_path):
    path = raster_path(data_dir, "landsat.tif")
    mine = terrio.translate(path, srcwin=(10, 20, 100, 80), bands=[3, 1], ot="Float32")
    out = tmp_path / "t.tif"
    ds = gdal.Translate(
        str(out), path, srcWin=[10, 20, 100, 80], bandList=[3, 1], outputType=gdal.GDT_Float32
    )
    expected = np.stack([ds.GetRasterBand(i).ReadAsArray() for i in (1, 2)])
    assert_same_pixels(mine.read(), expected)
    assert mine.transform == pytest.approx(ds.GetGeoTransform())


def test_warp_matches_gdal(data_dir, gdal, tmp_path):
    path = raster_path(data_dir, "srtm_sub.tif")
    mine = terrio.warp(path, t_srs="EPSG:3857", resampling="near")
    ds = gdal.Warp(str(tmp_path / "w.tif"), path, dstSRS="EPSG:3857", resampleAlg="near")
    assert (mine.width, mine.height) == (ds.RasterXSize, ds.RasterYSize)
    assert mine.transform == pytest.approx(ds.GetGeoTransform(), rel=1e-9)
    a, b = mine.read(1), ds.GetRasterBand(1).ReadAsArray()
    assert a.dtype == b.dtype
    assert np.mean(a == b) > 0.999


def test_mosaic_round_trip(data_dir, tmp_path):
    path = raster_path(data_dir, "srtm_sub.tif")
    r = terrio.open_raster(path)
    half = r.width // 2
    parts = [
        terrio.translate(r, srcwin=(0, 0, half, r.height)),
        terrio.translate(r, srcwin=(half, 0, r.width - half, r.height)),
    ]
    assert_same_pixels(terrio.mosaic(parts).read(), r.read())


def assert_dem_pixels(gdal, actual, expected):
    """Asserts that terrio's DEM product matches GDAL's.

    terrio reproduces GDAL 3.12's arithmetic exactly. GDAL 3.13 computes in single precision,
    which can move values by one rounding step.

    Args:
        gdal: GDAL's Python bindings.
        actual: terrio's array.
        expected: GDAL's array.
    """
    if gdal.__version__.startswith("3.12."):
        assert_same_pixels(actual, expected)
        return
    assert actual.shape == expected.shape and actual.dtype == expected.dtype
    tol = 1 if actual.dtype == np.uint8 else 1e-3
    diff = np.abs(actual.astype(np.float64) - expected.astype(np.float64))
    assert np.mean(diff > tol) < 1e-4


def test_dem_matches_gdaldem(data_dir, gdal, tmp_path):
    """Compares hillshade, slope, aspect and color relief with gdal.DEMProcessing."""
    path = raster_path(data_dir, "las_vegas_dem.tif")
    for mode, mine, opts in [
        ("hillshade", {}, {}),
        (
            "hillshade",
            {"multidirectional": True, "compute_edges": True},
            {"multiDirectional": True, "computeEdges": True},
        ),
        ("slope", {"percent": True}, {"slopeFormat": "percent"}),
        ("aspect", {"alg": "ZevenbergenThorne"}, {"alg": "ZevenbergenThorne"}),
        ("TRI", {"alg": "Wilson"}, {"alg": "Wilson"}),
        ("roughness", {}, {}),
    ]:
        out = tmp_path / f"{mode}.tif"
        gdal.DEMProcessing(str(out), path, mode, **opts)
        expected = gdal.Open(str(out)).ReadAsArray()
        assert_dem_pixels(gdal, terrio.dem(path, mode, **mine).read(1), expected)
    colors = tmp_path / "colors.txt"
    colors.write_text("nv 0 0 0 0\n0% blue\n50% white\n100% red\n")
    gdal.DEMProcessing(
        str(tmp_path / "c.tif"), path, "color-relief", colorFilename=str(colors), addAlpha=True
    )
    expected = gdal.Open(str(tmp_path / "c.tif")).ReadAsArray()
    assert_same_pixels(
        terrio.dem(path, "color-relief", color_file=str(colors), alpha=True).read(), expected
    )


def test_calc_matches_gdal_calc(data_dir, gdal, tmp_path):
    """Compares band math with gdal_calc.py's Calc on Landsat bands."""
    gdal_calc = pytest.importorskip("osgeo_utils.gdal_calc")
    path = raster_path(data_dir, "landsat.tif")
    for expr, opts in [
        ("(B.astype(float32) - A) / (B.astype(float32) + A)", {"type": "Float32"}),
        ("A + B", {}),
        ("where(B > A, B - A, 0)", {"nodata": 0}),
        ("sqrt(A) * 10", {}),
    ]:
        out = tmp_path / "calc.tif"
        kwargs = {"NoDataValue": opts["nodata"]} if "nodata" in opts else {}
        if "type" in opts:
            kwargs["type"] = opts["type"]
        gdal_calc.Calc(
            expr,
            A=path,
            A_band=3,
            B=path,
            B_band=4,
            outfile=str(out),
            overwrite=True,
            quiet=True,
            **kwargs,
        )
        ds = gdal.Open(str(out))
        mine = terrio.calc(expr, A=path, A_band=3, B=path, B_band=4, **opts)
        assert_same_pixels(mine.read(1), ds.ReadAsArray())
        assert mine.nodata == ds.GetRasterBand(1).GetNoDataValue()


def test_proximity_and_sieve_match_gdal(data_dir, gdal, tmp_path):
    """Compares proximity and sieve with gdal.ComputeProximity and gdal.SieveFilter."""
    path = raster_path(data_dir, "nlcd_knoxville.tif")
    src = gdal.Open(path)
    drv = gdal.GetDriverByName("MEM")
    prox = drv.Create("", src.RasterXSize, src.RasterYSize, 1, gdal.GDT_Float32)
    gdal.ComputeProximity(
        src.GetRasterBand(1), prox.GetRasterBand(1), ["VALUES=11", "DISTUNITS=GEO"]
    )
    assert_same_pixels(
        terrio.proximity(path, values=[11], distunits="GEO").read(1), prox.ReadAsArray()
    )
    sieved = drv.Create("", src.RasterXSize, src.RasterYSize, 1, gdal.GDT_Byte)
    gdal.SieveFilter(
        src.GetRasterBand(1), src.GetRasterBand(1).GetMaskBand(), sieved.GetRasterBand(1), 20, 8
    )
    assert_same_pixels(
        terrio.sieve(path, threshold=20, connectedness=8).read(1), sieved.ReadAsArray()
    )


def test_netcdf4_written_by_terrio_read_by_gdal(data_dir, gdal, tmp_path):
    """netCDF-4 (chunked, deflate) written by terrio reads in GDAL as the source."""
    path = raster_path(data_dir, "srtm_sub.tif")
    src = terrio.open_raster(path)
    for name, options in [
        ("a.nc", {"FORMAT": "NC4"}),
        ("b.nc", {"FORMAT": "NC4C", "COMPRESS": "DEFLATE", "ZLEVEL": "6"}),
    ]:
        out = tmp_path / name
        src.write(str(out), options=options)
        assert out.read_bytes()[:4] == b"\x89HDF"
        assert_same_pixels(gdal_array(gdal, out), gdal_array(gdal, path))
        ds = gdal.Open(str(out))
        assert ds.GetGeoTransform() == pytest.approx(src.transform)
        assert ds.GetRasterBand(1).GetNoDataValue() == -32768
        assert ds.GetSpatialRef().GetAuthorityCode(None) == "4326"


def test_subdatasets_match_gdal(data_dir, gdal):
    """HDF5 datasets and netCDF-4 group variables, listed and read as GDAL lists and reads them."""
    for name in [
        "SATMS_j01_d20230101_t0000026_e0000343_b26525_c20230101002941202701_oeac_ops.h5",
        "S5P_OFFL_L2__CH4____20230101T000319_20230101T014449_27036_03_020400_20230103T110255.nc",
    ]:
        path = raster_path(data_dir, name)
        expected = gdal.Open(path).GetSubDatasets()
        listed = [(s["name"], s["description"]) for s in terrio.info(path)["subdatasets"]]
        assert listed == [tuple(s) for s in expected]
        sub = expected[1 if name.endswith(".h5") else 5][0]
        assert_same_pixels(terrio.open_raster(sub).read(), gdal_array(gdal, sub))


def test_histograms_match_gdalinfo(data_dir, gdal, tmp_path):
    """`info(hist=True)` gives `gdalinfo -json -hist`'s histograms."""
    for name in ["srtm_sub.tif", "landsat.tif"]:
        copy = tmp_path / name
        copy.write_bytes(open(raster_path(data_dir, name), "rb").read())
        gdal.SetConfigOption("GDAL_PAM_ENABLED", "NO")
        try:
            expected = gdal.Info(str(copy), format="json", reportHistograms=True)
        finally:
            gdal.SetConfigOption("GDAL_PAM_ENABLED", None)
        bands = terrio.info(str(copy), hist=True)["bands"]
        assert [b["histogram"] for b in bands] == [b["histogram"] for b in expected["bands"]]


def test_color_relief_vrt_read_by_gdal(data_dir, gdal, tmp_path):
    """A color relief VRT written by terrio reads in GDAL as `gdaldem`'s does."""
    path = raster_path(data_dir, "las_vegas_dem.tif")
    colors = tmp_path / "colors.txt"
    colors.write_text("nv 0 0 0 0\n600 blue\n650 white\n700 red\n")
    gdal.DEMProcessing(
        str(tmp_path / "g.vrt"),
        path,
        "color-relief",
        colorFilename=str(colors),
        format="VRT",
        addAlpha=True,
    )
    relief = terrio.dem(
        path, "color-relief", str(tmp_path / "t.vrt"), color_file=str(colors), alpha=True
    )
    expected = gdal_array(gdal, tmp_path / "g.vrt")
    assert_same_pixels(gdal_array(gdal, tmp_path / "t.vrt"), expected)
    assert_same_pixels(relief.read(), expected)


def test_complex_hdf5_matches_gdal(data_dir, gdal):
    """Complex (compound) HDF5 datasets read as GDAL's CFloat32 and CFloat64 arrays."""
    path = data_dir / "gdal_hdf5" / "complex.h5"
    if not path.exists():
        pytest.skip("gdal_hdf5/complex.h5 is missing")
    for name, dtype in (("f16", "complex64"), ("f32", "complex64"), ("f64", "complex128")):
        sub = f'HDF5:"{path}"://{name}'
        r = terrio.open_raster(sub)
        assert (r.count, r.dtype) == (1, dtype)
        a = r.read(1)
        assert a.dtype == np.dtype(dtype)
        # The dataset kept while its band is read (a band outliving it is invalid).
        ds = gdal.Open(sub)
        expected = ds.GetRasterBand(1).ReadAsArray()
        np.testing.assert_array_equal(a, expected)
        assert r.read().shape == (1, 5, 5)
        # Band numbers are the complex bands', not their parts'.
        for band in (0, 2):
            with pytest.raises(terrio.InvalidArgumentError, match="the raster has 1 band"):
                r.read(band)


def test_swath_geolocation_matches_gdal(data_dir, gdal):
    """An HDF-EOS5 swath's geolocation arrays, as GDAL's GEOLOCATION metadata."""
    path = data_dir / "gdal_hdf5" / "dummy_HDFEOS_swath.h5"
    if not path.exists():
        pytest.skip("gdal_hdf5/dummy_HDFEOS_swath.h5 is missing")
    sub = f'HDF5:"{path}"://HDFEOS/SWATHS/MySwath/Data_Fields/MyDataField'
    r = terrio.open_raster(sub)
    assert dict(r.geolocation) == gdal.Open(sub).GetMetadata("GEOLOCATION")
    assert terrio.open_raster(str(data_dir / "srtm_sub.tif")).geolocation is None
