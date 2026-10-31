//! Functional publication regressions, using the GDAL 3.13.3 tile conventions.
use std::collections::BTreeMap;
use terrio::ops::raster_publication::PublicationOptions;
use terrio::ops::raster_tile::{self as tile, Convention, FolderSink, GridOptions, TileOptions};
use terrio::raster::RasterData;
use terrio::{DataType, LazyRaster, RasterConfig, RasterDataset};

#[derive(Default)]
struct Collect(BTreeMap<String, Vec<u8>>);
impl FolderSink for Collect {
    fn put(&mut self, name: &str, data: &[u8]) -> terrio::Result<()> {
        self.0.insert(name.into(), data.into());
        Ok(())
    }
}
fn source() -> LazyRaster {
    let r = terrio::Raster::from_data_native(
        RasterConfig {
            cols: 128,
            rows: 64,
            bands: 1,
            x_min: -180.,
            y_min: -90.,
            cell_size: 2.8125,
            cell_size_y: Some(2.8125),
            data_type: DataType::U8,
            crs: terrio::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
            ..Default::default()
        },
        RasterData::U8(vec![100; 128 * 64]),
    )
    .unwrap();
    LazyRaster::from(RasterDataset::new(r, false))
}
fn options() -> TileOptions {
    TileOptions {
        tiling_scheme: "WorldCRS84Quad".into(),
        grid: GridOptions { tile_size: 64, min_zoom: Some(0), max_zoom: Some(0), ..Default::default() },
        resampling: "nearest".into(),
        publication: PublicationOptions {
            viewers: vec!["all".into()],
            title: "A <map> & title".into(),
            copyright: "Data & contributors".into(),
            kml: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn publication_files_are_incremental_and_stacta_reopens_the_actual_tile_grid() {
    let mut input = source();
    let mut sink = Collect::default();
    let report = tile::generate(&mut input, &options(), &mut sink).unwrap();
    assert_eq!(report.written, 2);
    for name in
        ["0/0/0.png", "0/1/0.png", "0/0/0.kml", "0/1/0.kml", "doc.kml", "stacta.json", "leaflet.html", "openlayers.html", "mapml.mapml"]
    {
        assert!(sink.0.contains_key(name), "{name}");
    }
    let doc: serde_json::Value = serde_json::from_slice(&sink.0["stacta.json"]).unwrap();
    let matrix = &doc["properties"]["tiles:tile_matrix_sets"]["WorldCRS84Quad"]["tileMatrix"][0];
    assert_eq!(matrix["tileWidth"], 64);
    assert_eq!(matrix["matrixWidth"], 2);
    assert!((matrix["scaleDenominator"].as_f64().unwrap() * 0.00028 / 111_319.490_793_273_58 - 2.8125).abs() < 1e-12);
    assert_eq!(doc["asset_templates"]["bands"]["bands"].as_array().unwrap().len(), 2);
    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in &sink.0 {
        let path = root.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let mut published = LazyRaster::open(root.path().join("stacta.json").to_str().unwrap()).unwrap();
    assert_eq!((published.width(), published.height(), published.band_count()), (128, 64, 2));
    assert!(published.geo_transform().iter().zip([-180., 2.8125, 0., 90., 0., -2.8125]).all(|(a, b)| (*a - b).abs() < 1e-12));
    let data = published.read().unwrap().raster.data.to_f64_vec();
    assert!(data[..128 * 64].iter().all(|v| *v == 100.));
    assert!(data[128 * 64..].iter().all(|v| *v == 255.));
    let mapml = String::from_utf8(sink.0["mapml.mapml"].clone()).unwrap();
    assert!(mapml.contains("A &lt;map&gt; &amp; title"));
}

#[test]
fn published_kml_urls_include_the_tile_directory_and_tms_row() {
    let mut input = source();
    let mut options = options();
    options.grid.min_zoom = Some(1);
    options.grid.max_zoom = Some(1);
    options.convention = Convention::Tms;
    options.publication.viewers = vec!["none".into(), "leaflet".into(), "none".into(), "openlayers".into()];
    options.publication.url = Some("https://example.test/tiles/".into());
    let mut sink = Collect::default();
    tile::generate(&mut input, &options, &mut sink).unwrap();
    let kml = String::from_utf8(sink.0["1/0/1.kml"].clone()).unwrap();
    assert!(kml.contains("https://example.test/tiles/1/0/1.png"));
    assert!(kml.contains("<north>90</north>") && kml.contains("<south>0</south>"));
    let leaflet = String::from_utf8(sink.0["leaflet.html"].clone()).unwrap();
    assert!(leaflet.contains("tms:true"));
    let openlayers = String::from_utf8(sink.0["openlayers.html"].clone()).unwrap();
    assert!(openlayers.contains("rows[c[0]][1]-1-c[2]"));
}

#[test]
fn unsupported_publications_fail_before_emitting_any_file() {
    for change in [0, 1, 2] {
        let mut options = options();
        match change {
            0 => {
                options.convention = Convention::Tms;
                options.publication.viewers = vec!["stac".into()];
            }
            1 => options.publication.url = Some("javascript:alert(1)".into()),
            _ => options.publication.viewers = vec!["unknown".into()],
        }
        let mut sink = Collect::default();
        assert!(tile::generate(&mut source(), &options, &mut sink).is_err());
        assert!(sink.0.is_empty());
    }
}

#[test]
fn title_and_attribution_cannot_escape_generated_html_scripts() {
    let mut options = options();
    options.publication.title = "</title><script>injected()</script>".into();
    options.publication.copyright = "</script><script>injected()</script>".into();
    let mut sink = Collect::default();
    tile::generate(&mut source(), &options, &mut sink).unwrap();
    for name in ["leaflet.html", "openlayers.html"] {
        let html = String::from_utf8(sink.0[name].clone()).unwrap();
        assert!(!html.contains("<script>injected()"));
    }
}

#[test]
fn resume_never_replaces_existing_publication_files() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("tiles");
    tile::to_directory(&mut source(), &options(), &output, false).unwrap();
    std::fs::write(output.join("leaflet.html"), b"keep custom viewer").unwrap();
    let report = tile::to_directory(&mut source(), &options(), &output, true).unwrap();
    assert_eq!(report.resumed, 2);
    assert_eq!(std::fs::read(output.join("leaflet.html")).unwrap(), b"keep custom viewer");
}

#[test]
fn mapml_substitutions_are_single_pass_and_expansion_is_preflighted() {
    let mut options = options();
    options.publication.viewers = vec!["mapml".into()];
    options.publication.title = "${URL} & title".into();
    options.publication.mapml_template = Some("${TITLE}|${UNKNOWN}|${URL}|${unfinished".into());
    let mut sink = Collect::default();
    tile::generate(&mut source(), &options, &mut sink).unwrap();
    assert_eq!(std::str::from_utf8(&sink.0["mapml.mapml"]).unwrap(), "${URL} &amp; title|${UNKNOWN}||${unfinished");

    options.publication.title = "x".repeat(4096);
    options.publication.mapml_template = Some("${TITLE}".repeat(8192));
    let mut sink = Collect::default();
    let error = tile::generate(&mut source(), &options, &mut sink).unwrap_err();
    assert!(error.to_string().contains("metadata byte budget"), "{error}");
    assert!(sink.0.is_empty());
}

#[test]
fn kml_links_include_spatial_regions_and_unflipped_child_bounds() {
    let mut options = options();
    options.grid.max_zoom = Some(1);
    options.convention = Convention::Tms;
    options.publication.viewers.clear();
    let mut sink = Collect::default();
    tile::generate(&mut source(), &options, &mut sink).unwrap();
    let root = std::str::from_utf8(&sink.0["doc.kml"]).unwrap();
    assert!(root.contains("<Region><LatLonAltBox>"));
    assert!(root.contains("<viewRefreshMode>onRegion</viewRefreshMode>"));
    let parent = std::str::from_utf8(&sink.0["0/0/0.kml"]).unwrap();
    assert!(parent.contains("../../1/0/1.kml"));
    assert!(parent.contains("<minLodPixels>32</minLodPixels>"));
    let child = std::str::from_utf8(&sink.0["1/0/1.kml"]).unwrap();
    assert!(child.contains("<north>90</north>") && child.contains("<south>0</south>"));
    assert!(child.contains("<drawOrder>2</drawOrder>"));
}

#[test]
fn root_kml_budget_counts_escaped_urls_before_any_tiles_are_written() {
    let mut options = options();
    options.grid.min_zoom = Some(6);
    options.grid.max_zoom = Some(6);
    options.publication.viewers.clear();
    options.publication.url = Some(format!("https://example.test/{}", "&".repeat(500)));
    let error = tile::plan(&source().header(), &options).unwrap_err();
    assert!(error.to_string().contains("metadata byte budget"), "{error}");
}

#[test]
fn typed_stacta_preserves_projected_feet_and_nondefault_tile_size() {
    let mut dataset = source().read().unwrap();
    dataset.raster.data_type = DataType::U16;
    dataset.raster.data = RasterData::U16(vec![4000; 128 * 64]);
    dataset.raster.crs = terrio::crs::SpatialRef::from_epsg(2263).unwrap().to_crs_info();
    dataset.set_geo_transform([980_000., 2., 0., 200_000., 0., -2.]);
    let mut input = LazyRaster::from(dataset);
    let mut options = options();
    options.tiling_scheme = "raster".into();
    options.format = "GTiff".into();
    options.alpha = Some(false);
    options.publication.viewers = vec!["stac".into()];
    options.publication.kml = false;
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("tiles");
    tile::to_directory(&mut input, &options, &output, false).unwrap();
    let mut published = LazyRaster::open(output.join("stacta.json").to_str().unwrap()).unwrap();
    assert_eq!((published.width(), published.height(), published.band_count()), (128, 64, 1));
    assert_eq!(published.data_type(), DataType::U16);
    assert!(published.geo_transform().iter().zip([980_000., 2., 0., 200_000., 0., -2.]).all(|(a, b)| (*a - b).abs() < 1e-8));
    assert_eq!(published.read().unwrap().raster.data.to_f64_vec(), vec![4000.; 128 * 64]);
}

#[test]
fn empty_publication_plans_emit_no_companions() {
    let mut dataset = source().read().unwrap();
    dataset.set_geo_transform([190., 0.01, 0., 1., 0., -0.01]);
    let mut options = options();
    options.grid.no_intersection_ok = true;
    let mut sink = Collect::default();
    let report = tile::generate(&mut LazyRaster::from(dataset), &options, &mut sink).unwrap();
    assert_eq!(report.planned, 0);
    assert!(sink.0.is_empty());
}

#[test]
fn publication_urls_accept_ports_relative_colons_and_case_insensitive_http() {
    for url in [
        "//cdn.example.test:8443/tiles",
        "maps/2024:01/",
        "HTTPS://example.test/tiles",
        "https://[::1]:8443/tiles",
        "//[::1]:8443/tiles",
        "https://reader@example.test:443/tiles",
    ] {
        let mut options = options();
        options.publication.url = Some(url.into());
        assert!(tile::plan(&source().header(), &options).is_ok(), "{url}");
    }
}

#[test]
fn browser_assets_are_pinned_with_subresource_integrity() {
    let mut sink = Collect::default();
    tile::generate(&mut source(), &options(), &mut sink).unwrap();
    for (name, url, integrity) in [
        (
            "leaflet.html",
            "https://unpkg.com/leaflet@1.9.4/dist/leaflet.css",
            "sha384-sHL9NAb7lN7rfvG5lfHpm643Xkcjzp4jFvuavGOndn6pjVqS6ny56CAt3nsEVT4H",
        ),
        (
            "leaflet.html",
            "https://unpkg.com/leaflet@1.9.4/dist/leaflet.js",
            "sha384-cxOPjt7s7Iz04uaHJceBmS+qpjv2JkIHNVcuOrM+YHwZOmJGBXI00mdUXEq65HTH",
        ),
        (
            "openlayers.html",
            "https://cdn.jsdelivr.net/npm/ol@10.11.0/ol.css",
            "sha384-Jpg1uvTciFJuHq+90Ly0/2SKRT9yS5OhbIN5FuFoFWji9fc44ok8xxO88CJi8Z5W",
        ),
        (
            "openlayers.html",
            "https://cdn.jsdelivr.net/npm/ol@10.11.0/dist/ol.js",
            "sha384-rwbCGPbgxLkcQaFvIra1Vz3KBT1yBeF2/1VgLs/gCCNCJ61GHJGimHpkufVKcqap",
        ),
    ] {
        let html = std::str::from_utf8(&sink.0[name]).unwrap();
        assert!(html.contains(url), "{name}: missing pinned URL {url}");
        assert!(
            html.contains(&format!("{url}\" integrity=\"{integrity}\" crossorigin=\"anonymous\"")),
            "{name}: integrity does not match {url}"
        );
    }
}

#[test]
fn xml_forbidden_characters_are_rejected_before_publication() {
    for (title, copyright) in [("bad\u{1}", "ok"), ("ok", "bad\u{ffff}"), ("bad\u{fffe}", "ok")] {
        let mut options = options();
        options.publication.title = title.into();
        options.publication.copyright = copyright.into();
        let mut sink = Collect::default();
        assert!(tile::generate(&mut source(), &options, &mut sink).is_err());
        assert!(sink.0.is_empty());
    }
    let mut options = options();
    options.publication.title = "tabs\tand\r\nUnicode 🌍".into();
    assert!(tile::plan(&source().header(), &options).is_ok());
    options.publication.url = Some("maps/\u{1}/".into());
    assert!(tile::plan(&source().header(), &options).is_err());
}

#[test]
fn root_kml_planning_accepts_16384_links_within_the_actual_byte_budget() {
    let mut options = options();
    options.grid.min_zoom = Some(7);
    options.grid.max_zoom = Some(7);
    options.grid.limits.max_x = Some(127);
    options.publication.viewers.clear();
    let plan = tile::plan(&source().header(), &options).unwrap().0;
    assert_eq!(plan.count().unwrap(), 16384);
}

#[test]
fn invalid_http_hosts_and_obfuscated_schemes_fail_before_output() {
    for url in [
        "https://",
        "https:///tiles",
        "https://:443/tiles",
        "https://reader@:443/tiles",
        "https://reader@/tiles",
        "https://[]/tiles",
        "https://[::1/tiles",
        "//:443/tiles",
        "HTTP:/tiles",
        "\tjavascript:alert(1)",
        "java\tscript:alert(1)",
        " javaScript:alert(1)",
        "tiles/with space",
    ] {
        let mut options = options();
        options.publication.url = Some(url.into());
        let mut sink = Collect::default();
        assert!(tile::generate(&mut source(), &options, &mut sink).is_err(), "{url:?}");
        assert!(sink.0.is_empty());
    }
}

#[test]
fn stac_requires_blank_tiles_while_all_omits_inapplicable_stac() {
    let mut options = options();
    options.skip_blank = true;
    options.publication.viewers = vec!["stac".into()];
    let mut sink = Collect::default();
    assert!(tile::generate(&mut source(), &options, &mut sink).is_err());
    assert!(sink.0.is_empty());
    options.publication.viewers = vec!["all".into()];
    tile::generate(&mut source(), &options, &mut sink).unwrap();
    assert!(!sink.0.contains_key("stacta.json"));
    assert!(sink.0.contains_key("leaflet.html"));
}

#[test]
fn finer_kml_cells_outside_geographic_bounds_fail_before_pixels_or_output() {
    struct Metadata(RasterDataset);
    impl terrio::ops::warp::WarpSource for Metadata {
        fn info(&mut self) -> terrio::Result<terrio::ops::warp::WarpSourceInfo> {
            Ok(terrio::ops::warp::WarpSourceInfo { raster: self.0.clone(), overviews: vec![] })
        }
        fn read_window(&mut self, _: terrio::Window, _: &terrio::WindowOptions) -> terrio::Result<RasterDataset> {
            panic!("invalid KML plan must fail before reading pixels")
        }
    }
    let mut header = source().header();
    header.raster.cols = 1400;
    header.raster.rows = 1000;
    header.set_geo_transform([169., 0.01, 0., 10., 0., -0.01]);
    let mut input = Metadata(header);
    let mut options = options();
    options.tiling_scheme = "raster".into();
    options.grid = GridOptions { tile_size: 256, min_zoom: Some(1), max_zoom: Some(2), ..Default::default() };
    options.publication.viewers.clear();
    let mut sink = Collect::default();
    assert!(tile::generate(&mut input, &options, &mut sink).is_err());
    assert!(sink.0.is_empty());
}

#[test]
fn oversized_publication_plans_fail_before_geographic_cell_validation() {
    let mut options = options();
    options.grid.max_zoom = Some(1);
    options.max_tiles = 1;
    assert!(tile::plan(&source().header(), &options).unwrap_err().to_string().contains("max_tiles"));
}
