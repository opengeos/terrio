// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

pub fn model(version: &str, body: &str, crs: &str) -> String {
    let gml = if version == "2.0" { "http://www.opengis.net/gml" } else { "http://www.opengis.net/gml/3.2" };
    format!(
        r#"<c:CityModel xmlns:c="http://www.opengis.net/citygml/{version}" xmlns:g="{gml}" xmlns:b="http://www.opengis.net/citygml/building/{version}" xmlns:l="http://www.w3.org/1999/xlink">
    <g:boundedBy><g:Envelope srsName="{crs}" srsDimension="3"><g:lowerCorner>0 0 0</g:lowerCorner><g:upperCorner>1000 1000 1000</g:upperCorner></g:Envelope></g:boundedBy>
    <c:cityObjectMember><b:Building g:id="house">{body}</b:Building></c:cityObjectMember></c:CityModel>"#
    )
}

pub const FACE: &str = r#"<g:Polygon g:id="wall"><g:exterior><g:LinearRing><g:posList>10 0 0 10 10 0 10 10 10 10 0 10 10 0 0</g:posList></g:LinearRing></g:exterior><g:interior><g:LinearRing><g:posList>10 2 2 10 3 2 10 3 3 10 2 2</g:posList></g:LinearRing></g:interior></g:Polygon>"#;
