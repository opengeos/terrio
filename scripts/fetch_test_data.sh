#!/usr/bin/env bash
# Downloads the real-world test files into a directory (default: ../terrio-data), and derives
# copies in other formats with GDAL when ogr2ogr and gdal_translate are available.
#
# Usage: scripts/fetch_test_data.sh [DIR]
set -euo pipefail
dir="${1:-$(dirname "$0")/../../terrio-data}"
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
[ -e usgs_dem.tif ] || curl -sSfL -o usgs_dem.tif https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif
# Adjacent USGS 3DEP 1 arc-second tiles, for mosaics.
for t in n40w107 n41w106 n41w107; do
  [ -e "USGS_1_$t.tif" ] || curl -sSfL -o "USGS_1_$t.tif" "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/$t/USGS_1_$t.tif"
done
for z in ne_countries ne_places nybb; do [ -d "$z" ] || unzip -q -o "$z.zip" -d "$z"; done

if command -v ogr2ogr >/dev/null && command -v gdal_translate >/dev/null; then
  [ -e countries.gpkg ] || ogr2ogr -f GPKG countries.gpkg countries.geojson -nln countries
  [ -e countries.parquet ] || ogr2ogr -f Parquet countries.parquet countries.geojson
  [ -e countries.kml ] || ogr2ogr -f KML countries.kml countries.geojson
  [ -e countries.gml ] || ogr2ogr -f GML countries.gml countries.geojson
  [ -e countries.fgb ] || ogr2ogr -f FlatGeobuf countries.fgb countries.geojson
  [ -e countries.mif ] || ogr2ogr -f "MapInfo File" countries.mif countries.geojson
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
fi
echo "Test data in $(pwd)"
