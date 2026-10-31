//! Publication companions for folder raster tiles. Reference: GDAL 3.13.3.
//!
//! The supplied URL names the tile directory itself. Generated viewers use pinned
//! public Leaflet/OpenLayers distributions; no third-party code is embedded here.
//! Independently implemented to match GDAL behavior, without translating its code.

use super::raster_tile::{Convention, FolderSink, TileCell, TilePlan};
use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, Result};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeSet;

/// Optional files alongside the encoded tiles. Empty viewers preserve the
/// historical image-only output. `all` selects companions applicable to the grid.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PublicationOptions {
    /// none, all, leaflet, openlayers, mapml or stac.
    pub viewers: Vec<String>,
    /// Published tile-directory URL; otherwise companions use relative links.
    pub url: Option<String>,
    /// Human-readable map title.
    pub title: String,
    /// Plain-text attribution, escaped for each output language.
    pub copyright: String,
    /// MapML template contents, using GDAL's `${TITLE}`, `${URL}`, etc.
    #[serde(alias = "mapmlTemplate")]
    pub mapml_template: Option<String>,
    /// Generate a root KML and a companion for every planned tile.
    pub kml: bool,
}

/// Validated publication state; retains levels, never image pixels or a pyramid
/// of per-tile records. A KML file is emitted as each tile is processed.
pub(crate) struct Publication<'a> {
    plan: &'a TilePlan,
    options: &'a PublicationOptions,
    convention: Convention,
    extension: &'a str,
    viewers: BTreeSet<String>,
    scheme: &'a str,
    bounds: [f64; 4],
    geographic: Option<crate::crs::Transformer>,
}

const MAX_METADATA: usize = 16 * 1024 * 1024;

fn invalid(message: &str) -> Error {
    Error::InvalidArgument(message.into())
}

impl<'a> Publication<'a> {
    pub(crate) fn new(
        plan: &'a TilePlan,
        source: &RasterDataset,
        options: &'a PublicationOptions,
        scheme: &'a str,
        convention: Convention,
        extension: &'a str,
    ) -> Result<Self> {
        if [&options.title, &options.copyright].iter().any(|s| s.len() > 4096)
            || options.url.as_ref().is_some_and(|s| s.len() > 8192 || s.contains(['?', '#', '\0']))
            || options.mapml_template.as_ref().is_some_and(|s| s.len() > MAX_METADATA)
        {
            return Err(invalid("publication strings exceed their limits, or the URL contains a query/fragment"));
        }
        if let Some(url) = &options.url
            && let Some((protocol, _)) = url.split_once(':')
            && protocol.starts_with(|c: char| c.is_ascii_alphabetic())
            && protocol.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && !protocol.eq_ignore_ascii_case("http")
            && !protocol.eq_ignore_ascii_case("https")
        {
            return Err(invalid("publication URLs must be HTTP(S) or relative"));
        }
        let global = !scheme.eq_ignore_ascii_case("raster");
        let square = plan.levels.iter().all(|l| (l.matrix.resolution[0] / l.matrix.resolution[1] - 1.).abs() < 1e-12);
        let applicable = |name: &str| match name {
            "leaflet" => global && extension == "png",
            "openlayers" => extension == "png",
            "mapml" => global && convention == Convention::Xyz,
            "stac" => convention == Convention::Xyz && plan.crs.is_some() && square,
            _ => false,
        };
        let mut viewers = BTreeSet::new();
        for name in &options.viewers {
            let name = name.to_ascii_lowercase();
            match name.as_str() {
                "none" if options.viewers.len() == 1 => {}
                "all" => {
                    for candidate in ["leaflet", "openlayers", "mapml", "stac"] {
                        if applicable(candidate) {
                            viewers.insert(candidate.into());
                        }
                    }
                }
                "leaflet" | "openlayers" | "mapml" | "stac" if applicable(&name) => {
                    viewers.insert(name);
                }
                "leaflet" | "openlayers" | "mapml" | "stac" => {
                    return Err(invalid("requested publication is incompatible with this grid, convention or image format"));
                }
                _ => return Err(invalid("viewers must be none, all, leaflet, openlayers, mapml or stac")),
            }
        }
        if options.mapml_template.is_some() && !viewers.contains("mapml") {
            return Err(invalid("mapml_template requires mapml publication"));
        }
        let bounds = plan.levels.last().map(level_bounds).unwrap_or([0.; 4]);
        let geographic = if !plan.levels.is_empty() && (options.kml || viewers.contains("stac") || viewers.contains("leaflet")) {
            let srs = match &plan.crs {
                Some(definition) => SpatialRef::parse(definition)?,
                None => source.crs(),
            };
            if srs.is_unknown() {
                return Err(invalid("KML/STAC/geographic viewers require a known CRS"));
            }
            Some(crate::crs::Transformer::new(&srs, &SpatialRef::from_epsg(4326)?)?)
        } else {
            None
        };
        if options.kml && plan.levels.first().is_some_and(|l| l.range.count().unwrap_or(u64::MAX) > 20_000) {
            return Err(invalid("root KML is limited to 20,000 network links; select a lower minimum zoom"));
        }
        let publication = Self { plan, options, convention, extension, viewers, scheme, bounds, geographic };
        if !plan.levels.is_empty() && publication.geographic.is_some() {
            publication.geo_bounds(bounds)?;
        }
        if publication.viewers.contains("stac") && !plan.levels.is_empty() {
            publication.stac(source)?;
        }
        if publication.viewers.contains("mapml") && !plan.levels.is_empty() {
            publication.mapml()?;
        }
        if options.kml {
            let links = plan.levels.first().map(|l| l.range.count()).transpose()?.unwrap_or(0);
            let per_link = 1024 + options.url.as_ref().map_or(0, |url| xml(url).len());
            let root_bytes = xml(&options.title).len() + xml(&options.copyright).len() + 128;
            if links > (MAX_METADATA.saturating_sub(root_bytes) / per_link) as u64 {
                return Err(invalid("root KML exceeds the metadata byte budget"));
            }
        }
        Ok(publication)
    }

    pub(crate) fn fixed_alpha(&self) -> bool {
        self.extension == "png" && self.viewers.contains("stac")
    }

    fn href(&self, path: &str) -> String {
        match &self.options.url {
            Some(base) => format!("{}/{path}", base.trim_end_matches('/')),
            None => path.into(),
        }
    }

    fn geo_bounds(&self, bounds: [f64; 4]) -> Result<[f64; 4]> {
        let transform = self.geographic.as_ref().ok_or_else(|| invalid("missing publication CRS transform"))?;
        let (x0, y0, x1, y1) = transform.transform_bounds((bounds[0], bounds[1], bounds[2], bounds[3]))?;
        if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) || x0 > x1 || y0 > y1 {
            return Err(invalid("publication extent cannot be represented by one geographic bounding box"));
        }
        let clipped = [x0.max(-180.), y0.max(-90.), x1.min(180.), y1.min(90.)];
        if clipped[0] >= clipped[2] || clipped[1] >= clipped[3] {
            return Err(invalid("publication extent lies outside valid geographic bounds"));
        }
        Ok(clipped)
    }

    pub(crate) fn cell(&self, cell: &TileCell, image: bool, sink: &mut dyn FolderSink) -> Result<()> {
        if !self.options.kml {
            return Ok(());
        }
        let name = format!("{}/{}/{}.kml", cell.zoom, cell.x, cell.y);
        if sink.exists(&name)? {
            return Ok(());
        }
        let bounds = self.geo_bounds(cell_bounds(cell))?;
        let mut body = region(bounds, cell.size[0]);
        if image {
            body.push_str(&format!(
                "<GroundOverlay><drawOrder>{}</drawOrder><name>{}</name><Icon><href>{}</href></Icon>{}</GroundOverlay>",
                u16::from(cell.zoom) + 1,
                xml(&self.options.title),
                xml(&if self.options.url.is_some() {
                    self.href(&format!("{}/{}/{}.{}", cell.zoom, cell.x, cell.y, self.extension))
                } else {
                    format!("{}.{}", cell.y, self.extension)
                }),
                latlon(bounds)
            ));
        }
        if let Some((i, level)) = self.plan.levels.iter().enumerate().find(|(_, l)| l.matrix.zoom == cell.zoom)
            && let Some(next) = self.plan.levels.get(i + 1)
        {
            let factor = 1u64 << (next.matrix.zoom - level.matrix.zoom);
            let row = if self.convention == Convention::Tms { level.matrix.rows - 1 - cell.y } else { cell.y };
            for y in (row * factor).max(next.range.min_y)..=((row + 1) * factor - 1).min(next.range.max_y) {
                for x in (cell.x * factor).max(next.range.min_x)..=((cell.x + 1) * factor - 1).min(next.range.max_x) {
                    let stored = if self.convention == Convention::Tms { next.matrix.rows - 1 - y } else { y };
                    let m = next.matrix;
                    let width = m.tile_size as f64 * m.resolution[0];
                    let height = m.tile_size as f64 * m.resolution[1];
                    let child_bounds = self.geo_bounds([
                        m.origin[0] + x as f64 * width,
                        m.origin[1] - (y + 1) as f64 * height,
                        m.origin[0] + (x + 1) as f64 * width,
                        m.origin[1] - y as f64 * height,
                    ])?;
                    body.push_str(&format!(
                        "<NetworkLink>{}<Link><href>{}</href><viewRefreshMode>onRegion</viewRefreshMode></Link></NetworkLink>",
                        region(child_bounds, m.tile_size),
                        xml(&if self.options.url.is_some() {
                            self.href(&format!("{}/{x}/{stored}.kml", next.matrix.zoom))
                        } else {
                            format!("../../{}/{x}/{stored}.kml", next.matrix.zoom)
                        })
                    ));
                }
            }
        }
        let content = kml(&body);
        put(sink, &name, content.as_bytes())
    }

    pub(crate) fn finish(&self, source: &RasterDataset, sink: &mut dyn FolderSink) -> Result<()> {
        if self.plan.levels.is_empty() {
            return Ok(());
        }
        for viewer in &self.viewers {
            let (name, content) = match viewer.as_str() {
                "stac" => ("stacta.json", self.stac(source)?),
                "mapml" => ("mapml.mapml", self.mapml()?),
                "leaflet" => ("leaflet.html", self.leaflet()?),
                "openlayers" => ("openlayers.html", self.openlayers()),
                _ => unreachable!(),
            };
            put(sink, name, content.as_bytes())?;
        }
        if self.options.kml {
            let mut body = format!("<name>{}</name><description>{}</description>", xml(&self.options.title), xml(&self.options.copyright));
            for cell in self.plan.levels[0].cells(self.convention) {
                body.push_str(&format!(
                    "<NetworkLink>{}<Link><href>{}</href><viewRefreshMode>onRegion</viewRefreshMode></Link></NetworkLink>",
                    region(self.geo_bounds(cell_bounds(&cell))?, cell.size[0]),
                    xml(&self.href(&format!("{}/{}/{}.kml", cell.zoom, cell.x, cell.y)))
                ));
                if body.len() > MAX_METADATA {
                    return Err(invalid("root KML exceeds the metadata byte budget"));
                }
            }
            put(sink, "doc.kml", kml(&body).as_bytes())?;
        }
        Ok(())
    }

    fn stac(&self, source: &RasterDataset) -> Result<String> {
        let bounds = self.geo_bounds(self.bounds)?;
        let crs = self.plan.crs.as_deref().ok_or_else(|| invalid("STAC requires a CRS"))?;
        let srs = SpatialRef::parse(crs)?;
        let meters = srs.metres_per_unit();
        if !meters.is_finite() || meters <= 0. {
            return Err(invalid("STAC needs a known unit conversion"));
        }
        let matrices: Vec<_> = self
            .plan
            .levels
            .iter()
            .map(|l| {
                json!({
                    "type":"TileMatrixType", "identifier":l.matrix.zoom.to_string(),
                    "scaleDenominator":l.matrix.resolution[1] * meters / 0.00028,
                    "topLeftCorner":l.matrix.origin, "tileWidth":l.matrix.tile_size, "tileHeight":l.matrix.tile_size,
                    "matrixWidth":l.matrix.columns, "matrixHeight":l.matrix.rows
                })
            })
            .collect();
        let limits: serde_json::Map<_, _> = self
            .plan
            .levels
            .iter()
            .map(|l| {
                (
                    l.matrix.zoom.to_string(),
                    json!({
                        "min_tile_col":l.range.min_x,"max_tile_col":l.range.max_x,"min_tile_row":l.range.min_y,"max_tile_row":l.range.max_y
                    }),
                )
            })
            .collect();
        let bands: Vec<_> =
            (0..source.raster.bands).map(|i| json!({"name":format!("Band{}",i+1),"data_type":dtype(source.raster.data_type)})).collect();
        let scheme = if self.scheme.eq_ignore_ascii_case("raster") { "raster" } else { self.scheme };
        let document = json!({
            "stac_version":"1.1.0", "type":"Feature", "id":if self.options.title.is_empty(){"tiles"}else{&self.options.title},
            "stac_extensions":["https://stac-extensions.github.io/tiled-assets/v1.0.0/schema.json"],
            "bbox":bounds, "geometry":{"type":"Polygon","coordinates":[[[bounds[0],bounds[1]],[bounds[2],bounds[1]],[bounds[2],bounds[3]],[bounds[0],bounds[3]],[bounds[0],bounds[1]]]]},
            "properties":{"datetime":null,"start_datetime":"0001-01-01T00:00:00Z","end_datetime":"9999-12-31T23:59:59Z",
                "tiles:tile_matrix_sets":{(scheme):{"type":"TileMatrixSetType","identifier":scheme,"supportedCRS":crs,"tileMatrix":matrices}},
                "tiles:tile_matrix_links":{(scheme):{"url":format!("#{scheme}"),"limits":limits}}},
            "links":[], "assets":{}, "asset_templates":{"bands":{"href":self.href(&format!("{{TileMatrix}}/{{TileCol}}/{{TileRow}}.{}",self.extension)),
                "type":if self.extension=="png"{"image/png"}else{"image/tiff; application=geotiff"},"bands":bands}}
        });
        serde_json::to_string_pretty(&document).map_err(|e| Error::InvalidData(e.to_string()))
    }

    fn mapml(&self) -> Result<String> {
        let first = &self.plan.levels[0];
        let last = self.plan.levels.last().unwrap();
        let projection = if self.scheme.eq_ignore_ascii_case("WorldCRS84Quad") { "WGS84" } else { "OSMTILE" };
        let template = self.options.mapml_template.as_deref().unwrap_or(
            "<mapml- xmlns=\"http://www.w3.org/1999/xhtml\"><map-head><map-title>${TITLE}</map-title></map-head><map-body><map-extent units=\"${TILING_SCHEME}\" checked=\"checked\"><map-input name=\"z\" type=\"zoom\" min=\"${MINZOOM}\" max=\"${MAXZOOM}\" value=\"${CURZOOM}\"/><map-input name=\"x\" type=\"location\" axis=\"column\" units=\"tilematrix\" min=\"${MINTILEX}\" max=\"${MAXTILEX}\"/><map-input name=\"y\" type=\"location\" axis=\"row\" units=\"tilematrix\" min=\"${MINTILEY}\" max=\"${MAXTILEY}\"/><map-link rel=\"tile\" tref=\"${URL}{z}/{x}/{y}.${TILEEXT}\"/></map-extent></map-body></mapml->");
        let substitutions = [
            ("TITLE", self.options.title.clone()),
            ("URL", self.href("")),
            ("TILEEXT", self.extension.into()),
            ("TILING_SCHEME", projection.into()),
            ("MINZOOM", first.matrix.zoom.to_string()),
            ("MAXZOOM", last.matrix.zoom.to_string()),
            ("CURZOOM", first.matrix.zoom.to_string()),
            ("MINTILEX", last.range.min_x.to_string()),
            ("MAXTILEX", last.range.max_x.to_string()),
            ("MINTILEY", last.range.min_y.to_string()),
            ("MAXTILEY", last.range.max_y.to_string()),
        ];
        // Substitute only the original template: replacement text may itself
        // contain a placeholder, and expansion must remain bounded in memory.
        let mut result = String::new();
        let mut remaining = template;
        while let Some(start) = remaining.find("${") {
            append_metadata(&mut result, &remaining[..start])?;
            remaining = &remaining[start..];
            let Some(end) = remaining.find('}') else { break };
            let key = &remaining[2..end];
            if let Some((_, value)) = substitutions.iter().find(|(name, _)| *name == key) {
                append_metadata(&mut result, &xml(value))?;
            } else {
                append_metadata(&mut result, &remaining[..=end])?;
            }
            remaining = &remaining[end + 1..];
        }
        append_metadata(&mut result, remaining)?;
        Ok(result)
    }

    fn leaflet(&self) -> Result<String> {
        let bounds = self.geo_bounds(self.bounds)?;
        let first = &self.plan.levels[0];
        let last = self.plan.levels.last().unwrap();
        let crs = if self.scheme.eq_ignore_ascii_case("WorldCRS84Quad") { "L.CRS.EPSG4326" } else { "L.CRS.EPSG3857" };
        let limits: Vec<_> = self.plan.levels.iter().map(|l| [l.range.min_x, l.range.min_y, l.range.max_x, l.range.max_y]).collect();
        Ok(format!(
            "{}<link rel=\"stylesheet\" href=\"https://unpkg.com/leaflet@1.9.4/dist/leaflet.css\" integrity=\"sha384-sHL9NAb7lN7rfvG5lfHpm643Xkcjzp4jFvuavGOndn6pjVqS6ny56CAt3nsEVT4H\" crossorigin=\"anonymous\"><script src=\"https://unpkg.com/leaflet@1.9.4/dist/leaflet.js\" integrity=\"sha384-cxOPjt7s7Iz04uaHJceBmS+qpjv2JkIHNVcuOrM+YHwZOmJGBXI00mdUXEq65HTH\" crossorigin=\"anonymous\"></script><script>const crs=L.extend({{}},{crs},{{scale:z=>{}*Math.pow(2,z),zoom:s=>Math.log2(s/{})}});const bounds=[[{},{}],[{},{}]];const limits={};const Tiles=L.TileLayer.extend({{getTileUrl(c){{const l=limits[c.z-{}];if(!l||c.x<l[0]||c.x>l[2]||c.y<l[1]||c.y>l[3])return L.Util.emptyImageUrl;return L.TileLayer.prototype.getTileUrl.call(this,c);}}}});const map=L.map('map',{{crs}});new Tiles({},{{minZoom:{},maxZoom:{},tileSize:{},tms:{},noWrap:true,bounds,attribution:{}}}).addTo(map);map.fitBounds(bounds);window.terrioMap=map;</script></body></html>",
            html(&self.options.title),
            first.matrix.tile_size,
            first.matrix.tile_size,
            bounds[1],
            bounds[0],
            bounds[3],
            bounds[2],
            js(&limits),
            first.matrix.zoom,
            js(&self.href(&format!("{{z}}/{{x}}/{{y}}.{}", self.extension))),
            first.matrix.zoom,
            last.matrix.zoom,
            first.matrix.tile_size,
            self.convention == Convention::Tms,
            js(&xml(&self.options.copyright)),
        ))
    }

    fn openlayers(&self) -> String {
        let first = &self.plan.levels[0];
        let tile_sizes: Vec<_> = self
            .plan
            .levels
            .iter()
            .map(|l| [l.matrix.tile_size as f64, l.matrix.tile_size as f64 * l.matrix.resolution[1] / l.matrix.resolution[0]])
            .collect();
        let resolutions: Vec<_> = self.plan.levels.iter().map(|l| l.matrix.resolution[0]).collect();
        let sizes: Vec<_> = self.plan.levels.iter().map(|l| [l.matrix.columns, l.matrix.rows]).collect();
        let origins: Vec<_> = self.plan.levels.iter().map(|l| l.matrix.origin).collect();
        let limits: Vec<_> = self.plan.levels.iter().map(|l| [l.range.min_x, l.range.min_y, l.range.max_x, l.range.max_y]).collect();
        format!(
            "{}<link rel=\"stylesheet\" href=\"https://cdn.jsdelivr.net/npm/ol@10.11.0/ol.css\" integrity=\"sha384-Jpg1uvTciFJuHq+90Ly0/2SKRT9yS5OhbIN5FuFoFWji9fc44ok8xxO88CJi8Z5W\" crossorigin=\"anonymous\"><script src=\"https://cdn.jsdelivr.net/npm/ol@10.11.0/dist/ol.js\" integrity=\"sha384-rwbCGPbgxLkcQaFvIra1Vz3KBT1yBeF2/1VgLs/gCCNCJ61GHJGimHpkufVKcqap\" crossorigin=\"anonymous\"></script><script>const grid=new ol.tilegrid.TileGrid({{resolutions:{},origins:{},sizes:{},tileSizes:{}}});const rows={};const limits={};const source=new ol.source.TileImage({{tileGrid:grid,wrapX:false,projection:new ol.proj.Projection({{code:'terrio-tiles',units:'m'}}),attributions:{},tileUrlFunction:c=>{{if(!c)return;const l=limits[c[0]];if(!l||c[1]<l[0]||c[1]>l[2]||c[2]<l[1]||c[2]>l[3])return;const z=c[0]+{};const y={} ? rows[c[0]][1]-1-c[2]:c[2];return {}.replace('{{z}}',z).replace('{{x}}',c[1]).replace('{{y}}',y);}}}});const map=new ol.Map({{target:'map',layers:[new ol.layer.Tile({{source}})],view:new ol.View({{projection:source.getProjection(),resolutions:{},center:[{},{}],zoom:0}})}});map.getView().fit({},{{size:map.getSize()}});window.terrioMap=map;</script></body></html>",
            html(&self.options.title),
            js(&resolutions),
            js(&origins),
            js(&sizes),
            js(&tile_sizes),
            js(&sizes),
            js(&limits),
            js(&xml(&self.options.copyright)),
            first.matrix.zoom,
            self.convention == Convention::Tms,
            js(&self.href(&format!("{{z}}/{{x}}/{{y}}.{}", self.extension))),
            js(&resolutions),
            (self.bounds[0] + self.bounds[2]) / 2.,
            (self.bounds[1] + self.bounds[3]) / 2.,
            js(&self.bounds)
        )
    }
}

fn level_bounds(level: &super::raster_tile::TileLevel) -> [f64; 4] {
    let m = level.matrix;
    let r = level.range;
    [
        m.origin[0] + r.min_x as f64 * m.tile_size as f64 * m.resolution[0],
        m.origin[1] - (r.max_y + 1) as f64 * m.tile_size as f64 * m.resolution[1],
        m.origin[0] + (r.max_x + 1) as f64 * m.tile_size as f64 * m.resolution[0],
        m.origin[1] - r.min_y as f64 * m.tile_size as f64 * m.resolution[1],
    ]
}
fn cell_bounds(cell: &TileCell) -> [f64; 4] {
    let g = cell.geo_transform;
    [g[0], g[3] + g[5] * cell.size[1] as f64, g[0] + g[1] * cell.size[0] as f64, g[3]]
}
fn latlon(b: [f64; 4]) -> String {
    format!("<LatLonBox><west>{}</west><south>{}</south><east>{}</east><north>{}</north></LatLonBox>", b[0], b[1], b[2], b[3])
}
fn region(bounds: [f64; 4], tile_size: usize) -> String {
    format!(
        "<Region><LatLonAltBox><west>{}</west><south>{}</south><east>{}</east><north>{}</north></LatLonAltBox><Lod><minLodPixels>{}</minLodPixels><maxLodPixels>-1</maxLodPixels></Lod></Region>",
        bounds[0],
        bounds[1],
        bounds[2],
        bounds[3],
        tile_size / 2
    )
}
fn kml(body: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><kml xmlns=\"http://www.opengis.net/kml/2.2\"><Document>{body}</Document></kml>")
}
fn html(title: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>html,body,#map{{height:100%;width:100%;margin:0}}</style></head><body><div id=\"map\"></div>",
        xml(title)
    )
}
fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}
fn js(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value)
        .unwrap()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}
fn append_metadata(output: &mut String, text: &str) -> Result<()> {
    if text.len() > MAX_METADATA.saturating_sub(output.len()) {
        return Err(invalid("publication exceeds the metadata byte budget"));
    }
    output.push_str(text);
    Ok(())
}
fn put(sink: &mut dyn FolderSink, name: &str, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_METADATA {
        return Err(invalid("publication exceeds the metadata byte budget"));
    }
    if !sink.exists(name)? {
        sink.put(name, bytes)?;
    }
    Ok(())
}
fn dtype(t: crate::DataType) -> &'static str {
    use crate::DataType::*;
    match t {
        U8 => "uint8",
        I8 => "int8",
        U16 => "uint16",
        I16 => "int16",
        U32 => "uint32",
        I32 => "int32",
        U64 => "uint64",
        I64 => "int64",
        F32 => "float32",
        F64 => "float64",
    }
}
