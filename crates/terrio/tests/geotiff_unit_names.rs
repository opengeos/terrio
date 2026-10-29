//! User-defined units named by a GeoTIFF's citation (`LUnits = `, `AUnits = `, as GDAL writes
//! them) with a double quote in their name: the CRS's WKT quotes the name as WKT 1 does, the quote
//! doubled, so that the CRS parses, in its unit, and the name reads back whole. GDAL 3.12.1 writes
//! such a citation as given (`LUnits = half "metre"|`) and gives the name without its quotes
//! (`half metre`, rasterio 1.5.0's `crs.linear_units`).

use std::path::Path;

use terrio::crs::{SpatialRef, Transformer};

/// A 1 x 1 GeoTIFF at `path` of GeoKeys `keys`, with their double and ASCII parameters.
fn geotiff(path: &Path, keys: &[u16], doubles: &[f64], ascii: &str) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = tiff::encoder::TiffEncoder::new(file).unwrap();
    let mut image = encoder.new_image::<tiff::encoder::colortype::Gray8>(1, 1).unwrap();
    let tags = image.encoder();
    tags.write_tag(tiff::tags::Tag::Unknown(33550), &[10.0f64, 10.0, 0.0][..]).unwrap();
    tags.write_tag(tiff::tags::Tag::Unknown(33922), &[0.0f64, 0.0, 0.0, 1000.0, 2000.0, 0.0][..]).unwrap();
    tags.write_tag(tiff::tags::Tag::Unknown(34735), keys).unwrap();
    tags.write_tag(tiff::tags::Tag::Unknown(34736), doubles).unwrap();
    tags.write_tag(tiff::tags::Tag::Unknown(34737), ascii).unwrap();
    image.write_data(&[0u8]).unwrap();
}

/// The name of the last `UNIT` node of a WKT 1 (a CRS's own), read as WKT 1 quotes text: a doubled
/// quote is a quote.
fn last_unit_name(wkt: &str) -> String {
    let start = wkt.rfind("UNIT[\"").unwrap() + 6;
    let mut name = String::new();
    let mut chars = wkt[start..].chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            if chars.peek() != Some(&'"') {
                return name;
            }
            chars.next();
        }
        name.push(c);
    }
    panic!("unterminated name in {wkt}")
}

/// The CRS of the GeoTIFF at `path`, read eagerly and lazily (the same), and its WKT.
fn crs_of(path: &Path) -> (SpatialRef, String) {
    let path = path.to_str().unwrap();
    let eager = terrio::open_raster(path).unwrap().crs();
    let lazy = terrio::open_raster_lazy(path).unwrap().crs();
    let wkt = eager.to_wkt().unwrap();
    assert_eq!(lazy.to_wkt().as_deref(), Some(wkt.as_str()));
    (eager, wkt)
}

#[test]
fn unit_names_with_a_quote_are_quoted_in_the_wkt() {
    let dir = tempfile::tempdir().unwrap();
    // UTM zone 33N in a user-defined unit of half a metre named `half "metre"`, on a user-defined
    // geographic CRS on WGS 84: GDAL 3.12.1's GeoKeys and citations for that CRS, as it writes them.
    let keys = [
        1, 1, 0, 15, 1024, 0, 1, 1, 1025, 0, 1, 1, 1026, 34737, 7, 0, 2048, 0, 1, 32767, 2049, 34737, 57, 7, 2050, 0, 1, 6326, 2054, 0, 1,
        9102, 2057, 34736, 1, 2, 2059, 34736, 1, 1, 2061, 34736, 1, 3, 3072, 0, 1, 32767, 3073, 34737, 22, 64, 3074, 0, 1, 16033, 3076, 0,
        1, 32767, 3077, 34736, 1, 0,
    ];
    let ascii = "custom|GCS Name = WGS 84|Ellipsoid = WGS 84|Primem = Greenwich||LUnits = half \"metre\"|";
    let path = dir.path().join("lunits.tif");
    geotiff(&path, &keys, &[0.5, 298.257223563, 6378137.0, 0.0], ascii);
    let (crs, wkt) = crs_of(&path);
    assert!(wkt.contains("UNIT[\"half \"\"metre\"\"\",0.5]"), "{wkt}");
    assert_eq!(last_unit_name(&wkt), "half \"metre\"");
    // It parses, in half metres: the zone's central meridian 500 km east of its false origin.
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (x, y) = Transformer::new(&wgs84, &crs).unwrap().transform(15.0, 0.0).unwrap();
    assert!((x - 1_000_000.0).abs() < 1e-6 && y.abs() < 1e-6, "({x}, {y})");

    // A geographic CRS in a user-defined angular unit of 0.015707963267949 radians named `my
    // "grad"` (GeogAngularUnitsGeoKey 32767 and its size).
    let keys = [
        1, 1, 0, 10, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 32767, 2049, 34737, 76, 0, 2050, 0, 1, 6326, 2054, 0, 1, 32767, 2055, 34736,
        1, 0, 2057, 34736, 1, 2, 2059, 34736, 1, 1, 2061, 34736, 1, 3,
    ];
    let ascii = "GCS Name = custom|Ellipsoid = WGS 84|Primem = Greenwich|AUnits = my \"grad\"||";
    let path = dir.path().join("aunits.tif");
    geotiff(&path, &keys, &[0.015707963267949, 298.257223563, 6378137.0, 0.0], ascii);
    let (crs, wkt) = crs_of(&path);
    assert!(wkt.contains("UNIT[\"my \"\"grad\"\"\",0.015707963267949]"), "{wkt}");
    assert_eq!(last_unit_name(&wkt), "my \"grad\"");
    // In that unit: 90 degrees are 100 of it.
    let (x, y) = Transformer::new(&wgs84, &crs).unwrap().transform(9.0, 45.0).unwrap();
    assert!((x - 10.0).abs() < 1e-9 && (y - 50.0).abs() < 1e-9, "({x}, {y})");
}
