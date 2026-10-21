#!/usr/bin/env bash
# Downloads the real-world test files into a directory (default: ../terrio-data), and derives
# copies in other formats with GDAL when ogr2ogr and gdal_translate are available, and point
# clouds of every LAS point format with laspy when it is installed.
#
# Usage: scripts/fetch_test_data.sh [DIR]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dir="${1:-$here/../../terrio-data}"
mkdir -p "$dir"
cd "$dir"
base=https://github.com/opengeos/datasets/releases/download
files=(
  raster/cog.tif raster/dem.tif raster/srtm90.tif raster/landsat.tif raster/landsat7.tif
  raster/OPERA_L3_DSWx_WTR.tif raster/wind_global.nc
  raster/ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif
  vector/countries.geojson vector/nybb.zip vector/cables.geojson vector/hike_gps_trace.csv
  vector/TN_Counties.geojson places/las_vegas_buildings.geojson places/las_vegas_roads.geojson
  places/las_vegas_dem.tif places/las_vegas_bounds.geojson places/nyc_buildings.geojson
)
for f in "${files[@]}"; do
  [ -e "$(basename "$f")" ] || curl -sSfL -o "$(basename "$f")" "$base/$f"
done
[ -e ne_countries.zip ] || curl -sSfL -o ne_countries.zip https://naciscdn.org/naturalearth/110m/cultural/ne_110m_admin_0_countries.zip
[ -e ne_places.zip ] || curl -sSfL -o ne_places.zip https://naciscdn.org/naturalearth/10m/cultural/ne_10m_populated_places_simple.zip
[ -e UScounties.fgb ] || curl -sSfL -o UScounties.fgb https://flatgeobuf.org/test/data/UScounties.fgb
# Vector formats from other producers: GPX 1.0 and 1.1 (gpxpy test files), KML samples (GDAL test
# data), US Census KML, TopoJSON (world-atlas, us-atlas) and an OpenStreetMap extract (Geofabrik).
gpxpy=https://raw.githubusercontent.com/tkrajina/gpxpy/master/test_files
[ -e gpx10_tracks.gpx ] || curl -sSfL -o gpx10_tracks.gpx $gpxpy/korita-zbevnica.gpx
[ -e gpx11_all_fields.gpx ] || curl -sSfL -o gpx11_all_fields.gpx $gpxpy/gpx1.1_with_all_fields.gpx
[ -e kml_samples.kml ] || curl -sSfL -o kml_samples.kml https://raw.githubusercontent.com/OSGeo/gdal/master/autotest/ogr/data/kml/samples.kml
if [ ! -e census_states.kml ]; then
  curl -sSfL -o census_states.zip https://www2.census.gov/geo/tiger/GENZ2018/kml/cb_2018_us_state_20m.zip
  unzip -q -p census_states.zip cb_2018_us_state_20m.kml > census_states.kml && rm census_states.zip
fi
[ -e world-110m.topojson ] || curl -sSfL -o world-110m.topojson https://cdn.jsdelivr.net/npm/world-atlas@2.0.2/countries-110m.json
[ -e us-states-10m.topojson ] || curl -sSfL -o us-states-10m.topojson https://cdn.jsdelivr.net/npm/us-atlas@3.0.1/states-10m.json
[ -e liechtenstein.osm.pbf ] || curl -sSfL -o liechtenstein.osm.pbf https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf
# Vector tiles in a PMTiles archive (ZIP code tabulation areas, by Protomaps, made with Tippecanoe).
[ -e cb_2018_us_zcta510_500k.pmtiles ] || curl -sSfL -o cb_2018_us_zcta510_500k.pmtiles \
  https://r2-public.protomaps.com/protomaps-sample-datasets/cb_2018_us_zcta510_500k.pmtiles
[ -e usgs_dem.tif ] || curl -sSfL -o usgs_dem.tif https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif
# Adjacent USGS 3DEP 1 arc-second tiles, for mosaics.
for t in n40w107 n41w106 n41w107; do
  [ -e "USGS_1_$t.tif" ] || curl -sSfL -o "USGS_1_$t.tif" "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/$t/USGS_1_$t.tif"
done
# HDF5 and netCDF-4 products: JPSS ATMS and VIIRS sensor data records (HDF5 with a user block,
# from NOAA's open data on AWS) and a Sentinel-5P methane product (netCDF-4 with nested groups, from
# the Copernicus mirror on AWS).
n20=https://noaa-nesdis-n20-pds.s3.amazonaws.com
for f in ATMS-SDR/2023/01/01/SATMS_j01_d20230101_t0000026_e0000343_b26525_c20230101002941202701_oeac_ops.h5 \
  ATMS-SDR-GEO/2023/01/01/GATMO_j01_d20230101_t0000026_e0000343_b26525_c20230101002941320788_oeac_ops.h5 \
  VIIRS-I1-SDR/2022/06/29/SVI01_j01_d20220629_t1513542_e1515187_b23896_c20220629153147215027_oeac_pop.h5; do
  [ -e "$(basename "$f")" ] || curl -sSfL -o "$(basename "$f")" "$n20/$f"
done
s5p=S5P_OFFL_L2__CH4____20230101T000319_20230101T014449_27036_03_020400_20230103T110255.nc
[ -e $s5p ] || curl -sSfL -o $s5p "https://meeo-s5p.s3.amazonaws.com/OFFL/L2__CH4___/2023/01/01/$s5p"
# HDF-EOS5 grids (geographic, sinusoidal and UTM) and swaths, complex (compound) datasets,
# COSMO-SkyMed products and netCDF geolocation arrays (CF coordinates, NASA ocean color, EMIT,
# irregular axes), from GDAL's test suite.
gdal_data=https://raw.githubusercontent.com/OSGeo/gdal/v3.12.2/autotest/gdrivers/data
mkdir -p gdal_hdf5 gdal_netcdf
for f in dummy_HDFEOS_swath.h5 dummy_HDFEOS_with_sinu_projection.h5 dummy_HDFEOS_with_utm_projection.h5 \
  dummy_HDFEOS_with_geo_projection.h5 hdfeos_sample_swath.h5 complex.h5 CSK_GEC.h5 CSK_DGM.h5 recursive_groups.h5; do
  [ -e "gdal_hdf5/$f" ] || curl -sSfL -o "gdal_hdf5/$f" "$gdal_data/hdf5/$f"
done
for f in GLMELT_4X5.OCN.nc fake_SNPP_VIIRS.20230406T024200.L2.OC.NRT.nc cf_lon_lat_with_coordinates_no_crs.nc \
  fake_EMIT_L2B_MIN.nc sentinel5p_fake.nc uint16_netcdf4_without_fill.nc; do
  [ -e "gdal_netcdf/$f" ] || curl -sSfL -o "gdal_netcdf/$f" "$gdal_data/netcdf/$f"
done
# NASA HDF-EOS5 products, when Earthdata Login credentials are in ~/.netrc (not in CI): a VIIRS
# Black Marble grid (geographic, LAADS), a VIIRS surface reflectance grid (sinusoidal, LP DAAC),
# AMSR2 sea ice polar stereographic grids and a precipitation swath (NSIDC).
if grep -qs urs.earthdata.nasa.gov ~/.netrc; then
  mkdir -p hdfeos5
  edl() { [ -e "hdfeos5/$1" ] || curl -sSfL -n -c hdfeos5/.cookies -b hdfeos5/.cookies -o "hdfeos5/$1" "$2" || rm -f "hdfeos5/$1"; }
  edl VNP46A2.A2022365.h11v09.002.2025135012224.h5 \
    https://data.laadsdaac.earthdatacloud.nasa.gov/prod-lads/VNP46A2/VNP46A2.A2022365.h11v09.002.2025135012224.h5
  edl VNP09GA.A2026284.h01v11.002.2026286234935.h5 \
    https://data.lpdaac.earthdatacloud.nasa.gov/lp-prod-protected/VNP09GA.002/VNP09GA.A2026284.h01v11.002.2026286234935/VNP09GA.A2026284.h01v11.002.2026286234935.h5
  edl AMSR_U2_L3_SeaIce12km_B04_20250831.he5 \
    https://data.nsidc.earthdatacloud.nasa.gov/nsidc-cumulus-prod-protected/AMSA/AU_SI12/1/2025/08/31/AMSR_U2_L3_SeaIce12km_B04_20250831.he5
  edl AMSR_U2_L2_Rain_V02_202508312319_A.he5 \
    https://data.nsidc.earthdatacloud.nasa.gov/nsidc-cumulus-prod-protected/AMSA/AU_Rain/1/2025/08/31/AMSR_U2_L2_Rain_V02_202508312319_A.he5
  rm -f hdfeos5/.cookies
fi
# JPEG 2000: Sentinel-2 band files, georeferenced by GMLJP2 boxes (L1C and L2A, tile 10SEG, from
# the public Google Cloud copy of the Copernicus archive), and GDAL's test files (GMLJP2 in EPSG
# axis order, a 1-bit alpha channel, a GeoJP2 box written by the ECW SDK).
s2=https://storage.googleapis.com/gcp-public-data-sentinel-2
l1c=$s2/tiles/10/S/EG/S2B_MSIL1C_20240630T185919_N0510_R013_T10SEG_20240630T211017.SAFE/GRANULE/L1C_T10SEG_A038220_20240630T190812/IMG_DATA
l2a=$s2/L2/tiles/10/S/EG/S2B_MSIL2A_20240630T185919_N0510_R013_T10SEG_20240630T215047.SAFE/GRANULE/L2A_T10SEG_A038220_20240630T190812/IMG_DATA
[ -e s2_l1c_B01.jp2 ] || curl -sSfL -o s2_l1c_B01.jp2 "$l1c/T10SEG_20240630T185919_B01.jp2"
[ -e s2_l2a_B01_60m.jp2 ] || curl -sSfL -o s2_l2a_B01_60m.jp2 "$l2a/R60m/T10SEG_20240630T185919_B01_60m.jp2"
[ -e s2_l2a_TCI_60m.jp2 ] || curl -sSfL -o s2_l2a_TCI_60m.jp2 "$l2a/R60m/T10SEG_20240630T185919_TCI_60m.jp2"
[ -e s2_l2a_SCL_20m.jp2 ] || curl -sSfL -o s2_l2a_SCL_20m.jp2 "$l2a/R20m/T10SEG_20240630T185919_SCL_20m.jp2"
# A full-resolution band (10980 x 10980, 26 MB), read by window and resolution level.
[ -e s2_l1c_B04.jp2 ] || curl -sSfL -o s2_l1c_B04.jp2 "$l1c/T10SEG_20240630T185919_B04.jp2"
gdal_jp2=https://raw.githubusercontent.com/OSGeo/gdal/v3.12.2/autotest/gdrivers/data/jpeg2000
for f in gmljp2_dtedsm_epsg_4326_axes.jp2 stefan_full_rgba_alpha_1bit.jp2 3_13bit_and_1bit.jp2; do
  [ -e "$f" ] || curl -sSfL -o "$f" "$gdal_jp2/$f"
done
for z in ne_countries ne_places nybb; do [ -d "$z" ] || unzip -q -o "$z.zip" -d "$z"; done
# OSM XML, when osmium is installed.
if [ ! -e liechtenstein.osm ] && command -v osmium >/dev/null; then
  osmium cat liechtenstein.osm.pbf -o liechtenstein.osm
fi
# Point clouds (LAS 1.0-1.4, LAZ, COPC) from the PDAL and laspy test suites, and a COPC file of
# USGS 3DEP lidar over Madison, Wisconsin (23 MB).
mkdir -p pointcloud
pdal=https://raw.githubusercontent.com/PDAL/PDAL/2.10.2/test/data
laspy=https://raw.githubusercontent.com/laspy/laspy/2.7.0/tests/data
pc_files=(
  "$pdal/las/permutations/1.0_0.las" "$pdal/las/permutations/1.0_1.las" "$pdal/las/permutations/1.1_0.las"
  "$pdal/las/permutations/1.1_1.las" "$pdal/las/permutations/1.2_0.las" "$pdal/las/permutations/1.2_1.las"
  "$pdal/las/permutations/1.2_2.las" "$pdal/las/permutations/1.2_3.las" "$pdal/las/1.2-with-color.las"
  "$pdal/las/extrabytes.las" "$pdal/las/test_utm16.las" "$pdal/las/mvk-thin.las" "$pdal/las/test1_4.las"
  "$pdal/las/autzen_trim.las" "$pdal/las/no-points.las" "$pdal/las/epsg_4326.las" "$pdal/las/4_6_crop.las"
  "$pdal/laz/simple-laszip-compressor-version-1.2r0.laz" "$pdal/laz/las_with_several_extra_byte_bloc.laz"
  "$laspy/1_4_w_evlr.las" "$laspy/1_4_w_evlr.laz" "$laspy/simple.copc.laz" "$laspy/simple_with_page.copc.laz"
  "$laspy/simple.laz" "$laspy/simple1_3.las" "$laspy/simple1_4.las" "$laspy/extra.laz"
  "$laspy/plane.laz" "$laspy/vegetation_1_3.las" "$laspy/autzen_trim.laz" "$laspy/unregistered_extra_bytes.las"
  "$laspy/file_with_both_wkt_and_geotiff_vlrs.las"
  "$base/lidar/madison.copc.laz"
)
for u in "${pc_files[@]}"; do
  [ -e "pointcloud/$(basename "$u")" ] || curl -sSfL -o "pointcloud/$(basename "$u")" "$u"
done
# Every point format (0 to 10) as LAS and LAZ, written by laspy, when laspy and lazrs are installed.
if [ ! -e pointcloud/synthetic/format10.laz ] && python3 -c "import laspy, lazrs" 2>/dev/null; then
  python3 "$here/make_pointcloud_samples.py" pointcloud/synthetic
fi
# EPT datasets (LASzip, binary and Zstandard data, hierarchy pages), PLY and E57 files from the
# PDAL test suite, E57 files from the e57 crate's, and an airborne survey with full waveforms
# (LAS 1.3, point format 4, internal waveform packets; NERC-ARF's tutorial data, 140 MB).
ept_files=(
  1.2-with-color/ept.json 1.2-with-color/ept-data/0-0-0-0.laz 1.2-with-color/ept-hierarchy/0-0-0-0.json
  ellipsoid-binary/ept.json ellipsoid-binary/ept-data/0-0-0-0.bin ellipsoid-binary/ept-hierarchy/0-0-0-0.json
  ellipsoid-zstandard/ept.json ellipsoid-zstandard/ept-data/0-0-0-0.zst ellipsoid-zstandard/ept-hierarchy/0-0-0-0.json
  lone-star-laszip/ept.json lone-star-laszip/ept-hierarchy/0-0-0-0.json
)
for k in 0-0-0-0 1-0-0-0 1-0-1-0 1-1-0-0 1-1-1-0 1-1-1-1 2-2-2-1 2-2-2-2 2-2-3-1 2-3-2-1 2-3-3-1 3-5-4-3 3-5-4-4; do
  ept_files+=("lone-star-laszip/ept-data/$k.laz")
done
for k in 2-2-2-1 2-2-2-2 2-2-3-1 2-3-2-1 2-3-3-1; do
  ept_files+=("lone-star-laszip/ept-hierarchy/$k.json")
done
for f in "${ept_files[@]}"; do
  mkdir -p "pointcloud/ept/$(dirname "$f")"
  [ -e "pointcloud/ept/$f" ] || curl -sSfL -o "pointcloud/ept/$f" "$pdal/ept/$f"
done
mkdir -p pointcloud/ply pointcloud/e57 pointcloud/waveform
for f in simple_text.ply simple_binary.ply sized_dims.ply unsized_dims.ply text_extradim.ply mesh.ply issue_2421.ply; do
  [ -e "pointcloud/ply/$f" ] || curl -sSfL -o "pointcloud/ply/$f" "$pdal/ply/$f"
done
for f in A4.e57 A4_moved.e57 A_B.e57 A_B_different_dims.e57 B2.e57; do
  [ -e "pointcloud/e57/$f" ] || curl -sSfL -o "pointcloud/e57/$f" "$pdal/e57/$f"
done
e57rs=https://raw.githubusercontent.com/cry-inc/e57/0.11.13/testdata
for f in bunnyDouble.e57 bunnyFloat.e57 bunnyInt19.e57 bunnyInt32.e57 tinyCartesianFloatRgb.e57 tiny_spherical.e57 \
  integer_intensity.e57 scaled_integer_intensity.e57; do
  [ -e "pointcloud/e57/$f" ] || curl -sSfL -o "pointcloud/e57/$f" "$e57rs/$f"
done
[ -e pointcloud/waveform/full_waveform_tutorial_data.las ] || curl -sSfL -o pointcloud/waveform/full_waveform_tutorial_data.las \
  https://nerc-arf-dan.pml.ac.uk/files/full_waveform_tutorial_data.LAS

if command -v ogr2ogr >/dev/null && command -v gdal_translate >/dev/null; then
  [ -e countries.gpkg ] || ogr2ogr -f GPKG countries.gpkg countries.geojson -nln countries
  [ -e countries.parquet ] || ogr2ogr -f Parquet countries.parquet countries.geojson
  [ -e countries.kml ] || ogr2ogr -f KML countries.kml countries.geojson
  [ -e countries.gml ] || ogr2ogr -f GML countries.gml countries.geojson
  [ -e countries.fgb ] || ogr2ogr -f FlatGeobuf countries.fgb countries.geojson
  [ -e countries.mif ] || ogr2ogr -f "MapInfo File" countries.mif countries.geojson
  [ -e countries.tab ] || ogr2ogr -f "MapInfo File" countries.tab countries.geojson
  [ -e roads_utm.tab ] || ogr2ogr -f "MapInfo File" -t_srs EPSG:32611 roads_utm.tab las_vegas_roads.geojson
  [ -e ne_places.kmz ] || ogr2ogr -f LIBKML ne_places.kmz ne_places/ne_10m_populated_places_simple.shp
  [ -e ne_places.parquet ] || ogr2ogr -f Parquet ne_places.parquet ne_places/ne_10m_populated_places_simple.shp
  [ -e hike.gpx ] || ogr2ogr -f GPX hike.gpx hike_gps_trace.csv -oo X_POSSIBLE_NAMES=longitude \
    -oo Y_POSSIBLE_NAMES=latitude -a_srs EPSG:4326 -nlt POINT -nln waypoints \
    -sql "SELECT annotation AS name FROM hike_gps_trace"
  if [ ! -e multi.gpkg ]; then
    ogr2ogr -f GPKG multi.gpkg las_vegas_buildings.geojson -nln buildings
    ogr2ogr -update -f GPKG multi.gpkg las_vegas_roads.geojson -nln roads
  fi
  [ -e UScounties.gpkg ] || ogr2ogr -f GPKG UScounties.gpkg UScounties.fgb -nln UScounties
  # Tile archives written by GDAL: raster tiles (with overviews) and vector tiles.
  if [ ! -e landsat.mbtiles ]; then
    gdal_translate -q -of MBTiles landsat.tif landsat.mbtiles && gdaladdo -q -r bilinear landsat.mbtiles
  fi
  [ -e countries_mvt.mbtiles ] || ogr2ogr -f MBTiles countries_mvt.mbtiles countries.geojson -dsco MAXZOOM=3
  [ -e countries_mvt.pmtiles ] || ogr2ogr -f PMTiles countries_mvt.pmtiles countries.geojson -dsco MAXZOOM=3
  # GeoParquet in row groups of 1000 rows, read by row group.
  [ -e ne_places_rg.parquet ] || ogr2ogr -f Parquet -lco ROW_GROUP_SIZE=1000 ne_places_rg.parquet \
    ne_places/ne_10m_populated_places_simple.shp
  # A FlatGeobuf larger than 100 MB (the US counties nine times, 127 MB), which overflowed 32-bit
  # WebAssembly in the format library's reader.
  if [ ! -e big_counties.fgb ]; then
    rm -f big_counties.gpkg
    for _ in 1 2 3 4 5 6 7 8 9; do ogr2ogr -f GPKG -append -nln big_counties big_counties.gpkg UScounties.fgb; done
    ogr2ogr -f FlatGeobuf big_counties.fgb big_counties.gpkg && rm big_counties.gpkg
  fi
  # A land cover subset (NLCD 2021, 90 m) around Knoxville, read in place from a COG.
  [ -e nlcd_knoxville.tif ] || gdal_translate -q -projwin_srs EPSG:4326 -projwin -84.4 36.25 -83.5 35.75 \
    -co COMPRESS=DEFLATE "/vsicurl/$base/raster/nlcd_2021_land_cover_90m.tif" nlcd_knoxville.tif
  [ -e dem.gpkg ] || gdal_translate -q -ot Float32 -of GPKG dem.tif dem.gpkg
  [ -e lv_dem.asc ] || gdal_translate -q -of AAIGrid las_vegas_dem.tif lv_dem.asc
  [ -e lv_dem.jp2 ] || gdal_translate -q -ot Int16 -of JP2OpenJPEG las_vegas_dem.tif lv_dem.jp2
  [ -e lv_dem.zarr ] || gdal_translate -q -of Zarr las_vegas_dem.tif lv_dem.zarr
  [ -e lv_dem.img ] || gdal_translate -q -of HFA las_vegas_dem.tif lv_dem.img
  [ -e lv_dem.envi ] || gdal_translate -q -of ENVI las_vegas_dem.tif lv_dem.envi
  # An Int16 SRTM subset with a nodata value, in the formats terrio reads with its own drivers.
  if [ ! -e srtm_sub.tif ]; then
    gdal_translate -q -srcwin 1000 1000 500 400 -a_nodata -32768 -co COMPRESS=DEFLATE srtm90.tif srtm_sub.tif
    gdal_translate -q -of EHdr srtm_sub.tif srtm_sub.bil
    gdal_translate -q -of SAGA srtm_sub.tif srtm_sub.sdat
    gdal_translate -q -of ERS srtm_sub.tif srtm_sub.ers
    gdal_translate -q -of RST srtm_sub.tif srtm_sub.rst
    gdal_translate -q -of netCDF srtm_sub.tif srtm_sub.nc
    gdal_translate -q -ot Float32 -of GSBG srtm_sub.tif srtm_sub.grd
    gdal_translate -q -of JP2OpenJPEG -co REVERSIBLE=YES -co QUALITY=100 srtm_sub.tif srtm_sub.jp2
    gdal_translate -q -of Zarr -co FORMAT=ZARR_V3 -co COMPRESS=ZSTD srtm_sub.tif srtm_sub_v3.zarr
    gdal_translate -q -of Zarr -co COMPRESS=BLOSC -co BLOSC_CNAME=lz4 srtm_sub.tif srtm_sub_blosc.zarr
  fi
  # GeoTIFFs in the compressions GDAL writes that the tiff crate does not read: JPEG (YCbCr and
  # RGB), WebP, JPEG XL (lossless and lossy), LERC (with Deflate or Zstandard, lossless and lossy,
  # with a NaN mask) and LZMA (with predictors, planar).
  if [ ! -e gtiff_codecs/f32_lzma.tif ]; then
    mkdir -p gtiff_codecs
    t="gdal_translate -q"
    $t -srcwin 500 500 700 600 landsat7.tif gtiff_codecs/rgb.tif
    $t -ot Float32 -srcwin 1000 600 700 500 dem.tif gtiff_codecs/f32.tif
    $t -ot UInt16 -scale -100 4500 0 4600 srtm_sub.tif gtiff_codecs/u16.tif
    $t -co COMPRESS=JPEG -co PHOTOMETRIC=YCBCR -co TILED=YES gtiff_codecs/rgb.tif gtiff_codecs/rgb_jpeg_ycbcr.tif
    $t -co COMPRESS=JPEG gtiff_codecs/rgb.tif gtiff_codecs/rgb_jpeg.tif
    $t -co COMPRESS=WEBP gtiff_codecs/rgb.tif gtiff_codecs/rgb_webp.tif
    $t -co COMPRESS=JXL gtiff_codecs/rgb.tif gtiff_codecs/rgb_jxl.tif
    $t -co COMPRESS=JXL -co JXL_LOSSLESS=NO gtiff_codecs/rgb.tif gtiff_codecs/rgb_jxl_lossy.tif
    $t -co COMPRESS=LERC gtiff_codecs/rgb.tif gtiff_codecs/rgb_lerc.tif
    $t -co COMPRESS=LZMA -co PREDICTOR=2 -co INTERLEAVE=BAND gtiff_codecs/rgb.tif gtiff_codecs/rgb_lzma.tif
    $t -co COMPRESS=LERC_DEFLATE srtm_sub.tif gtiff_codecs/i16_lerc_deflate.tif
    $t -co COMPRESS=LERC_ZSTD -co TILED=YES srtm_sub.tif gtiff_codecs/i16_lerc_zstd.tif
    $t -co COMPRESS=JXL gtiff_codecs/u16.tif gtiff_codecs/u16_jxl.tif
    $t -co COMPRESS=LERC gtiff_codecs/f32.tif gtiff_codecs/f32_lerc.tif
    $t -of COG -co COMPRESS=LERC -co MAX_Z_ERROR=0.01 gtiff_codecs/f32.tif gtiff_codecs/f32_lerc_lossy_cog.tif
    $t -co COMPRESS=JXL gtiff_codecs/f32.tif gtiff_codecs/f32_jxl.tif
    $t -co COMPRESS=LZMA -co PREDICTOR=3 gtiff_codecs/f32.tif gtiff_codecs/f32_lzma.tif
  fi
  # Tiled JPEG XL COGs with overviews, read by window.
  [ -e gtiff_codecs/rgb_jxl_cog.tif ] || gdal_translate -q -of COG -co COMPRESS=JXL -co BLOCKSIZE=256 \
    landsat7.tif gtiff_codecs/rgb_jxl_cog.tif
  [ -e gtiff_codecs/u16_jxl_cog.tif ] || gdal_translate -q -ot UInt16 -of COG -co COMPRESS=JXL \
    -co BLOCKSIZE=256 -srcwin 1500 5500 2048 2048 s2_l1c_B04.jp2 gtiff_codecs/u16_jxl_cog.tif
fi
# The outputs of GDAL's tools that the WebAssembly build's operations are checked against
# (js/tests/node.test.mjs), for the cases where terrio's native output is identical to GDAL's.
have_tools() { for t in "$@"; do command -v "$t" >/dev/null || return 1; done; }
if have_tools gdal_translate gdalwarp gdaldem gdal_calc.py gdal_merge.py gdal_proximity.py gdal_sieve.py \
  gdal_rasterize gdal_polygonize.py && [ ! -e wasm_refs/done ]; then
  mkdir -p wasm_refs
  r=wasm_refs
  gdal_translate -q -srcwin 100 200 500 400 usgs_dem.tif $r/translate_srcwin.tif
  gdal_translate -q -outsize 25% 25% las_vegas_dem.tif $r/translate_outsize.tif
  gdal_translate -q -b 3 -b 1 -ot Int16 landsat.tif $r/translate_bands.tif
  gdal_translate -q -ot Byte -scale 1000 4000 0 255 usgs_dem.tif $r/translate_scale.tif
  gdalwarp -q -t_srs EPSG:4326 -srcnodata 0 -dstnodata -1 las_vegas_dem.tif $r/warp_4326.tif
  gdalwarp -q -t_srs EPSG:3857 usgs_dem.tif $r/warp_3857.tif
  printf '%s' '{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
    "coordinates":[[[-115.2075,36.1222],[-115.2005,36.1220],[-115.2010,36.1195],[-115.2070,36.1197],[-115.2075,36.1222]]]}}]}' \
    > $r/cutline.geojson
  gdalwarp -q -cutline $r/cutline.geojson -crop_to_cutline -dstnodata -9999 las_vegas_dem.tif $r/warp_cutline.tif
  gdal_translate -q -srcwin 0 0 300 250 srtm_sub.tif $r/part1.tif
  gdal_translate -q -srcwin 200 150 300 250 srtm_sub.tif $r/part2.tif
  gdal_merge.py -q -n -32768 -init -32768 -a_nodata -32768 -o $r/mosaic.tif $r/part2.tif $r/part1.tif
  for m in hillshade slope aspect TRI TPI roughness; do gdaldem $m -q srtm_sub.tif $r/dem_$m.tif; done
  gdaldem hillshade -q -multidirectional -compute_edges srtm_sub.tif $r/dem_hillshade_multi.tif
  gdaldem slope -q -p -alg ZevenbergenThorne srtm_sub.tif $r/dem_slope_zt.tif
  printf 'nv 0 0 0 0\n0 46 154 88\n500 251 255 128\n1000 224 108 31\n2000 200 55 55\n3000 255 255 255\n' > $r/colors.txt
  gdaldem color-relief -q -alpha srtm_sub.tif $r/colors.txt $r/dem_color.tif
  gdal_calc.py --quiet -A landsat.tif --A_band 3 -B landsat.tif --B_band 4 \
    --calc "(B.astype(float32) - A) / (B.astype(float32) + A)" --type Float32 --outfile $r/calc_ndvi.tif
  gdal_calc.py --quiet -A srtm_sub.tif --calc "A * 20" --outfile $r/calc_wrap.tif
  gdal_proximity.py -q nlcd_knoxville.tif $r/proximity.tif -values 11
  gdal_sieve.py -q -st 10 nlcd_knoxville.tif $r/sieve.tif
  gdal_rasterize -q -burn 1 -tr 0.1 0.1 -ot Byte countries.geojson $r/rasterize_burn.tif
  gdal_rasterize -q -a POPULATION -tr 0.002 0.002 -ot Int32 TN_Counties.geojson $r/rasterize_attribute.tif
  gdal_polygonize.py -q nlcd_knoxville.tif -f FlatGeobuf $r/polygonize.fgb
  # Subdatasets of HDF5 and netCDF-4 products, read from memory.
  atms=SATMS_j01_d20230101_t0000026_e0000343_b26525_c20230101002941202701_oeac_ops.h5
  [ ! -e "$atms" ] || gdal_translate -q "HDF5:\"$atms\"://All_Data/ATMS-SDR_All/BrightnessTemperature" $r/h5_bt.tif
  [ ! -e "$s5p" ] || gdal_translate -q "NETCDF:\"$s5p\":/PRODUCT/methane_mixing_ratio" $r/nc4_methane.tif
  touch $r/done
fi
# Datum shift and geoid grids from the PROJ CDN (scripts/proj_grids.txt, 230 MB), and grids in
# the formats PROJ used before PROJ 7 (NTv2, GTX), from proj-datumgrid 1.8.
mkdir -p grids grids_legacy
grep -v '^#' "$here/proj_grids.txt" | while read -r g; do
  if [ -n "$g" ] && [ ! -e "grids/$g" ]; then curl -sSfL -o "grids/$g" "https://cdn.proj.org/$g"; fi
done
if [ ! -e grids_legacy/egm96_15.gtx ]; then
  curl -sSfL -o grids_legacy/proj-datumgrid.zip https://download.osgeo.org/proj/proj-datumgrid-1.8.zip
  unzip -q -o grids_legacy/proj-datumgrid.zip BETA2007.gsb ntf_r93.gsb nzgd2kgrid0005.gsb egm96_15.gtx -d grids_legacy
  rm grids_legacy/proj-datumgrid.zip
fi
# GDAL's transformations for every EPSG CRS, which terrio's are checked against (needs pyproj,
# NumPy and GDAL's Python bindings): without grids, and with those of grids/. 50 points of each
# kind per CRS: about 950,000 lines and 83 MB per file, and 10 CPU minutes each to compute.
if [ ! -e epsg_points.tsv ] && python -c "import numpy, pyproj, osgeo" 2>/dev/null; then
  python "$here/gen_epsg_points.py" epsg_points.tsv
fi
if [ ! -e epsg_points_grids.tsv ] && python -c "import numpy, pyproj, osgeo" 2>/dev/null; then
  python "$here/gen_epsg_points.py" epsg_points_grids.tsv --grids grids
fi
echo "Test data in $(pwd)"
