// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Native HTTP GET WFS discovery and feature reads; reference GDAL 3.13.3.
#![cfg(all(feature = "remote", not(target_family = "wasm")))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use terrio::{VectorOpenOptions, formats::VectorDriver};
use wbvector::{FieldValue, Geometry};

fn parameter<'a>(target: &'a str, key: &str) -> Option<&'a str> {
    target.split_once('?')?.1.split('&').find_map(|p| p.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v))
}

fn capabilities(base: &str, version: &str) -> String {
    let ns = if version.starts_with('2') { "http://www.opengis.net/wfs/2.0" } else { "http://www.opengis.net/wfs" };
    let operations = if version == "1.0.0" {
        let entries = ["GetFeature", "DescribeFeatureType"]
            .iter()
            .map(|op| {
                let path = if *op == "GetFeature" { "features" } else { "schema" };
                format!(r#"<{op}><DCPType><HTTP><Get onlineResource="{base}/{path}?fixed=1&amp;encoding=UTF-8"/></HTTP></DCPType></{op}>"#)
            })
            .collect::<String>();
        format!("<Capability><Request>{entries}</Request></Capability>")
    } else {
        let entries=["GetFeature","DescribeFeatureType"].iter().map(|op| {
            let path=if *op=="GetFeature" {"features"}else{"schema"};
            format!(r#"<ows:Operation name="{op}"><ows:DCP><ows:HTTP><ows:Get xlink:href="{base}/{path}?fixed=1&amp;encoding=UTF-8"/></ows:HTTP></ows:DCP></ows:Operation>"#)
        }).collect::<String>();
        format!(
            "<ows:OperationsMetadata>{entries}<ows:Constraint name=\"ImplementsResultPaging\"><ows:DefaultValue>TRUE</ows:DefaultValue></ows:Constraint></ows:OperationsMetadata>"
        )
    };
    let srs_tag = if version == "1.0.0" {
        "SRS"
    } else if version.starts_with('2') {
        "DefaultCRS"
    } else {
        "DefaultSRS"
    };
    let srs = if version == "1.0.0" { "EPSG:4326" } else { "urn:ogc:def:crs:EPSG::4326" };
    let types = ["roads", "empty"]
        .iter()
        .map(|name| format!("<FeatureType><Name>t:{name}</Name><Title>{name}</Title><{srs_tag}>{srs}</{srs_tag}></FeatureType>"))
        .collect::<String>();
    format!(
        r#"<wfs:WFS_Capabilities xmlns:wfs="{ns}" xmlns="{ns}" xmlns:t="urn:test" xmlns:ows="http://www.opengis.net/ows" xmlns:xlink="http://www.w3.org/1999/xlink" version="{version}">{operations}<FeatureTypeList>{types}</FeatureTypeList></wfs:WFS_Capabilities>"#
    )
}

fn schema(name: &str) -> String {
    format!(
        r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:gml="http://www.opengis.net/gml" xmlns:t="urn:test" targetNamespace="urn:test">
<xs:complexType name="{name}Type"><xs:complexContent><xs:extension base="gml:AbstractFeatureType"><xs:sequence>
<xs:element name="geom" type="gml:PointPropertyType"/><xs:element name="name" type="xs:string"/><xs:element name="rank" type="xs:int"/>
</xs:sequence></xs:extension></xs:complexContent></xs:complexType><xs:element name="{name}" type="t:{name}Type"/></xs:schema>"#
    )
}

fn page(name: &str, version: &str, offset: usize, count: usize) -> String {
    let total = if name == "empty" { 0 } else { 5 };
    let ns = if version.starts_with('2') { "http://www.opengis.net/wfs/2.0" } else { "http://www.opengis.net/wfs" };
    let mut features = String::new();
    for i in offset..offset.saturating_add(count).min(total) {
        let member = if version.starts_with('2') { "wfs:member" } else { "gml:featureMember" };
        let xy = if version == "1.0.0" {
            format!("<gml:coordinates>{},{}</gml:coordinates>", 10 + i, 50 + i)
        } else {
            format!("<gml:pos>{} {}</gml:pos>", 50 + i, 10 + i)
        };
        let srs = if version == "1.0.0" { "EPSG:4326" } else { "urn:ogc:def:crs:EPSG::4326" };
        let id = if version == "1.0.0" { "fid" } else { "gml:id" };
        features.push_str(&format!(r#"<{member}><t:{name} {id}="{name}.{}"><t:geom><gml:Point srsName="{srs}">{xy}</gml:Point></t:geom><t:name>point{}</t:name><t:rank>{}</t:rank></t:{name}></{member}>"#,i+1,i+1,i+1));
    }
    let matched = if version.starts_with('2') {
        format!(r#" numberMatched="{total}" numberReturned="{}""#, count.min(total.saturating_sub(offset)))
    } else {
        String::new()
    };
    format!(
        r#"<wfs:FeatureCollection xmlns:wfs="{ns}" xmlns:gml="http://www.opengis.net/gml" xmlns:t="urn:test"{matched}>{features}</wfs:FeatureCollection>"#
    )
}

fn serve(version: &'static str, mode: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let endpoint = base.clone();
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            record.lock().unwrap().push(target.clone());
            let operation = parameter(&target, "REQUEST").unwrap_or("");
            let body = if operation.eq_ignore_ascii_case("GetCapabilities") {
                let mut caps = capabilities(&endpoint, version);
                if mode.starts_with("no-default") {
                    caps = caps.replace("<DefaultCRS>urn:ogc:def:crs:EPSG::4326</DefaultCRS>", "");
                }
                if mode == "crs84-urn" {
                    caps = caps.replace("urn:ogc:def:crs:EPSG::4326", "urn:ogc:def:crs:OGC:1.3:CRS84");
                }
                if mode == "crs84-url" {
                    caps = caps.replace("urn:ogc:def:crs:EPSG::4326", "http://www.opengis.net/def/crs/OGC/1.3/CRS84");
                }
                if mode == "truncated" {
                    caps.truncate(caps.len() - 24);
                }
                caps
            } else if operation.eq_ignore_ascii_case("DescribeFeatureType") {
                let name = if target.contains("empty") { "empty" } else { "roads" };
                let mut xsd = schema(name);
                if mode == "inline" {
                    xsd = xsd
                        .replace(
                            &format!(r#"<xs:complexType name="{name}Type">"#),
                            &format!(r#"<xs:element name="{name}"><xs:complexType>"#),
                        )
                        .replace(
                            &format!(r#"</xs:complexType><xs:element name="{name}" type="t:{name}Type"/>"#),
                            "</xs:complexType></xs:element>",
                        );
                }
                if mode == "inline" {
                    xsd = xsd
                        .replace(
                            &format!(r#"<xs:element name="{name}"><xs:complexType>"#),
                            &format!(r#"<xs:element name="{name}" xmlns:g="http://www.opengis.net/gml/3.2"><xs:complexType>"#),
                        )
                        .replace("gml:PointPropertyType", "g:PointPropertyType");
                }
                if mode == "alias" {
                    xsd = xsd
                        .replace("xmlns:gml=", "xmlns:gml32=")
                        .replace("http://www.opengis.net/gml\"", "http://www.opengis.net/gml/3.2\"")
                        .replace("gml:", "gml32:");
                }
                if mode == "imported-type" {
                    xsd = xsd
                        .replace(r#"targetNamespace="urn:test""#, r#"xmlns:foreign="urn:foreign" targetNamespace="urn:test""#)
                        .replace(&format!(r#"type="t:{name}Type""#), &format!(r#"type="foreign:{name}Type""#));
                }
                if mode == "dense" {
                    let fields =
                        (0..5000).map(|i| format!(r#"<xs:element name="f{i}" type="xs:string" minOccurs="0"/>"#)).collect::<String>();
                    xsd = xsd.replace("</xs:sequence>", &format!("{fields}</xs:sequence>"));
                }
                xsd
            } else if operation.eq_ignore_ascii_case("GetFeature") {
                if mode == "exception" {
                    r#"<ows:ExceptionReport xmlns:ows="http://www.opengis.net/ows"><ows:Exception><ows:ExceptionText>Layer unavailable</ows:ExceptionText></ows:Exception></ows:ExceptionReport>"#.into()
                } else {
                    let offset = if matches!(mode, "repeat" | "repeat-no-ids") {
                        0
                    } else {
                        parameter(&target, "STARTINDEX").and_then(|s| s.parse().ok()).unwrap_or(0)
                    };
                    let count =
                        parameter(&target, "COUNT").or_else(|| parameter(&target, "MAXFEATURES")).and_then(|s| s.parse().ok()).unwrap_or(5);
                    if mode == "dense" {
                        let features = (0..500)
                            .map(|i| {
                                format!(
                                    r#"<wfs:member><t:roads gml:id="r{i}"><t:name>r{i}</t:name><t:rank>{i}</t:rank></t:roads></wfs:member>"#
                                )
                            })
                            .collect::<String>();
                        format!(
                            r#"<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0" xmlns:gml="http://www.opengis.net/gml" xmlns:t="urn:test" numberMatched="500">{features}</wfs:FeatureCollection>"#
                        )
                    } else {
                        let mut data = page(if target.contains("empty") { "empty" } else { "roads" }, version, offset, count);
                        if mode == "mixed-points" && offset == 2 {
                            data = data
                                .replace("<t:geom>", "<t:geom><gml:MultiPoint><gml:pointMember>")
                                .replace("</t:geom>", "</gml:pointMember></gml:MultiPoint></t:geom>");
                        }
                        if mode == "no-features" {
                            data = page("empty", version, 0, 0);
                        }
                        if mode == "wrong-namespace" {
                            data = data.replace("urn:test", "urn:other");
                        }
                        if mode == "namespace-alias" {
                            data = data.replace("xmlns:t=", "xmlns:b=\"urn:test\" xmlns:t=").replace("t:roads", "b:roads");
                        }
                        if mode == "bad-point" {
                            data = data.replace("<gml:pos>50 10</gml:pos>", "<gml:pos>invalid coordinates</gml:pos>");
                        }
                        if matches!(mode, "wrong-crs" | "no-default-mixed") {
                            data = data.replace(
                                r#"srsName="urn:ogc:def:crs:EPSG::4326"><gml:pos>51 11"#,
                                r#"srsName="urn:ogc:def:crs:EPSG::3857"><gml:pos>51 11"#,
                            );
                        }
                        if mode == "no-default-axis" {
                            data = data
                                .replace(r#"srsName="urn:ogc:def:crs:EPSG::4326"><gml:pos>51 11"#, r#"srsName="EPSG:4326"><gml:pos>11 51"#);
                        }
                        if mode == "shadow-ns" {
                            data = data.replace(r#"xmlns:t="urn:test""#, r#"xmlns:o="urn:noise" o:t="urn:test" xmlns:t="urn:other""#);
                        }
                        if mode == "shadow-ns-good" {
                            data = data.replace(r#"xmlns:t="urn:test""#, r#"xmlns:o="urn:noise" o:t="urn:other" xmlns:t="urn:test""#);
                        }
                        if mode == "scalar-geometry" {
                            data = data.replace(r#"<t:geom><gml:Point srsName="urn:ogc:def:crs:EPSG::4326"><gml:pos>50 10</gml:pos></gml:Point></t:geom>"#, "")
                                .replace("<t:name>point1</t:name>", "<t:name><gml:Point><gml:pos>50 10</gml:pos></gml:Point></t:name>");
                        }
                        if mode == "bad-entity-attribute" {
                            data = data.replace("roads.1", "a&unknown;b");
                        }
                        if mode == "bad-entity" {
                            data = data.replace("point1", "a&unknown;b");
                        }
                        if mode == "good-entity" {
                            data = data.replace("point1", "a&amp;&#x62;");
                        }
                        if mode == "complex" {
                            data = data.replace("<t:name>point1</t:name>", "<t:name><t:nested>point1</t:nested></t:name>");
                        }
                        if matches!(mode, "curve" | "spline") {
                            data =
                                data.replace("<gml:pos>50 10</gml:pos>", "<gml:Arc><gml:posList>50 10 51 11 52 12</gml:posList></gml:Arc>");
                        }
                        if mode == "spline" {
                            data = data
                                .replace("<gml:Arc>", "<gml:Curve><gml:segments><gml:CubicSpline>")
                                .replace("</gml:Arc>", "</gml:CubicSpline></gml:segments></gml:Curve>");
                        }
                        if mode == "repeat-no-ids" {
                            data = data.replace(r#"numberMatched="5""#, r#"numberMatched="4""#);
                            for i in 1..=5 {
                                data = data.replace(&format!(r#" gml:id="roads.{i}""#), "");
                            }
                        }
                        data
                    }
                }
            } else {
                "unknown operation".into()
            };
            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    (format!("WFS:{base}/service?map=demo"), requests)
}

#[test]
fn capabilities_info_and_layer_listing_fetch_no_features() {
    for version in ["1.0.0", "1.1.0", "2.0.0", "2.0.2"] {
        let (source, requests) = serve(version, "normal");
        let info = terrio::info::info_path(&source, &Default::default()).unwrap();
        let value = serde_json::to_value(info).unwrap();
        assert_eq!(value["type"], "vector");
        assert_eq!(value["driver"], "WFS");
        assert_eq!(value["metadata_only"], true);
        assert_eq!(value["layers"].as_array().unwrap().len(), 2);
        assert!(value["layers"][0]["feature_count"].is_null());
        assert_eq!(requests.lock().unwrap().len(), 1);
        requests.lock().unwrap().clear();
        assert_eq!(terrio::list_layers(&source).unwrap(), ["t:roads", "t:empty"]);
        assert_eq!(requests.lock().unwrap().len(), 1);
        requests.lock().unwrap().clear();
        assert_eq!(terrio::inspection::identify(&source, false).unwrap().unwrap().driver, "WFS");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn versions_schemas_paging_and_authority_axis_order_are_read() {
    for version in ["1.0.0", "1.1.0", "2.0.0", "2.0.2"] {
        let (source, requests) = serve(version, "normal");
        let mut options = VectorOpenOptions { layers: vec!["t:roads".into()], ..Default::default() };
        options.wfs.page_size = 2;
        let ds = terrio::open_vector(&source, &options).unwrap();
        assert_eq!(ds.driver, VectorDriver::WFS);
        let layer = &ds.layers[0];
        assert_eq!(layer.name, "t:roads");
        assert_eq!(layer.features.len(), 5);
        assert_eq!(layer.crs_epsg(), Some(4326));
        for (i, f) in layer.features.iter().enumerate() {
            assert_eq!(f.fid, i as u64);
            assert_eq!(f.attributes[2], FieldValue::Integer((i + 1) as i64));
            assert!(matches!(&f.geometry,Some(Geometry::Point(c)) if c.x==10.0+i as f64 && c.y==50.0+i as f64));
        }
        let requests = requests.lock().unwrap();
        let pages: Vec<_> = requests.iter().filter(|q| q.contains("REQUEST=GetFeature")).collect();
        assert_eq!(pages.len(), if version.starts_with('2') { 3 } else { 1 }, "{requests:?}");
        assert!(requests.iter().all(|q| q.contains("map=demo")), "{requests:?}");
        assert!(requests.iter().all(|q| !q.contains("amp;")), "{requests:?}");
        assert!(pages.iter().all(|q| q.contains(if version.starts_with('2') { "TYPENAMES=t%3Aroads" } else { "TYPENAME=t%3Aroads" })));
    }
}

#[test]
fn selected_ranges_and_empty_layers_keep_schema_and_crs() {
    let (source, _) = serve("2.0.0", "normal");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], skip_features: 1, max_features: Some(2), ..Default::default() };
    options.wfs.page_size = 1;
    let ds = terrio::open_vector(&source, &options).unwrap();
    assert_eq!(ds.layers[0].features.len(), 2);
    assert_eq!(ds.layers[0].features[0].attributes[2], FieldValue::Integer(2));
    let empty = terrio::open_vector(&source, &VectorOpenOptions { layers: vec!["empty".into()], ..Default::default() }).unwrap();
    assert!(empty.layers[0].features.is_empty());
    assert_eq!(empty.layers[0].schema.fields().len(), 3);
    assert_eq!(empty.layers[0].crs_epsg(), Some(4326));
}

#[test]
fn repeated_pages_and_service_exceptions_are_errors() {
    for (mode, needle) in [("repeat", "STARTINDEX"), ("exception", "Layer unavailable"), ("truncated", "XML")] {
        let (source, _) = serve("2.0.0", mode);
        let mut options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
        options.wfs.page_size = 2;
        let error = terrio::open_vector(&source, &options).unwrap_err().to_string();
        assert!(error.contains(needle), "{error}");
    }
}

#[test]
fn retained_budget_refusals_are_terminal_before_feature_fetch() {
    let (source, requests) = serve("2.0.0", "normal");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    options.wfs.max_vector_bytes = 1;
    assert!(matches!(terrio::open_vector(&source, &options), Err(terrio::Error::ResourceLimit(_))));
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(!requests.lock().unwrap().iter().any(|q| q.contains("REQUEST=GetFeature")));
}

#[test]
fn disabled_paging_ranges_include_skipped_features() {
    let (source, _) = serve("2.0.0", "normal");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], skip_features: 2, max_features: Some(2), ..Default::default() };
    options.wfs.paging = Some(false);
    let ds = terrio::open_vector(&source, &options).unwrap();
    assert_eq!(ds.layers[0].features.len(), 2);
    assert_eq!(ds.layers[0].features[0].attributes[2], FieldValue::Integer(3));
}

#[test]
fn crs84_filters_keep_longitude_latitude_order() {
    for mode in ["crs84-urn", "crs84-url"] {
        let (source, requests) = serve("2.0.0", mode);
        let options = VectorOpenOptions {
            layers: vec!["roads".into()],
            max_features: Some(1),
            spatial_filter: Some((9., 49., 12., 52.)),
            ..Default::default()
        };
        terrio::open_vector(&source, &options).unwrap();
        let requests = requests.lock().unwrap();
        let feature = requests.iter().find(|q| q.contains("REQUEST=GetFeature")).unwrap();
        assert!(feature.contains("BBOX=9%2C49%2C12%2C52%2C"), "{feature}");
    }
}

#[test]
fn repeated_pages_without_ids_are_refused() {
    let (source, _) = serve("2.0.0", "repeat-no-ids");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    options.wfs.page_size = 2;
    let error = terrio::open_vector(&source, &options).unwrap_err();
    assert!(error.to_string().contains("STARTINDEX"), "{error}");
}

#[test]
fn sparse_pages_preflight_the_dense_field_matrix() {
    let (source, _) = serve("2.0.0", "dense");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    options.wfs.max_vector_bytes = 1_000_000;
    let error = terrio::open_vector(&source, &options).unwrap_err();
    assert!(matches!(&error,terrio::Error::ResourceLimit(message) if message.contains("dense")), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn dense_field_allocation_failure_is_a_resource_error() {
    const CHILD: &str = "TERRIO_WFS_DENSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "dense_field_allocation_failure_is_a_resource_error", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        return;
    }
    let (source, _) = serve("2.0.0", "dense");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], max_features: Some(0), ..Default::default() };
    terrio::open_vector(&source, &options).unwrap();
    options.max_features = None;
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let used =
        status.lines().find(|line| line.starts_with("VmSize:")).unwrap().split_whitespace().nth(1).unwrap().parse::<u64>().unwrap() * 1024;
    let limit = libc::rlimit { rlim_cur: used + (40 << 20), rlim_max: used + (40 << 20) };
    let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: valid pointers; these limits affect only this isolated test child.
    unsafe {
        assert_eq!(libc::setrlimit(libc::RLIMIT_CORE, &no_core), 0);
        assert_eq!(libc::setrlimit(libc::RLIMIT_AS, &limit), 0);
    }
    let error = terrio::open_vector(&source, &options).unwrap_err();
    assert!(matches!(&error, terrio::Error::ResourceLimit(_)), "{error}");
}

#[test]
fn unsupported_complex_and_curved_features_are_explicit_errors() {
    for mode in ["complex", "curve", "spline"] {
        let (source, _) = serve("2.0.0", mode);
        let options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
        let error = terrio::open_vector(&source, &options).unwrap_err();
        assert!(matches!(&error, terrio::Error::Unsupported(_)), "{error}");
    }
}

#[test]
fn schema_namespace_aliases_preserve_fields_and_empty_geometry_types() {
    let (source, _) = serve("2.0.0", "alias");
    let ds = terrio::open_vector(&source, &Default::default()).unwrap();
    for layer in &ds.layers {
        assert_eq!(layer.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["gml_id", "name", "rank"]);
        assert_eq!(layer.geom_type, Some(wbvector::GeometryType::Point));
    }
    assert_eq!(ds.layers[1].features.len(), 0);
}

#[test]
fn getfeature_urls_report_service_exception_text_without_fallback() {
    let (source, requests) = serve("2.0.0", "exception");
    let url = format!("{}&SERVICE=WFS&REQUEST=GetFeature", source.strip_prefix("WFS:").unwrap());
    let error = terrio::open_vector(&url, &Default::default()).unwrap_err();
    assert!(matches!(&error, terrio::Error::InvalidData(message) if message.contains("Layer unavailable")), "{error}");
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn incomplete_single_pages_warn_instead_of_silently_truncating() {
    let (source, _) = serve("2.0.0", "normal");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    options.wfs.paging = Some(false);
    options.wfs.page_size = 2;
    terrio::warnings::take();
    let ds = terrio::open_vector(&source, &options).unwrap();
    assert_eq!(ds.layers[0].features.len(), 2);
    assert!(terrio::warnings::take().iter().any(|w| w.contains("2 of 5") && w.contains("WFS_PAGING=YES")));
}

#[test]
fn anonymous_application_schemas_preserve_nonempty_and_empty_layers() {
    let (source, _) = serve("2.0.0", "inline");
    let ds = terrio::open_vector(&source, &Default::default()).unwrap();
    assert_eq!(ds.layers[0].features.len(), 5);
    assert!(ds.layers[1].features.is_empty());
    for layer in &ds.layers {
        assert_eq!(layer.schema.fields().len(), 3);
        assert_eq!(layer.geom_type, Some(wbvector::GeometryType::Point));
        assert_eq!(layer.schema.fields()[2].field_type, wbvector::FieldType::Integer);
    }
}

#[test]
fn malformed_geometry_namespace_crs_and_entities_are_terminal_errors() {
    for mode in ["bad-point", "wrong-namespace", "wrong-crs", "bad-entity", "bad-entity-attribute"] {
        let (source, requests) = serve("2.0.0", mode);
        let options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
        let error = terrio::open_vector(&source, &options).unwrap_err();
        assert!(matches!(error, terrio::Error::InvalidData(_)), "{mode}: {error}");
        assert_eq!(requests.lock().unwrap().len(), 3, "no retry after a rejected first page");
    }
}

#[test]
fn namespace_aliases_and_valid_xml_entities_remain_supported() {
    for mode in ["namespace-alias", "good-entity"] {
        let (source, _) = serve("2.0.0", mode);
        let options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
        let ds = terrio::open_vector(&source, &options).unwrap();
        assert_eq!(ds.layers[0].features.len(), 5);
        if mode == "good-entity" {
            assert_eq!(ds.layers[0].features[0].attributes[1], FieldValue::Text("a&b".into()));
        }
    }
}

#[test]
fn generated_bbox_replaces_source_bbox_and_otherwise_keeps_it() {
    for replace in [false, true] {
        let (source, requests) = serve("2.0.0", "normal");
        let source = format!("{source}&bBoX=0%2C0%2C1%2C1");
        let options = VectorOpenOptions {
            layers: vec!["roads".into()],
            max_features: Some(1),
            spatial_filter: replace.then_some((9., 49., 12., 52.)),
            ..Default::default()
        };
        terrio::open_vector(&source, &options).unwrap();
        let seen = requests.lock().unwrap();
        let request = seen.iter().find(|q| q.contains("REQUEST=GetFeature")).unwrap();
        assert_eq!(request.to_ascii_uppercase().matches("BBOX=").count(), 1, "{request}");
        assert!(request.contains(if replace { "BBOX=49%2C9%2C52%2C12%2C" } else { "bBoX=0%2C0%2C1%2C1" }), "{request}");
    }
}

#[test]
fn malformed_recognized_getfeature_does_not_retry_through_gdal() {
    let (source, requests) = serve("2.0.0", "no-features");
    let url = format!("{}&SERVICE=WFS&REQUEST=GetFeature", source.strip_prefix("WFS:").unwrap());
    let error = terrio::open_vector(&url, &Default::default()).unwrap_err();
    assert!(matches!(error, terrio::Error::InvalidData(_)), "{error}");
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn schema_geometry_hints_do_not_mislabel_mixed_pages() {
    let (source, _) = serve("2.0.0", "mixed-points");
    let mut options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    options.wfs.page_size = 2;
    let ds = terrio::open_vector(&source, &options).unwrap();
    assert!(matches!(ds.layers[0].features[0].geometry, Some(Geometry::Point(_))));
    assert!(matches!(ds.layers[0].features[2].geometry, Some(Geometry::MultiPoint(_))));
    assert_eq!(terrio::vector::geometry_type_name(&ds.layers[0]), "MultiPoint");
}

#[test]
fn foreign_schema_type_names_cannot_resolve_to_local_collisions() {
    let (source, requests) = serve("2.0.0", "imported-type");
    let options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    let error = terrio::open_vector(&source, &options).unwrap_err();
    assert!(matches!(&error, terrio::Error::Unsupported(message) if message.contains("external application-schema")), "{error}");
    assert_eq!(requests.lock().unwrap().len(), 2, "reject before fetching features or following imports");
}

#[test]
fn undeclared_default_crs_uses_the_first_geometry_and_refuses_later_mismatches() {
    let (source, _) = serve("2.0.0", "no-default");
    let options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    let ds = terrio::open_vector(&source, &options).unwrap();
    assert_eq!(ds.layers[0].crs_epsg(), Some(4326));
    for page_size in [1, 1000] {
        let (source, _) = serve("2.0.0", "no-default-mixed");
        let mut options = options.clone();
        options.wfs.page_size = page_size;
        let error = terrio::open_vector(&source, &options).unwrap_err();
        assert!(error.to_string().contains("CRS differs"), "{error}");
    }
    let (source, _) = serve("2.0.0", "no-default-axis");
    assert!(terrio::open_vector(&source, &options).unwrap_err().to_string().contains("axis orders"));
}

#[test]
fn geometry_in_a_scalar_property_is_an_error() {
    let (source, _) = serve("2.0.0", "scalar-geometry");
    let options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    let error = terrio::open_vector(&source, &options).unwrap_err();
    assert!(error.to_string().contains("scalar property"), "{error}");
}

#[test]
fn ordinary_attributes_cannot_shadow_namespace_declarations() {
    let (source, _) = serve("2.0.0", "shadow-ns-good");
    let options = VectorOpenOptions { layers: vec!["roads".into()], ..Default::default() };
    assert_eq!(terrio::open_vector(&source, &options).unwrap().layers[0].features.len(), 5);
    let (source, _) = serve("2.0.0", "shadow-ns");
    let error = terrio::open_vector(&source, &options).unwrap_err();
    assert!(error.to_string().contains("feature namespace"), "{error}");
}
