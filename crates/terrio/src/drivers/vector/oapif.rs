// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Written to OGC API Features Part 1 and Part 2, using GDAL 3.13.3 as a behavior
// reference. No GDAL code is translated here.

//! Read-only OGC API Features with a transport-independent request/response reader.
//! Native callers use the existing synchronous HTTP transport; WebAssembly supplies fetch.

use crate::crs::SpatialRef;
use crate::formats::VectorDriver;
use crate::{Error, FieldDef, FieldType, FieldValue, Layer, Result, VectorDataset, VectorOpenOptions};
use serde_json::{Value, json};
use std::collections::HashSet;

const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const RESPONSE_CAP: usize = 16 << 20;
// Covers retained JSON Values, the temporary serialized collection and decoder
// buffers. Per-field typed workspace is added separately by check_memory.
const JSON_MEMORY_FACTOR: usize = 32;

/// Options shared by native Rust, CLI/MCP, Python and JavaScript's fetch adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OapifOpenOptions {
    /// Requested feature page size (default 1000). A server can return smaller pages.
    pub page_size: usize,
    /// Temporal filter, in the OGC datetime parameter syntax.
    pub datetime: Option<String>,
    /// Explicit advertised CRS, or None to use storageCRS/CRS84.
    pub crs: Option<String>,
    /// Preferred advertised CRS, falling back to storageCRS/CRS84 when unavailable.
    pub preferred_crs: Option<String>,
    /// Whether non-CRS84 coordinates are already in GIS X/Y order.
    pub gis_friendly: bool,
    /// Ignore advertised JSON schemas and infer fields from all received pages.
    pub ignore_schema: bool,
    /// Conservative JSON workspace budget (default 1 GiB), including all response bytes × 32.
    pub max_vector_bytes: usize,
    /// Maximum HTTP pages, including discovery and schemas (default 10000).
    pub max_pages: usize,
    /// Service URL when the dataset name is the bare OAPIF: prefix.
    pub url: Option<String>,
    pub(crate) configured: bool,
}
impl Default for OapifOpenOptions {
    fn default() -> Self {
        Self {
            page_size: 1000,
            datetime: None,
            crs: None,
            preferred_crs: None,
            gis_friendly: false,
            ignore_schema: false,
            max_vector_bytes: 1 << 30,
            max_pages: 10000,
            url: None,
            configured: false,
        }
    }
}
impl OapifOpenOptions {
    /// GDAL-style PAGE_SIZE, DATETIME, CRS, PREFERRED_CRS, SERVER_FEATURE_AXIS_ORDER,
    /// IGNORE_SCHEMA and URL; OAPIF_MAX_VECTOR_BYTES and OAPIF_MAX_PAGES set budgets.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        let positive = || {
            value
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| Error::InvalidArgument(format!("{key} must be a positive integer")))
        };
        match key.to_ascii_uppercase().as_str() {
            "PAGE_SIZE" | "OAPIF_PAGE_SIZE" => self.page_size = positive()?,
            "DATETIME" => self.datetime = Some(value.into()),
            "CRS" => self.crs = Some(value.into()),
            "PREFERRED_CRS" => self.preferred_crs = Some(value.into()),
            "IGNORE_SCHEMA" => self.ignore_schema = crate::tiles::parse_bool(value)?,
            "SERVER_FEATURE_AXIS_ORDER" => {
                self.gis_friendly = match value.to_ascii_uppercase().as_str() {
                    "GIS_FRIENDLY" => true,
                    "AUTHORITY_COMPLIANT" => false,
                    _ => {
                        return Err(Error::InvalidArgument("SERVER_FEATURE_AXIS_ORDER must be GIS_FRIENDLY or AUTHORITY_COMPLIANT".into()));
                    }
                }
            }
            "OAPIF_MAX_VECTOR_BYTES" => self.max_vector_bytes = positive()?,
            "OAPIF_MAX_PAGES" => self.max_pages = positive()?,
            "URL" => self.url = Some(value.into()),
            _ => return Ok(false),
        }
        self.configured = true;
        Ok(true)
    }
    pub(crate) fn validate_source(&self, path: &str) -> Result<()> {
        if service_target(path).is_none() && self != &Self::default() {
            return Err(Error::InvalidArgument("OAPIF open options require an OAPIF: source".into()));
        }
        Ok(())
    }
}

#[cfg(not(remote))]
pub(crate) fn transport_required() -> Error {
    if cfg!(target_arch = "wasm32") {
        Error::Unsupported("OAPIF in WebAssembly requires openOapif or a transport for oapif::Reader".into())
    } else {
        Error::disabled("remote")
    }
}

/// The public metadata of one collection, without requesting its features.
#[derive(Debug, Clone)]
pub struct Collection {
    /// Stable collection identifier, also its layer name.
    pub id: String,
    /// Advertised human-readable title.
    pub title: Option<String>,
    /// CRS of coordinates returned by this reader, normalized to GIS X/Y order.
    pub crs: SpatialRef,
    /// Advertised extent in CRS84, when available.
    pub wgs84_extent: Option<[f64; 4]>,
    items: String,
    schema: Option<String>,
    crs_uri: String,
    swap: bool,
}

/// One bounded GET request. Call [`Reader::accept_response`] once its body arrives.
#[derive(Debug, Clone)]
pub struct Request {
    /// Request URL. It may contain caller-supplied authentication; do not log it.
    pub url: String,
    /// HTTP Accept header for JSON content negotiation.
    pub accept: &'static str,
    /// Maximum response bytes. Transports should enforce this while streaming.
    pub byte_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Phase {
    Discover,
    Schema,
    Items,
    Done,
}

/// Portable OAPIF reader. Requests are sequential and pagination follows rel=next links.
/// Same-origin links/redirects, response counts and JSON workspace are bounded.
/// Numeric/string feature IDs and every observed field are retained across pages.
pub struct Reader {
    options: VectorOpenOptions,
    origin: (String, String),
    initial: String,
    phase: Phase,
    request: Option<Request>,
    visited: HashSet<(usize, Phase, String)>,
    requests: usize,
    bytes: usize,
    metadata_only: bool,
    collections: Vec<Collection>,
    index: usize,
    features: Vec<Value>,
    ids: HashSet<String>,
    previous_page_len: usize,
    fields: Vec<FieldDef>,
    layers: Vec<Layer>,
}
impl Reader {
    /// Opens a landing page, collection listing, individual collection, or items URL.
    pub fn new(url: &str, options: &VectorOpenOptions) -> Result<Self> {
        Self::start(url, options, false)
    }
    /// Discovers collections without fetching schema or feature pages.
    pub fn metadata(url: &str, options: &VectorOpenOptions) -> Result<Self> {
        Self::start(url, options, true)
    }
    fn start(url: &str, options: &VectorOpenOptions, metadata_only: bool) -> Result<Self> {
        let url = service_target(url).unwrap_or(url);
        let url = if url.is_empty() { options.oapif.url.as_deref().unwrap_or("") } else { url };
        let origin = crate::vfs::url_origin(url)
            .filter(|(scheme, host)| matches!(scheme.as_str(), "http" | "https") && !host.is_empty())
            .ok_or_else(|| Error::InvalidArgument("OAPIF requires an HTTP(S) service URL".into()))?;
        crate::formats::check_driver_allowed("OAPIF", "OAPIF:", crate::formats::Kind::Vector)?;
        let o = &options.oapif;
        if o.page_size == 0 || o.max_pages == 0 || o.max_vector_bytes < JSON_MEMORY_FACTOR {
            return Err(Error::InvalidArgument("OAPIF page size, page budget and JSON workspace budget must be positive".into()));
        }
        if o.crs.is_some() && o.preferred_crs.is_some() {
            return Err(Error::InvalidArgument("CRS and PREFERRED_CRS are mutually exclusive".into()));
        }
        if options.spatial_filter.is_some_and(|(a, b, c, d)| [a, b, c, d].iter().any(|n| !n.is_finite()) || a > c || b > d) {
            return Err(Error::InvalidArgument("OAPIF bbox must be finite with ordered bounds".into()));
        }
        let mut reader = Self {
            options: options.clone(),
            origin,
            initial: url.into(),
            phase: Phase::Discover,
            request: None,
            visited: HashSet::new(),
            requests: 0,
            bytes: 0,
            metadata_only,
            collections: Vec::new(),
            index: 0,
            features: Vec::new(),
            ids: HashSet::new(),
            previous_page_len: 0,
            fields: Vec::new(),
            layers: Vec::new(),
        };
        let (path, query) = split_url(url);
        let first = if path.trim_end_matches('/').ends_with("/items") {
            let parent = path.trim_end_matches('/').trim_end_matches("/items");
            format!("{parent}{}", query.map(|q| format!("?{q}")).unwrap_or_default())
        } else {
            url.into()
        };
        reader.schedule(first, Phase::Discover)?;
        Ok(reader)
    }
    /// The next request, or None when the complete result is ready.
    pub fn next_request(&self) -> Option<&Request> {
        self.request.as_ref()
    }
    /// Discovered collection metadata; complete once metadata discovery finishes.
    pub fn collections(&self) -> &[Collection] {
        &self.collections
    }
    /// Parses one response and advances the request state. Resource refusals are terminal.
    pub fn accept_response(&mut self, body: &[u8]) -> Result<()> {
        let request = self.request.take().ok_or_else(|| invalid("unexpected response after completion"))?;
        if body.len() > request.byte_limit {
            return Err(limited("response exceeds its byte budget"));
        }
        self.bytes = self.bytes.checked_add(body.len()).ok_or_else(|| limited("response byte counter overflow"))?;
        self.check_memory(0)?;
        let value: Value = serde_json::from_slice(body).map_err(|_| invalid("response is not valid JSON"))?;
        if !value.is_object() {
            return Err(invalid("response must be a JSON object"));
        }
        match self.phase {
            Phase::Discover => self.discover(&value, &request.url),
            Phase::Schema => {
                self.fields = schema_fields(&value)?;
                self.begin_items()
            }
            Phase::Items => self.items(value, &request.url),
            Phase::Done => Err(invalid("unexpected response after completion")),
        }
    }
    /// Accepts a response after a same-origin redirect, resolving relative links against
    /// the effective URL. Use `accept_response` for transports that refuse redirects.
    pub fn accept_response_from(&mut self, body: &[u8], effective_url: &str) -> Result<()> {
        let request = self.request.as_mut().ok_or_else(|| invalid("unexpected response after completion"))?;
        if request.url != effective_url {
            if crate::vfs::url_origin(effective_url).as_ref() != Some(&self.origin) {
                return Err(invalid("cross-origin redirect is unsupported"));
            }
            if !self.visited.insert((self.index, self.phase, effective_url.into())) {
                return Err(invalid("redirect returns to a previously read service page"));
            }
            request.url = effective_url.into();
        }
        self.accept_response(body)
    }

    /// Returns the completed vector dataset. Metadata readers return no materialized layers.
    pub fn finish(self) -> Result<VectorDataset> {
        if self.phase != Phase::Done || self.request.is_some() {
            return Err(invalid("service reader has pending requests"));
        }
        Ok(VectorDataset::new(self.layers, VectorDriver::OAPIF))
    }
    fn check_memory(&self, cells: usize) -> Result<()> {
        let estimate = self
            .bytes
            .saturating_mul(JSON_MEMORY_FACTOR)
            .saturating_add(cells.saturating_mul(std::mem::size_of::<FieldValue>()).saturating_mul(2));
        if estimate > self.options.oapif.max_vector_bytes {
            return Err(limited("JSON workspace exceeds OAPIF_MAX_VECTOR_BYTES"));
        }
        Ok(())
    }
    fn schedule(&mut self, url: String, phase: Phase) -> Result<()> {
        if crate::vfs::url_origin(&url).as_ref() != Some(&self.origin) {
            return Err(invalid("cross-origin service links are unsupported; open the linked service explicitly"));
        }
        if !self.visited.insert((self.index, phase, url.clone())) {
            return Err(invalid("service links form a pagination/discovery cycle"));
        }
        self.requests += 1;
        if self.requests > self.options.oapif.max_pages {
            return Err(limited("service exceeds OAPIF_MAX_PAGES"));
        }
        let remaining = self.options.oapif.max_vector_bytes / JSON_MEMORY_FACTOR - self.bytes;
        if remaining == 0 {
            return Err(limited("JSON workspace exceeds OAPIF_MAX_VECTOR_BYTES"));
        }
        self.phase = phase;
        self.request = Some(Request {
            url,
            accept: if phase == Phase::Items { "application/geo+json, application/json" } else { "application/json" },
            byte_limit: remaining.min(RESPONSE_CAP),
        });
        Ok(())
    }
    fn follow(&self, base: &str, href: &str) -> String {
        let resolved = resolve(base, href);
        // Preserve caller-supplied query selectors/authentication on this origin, without
        // overriding explicit links or leaking them to another origin.
        if crate::vfs::url_origin(&resolved).as_ref() != Some(&self.origin) {
            return resolved;
        }
        let mut resolved = resolved;
        if let Some(query) = split_url(&self.initial).1 {
            for pair in query.split('&').filter(|s| !s.is_empty()) {
                let key = crate::vfs::decode_query_component(pair.split('=').next().unwrap_or(""));
                if ["limit", "offset", "bbox", "datetime", "crs", "bbox-crs"].iter().any(|k| key.eq_ignore_ascii_case(k)) {
                    continue;
                }
                if !has_query(&resolved, &key) {
                    resolved.push(if resolved.contains('?') { '&' } else { '?' });
                    resolved.push_str(pair);
                }
            }
        }
        resolved
    }
    fn discover(&mut self, value: &Value, source: &str) -> Result<()> {
        if let Some(collections) = value.get("collections").and_then(Value::as_array) {
            for entry in collections {
                self.add_collection(entry, source, true)?;
            }
            if let Some(href) = link(value, "next", false) {
                return self.schedule(self.follow(source, href), Phase::Discover);
            }
        } else if let Some(href) = link(value, "data", false) {
            return self.schedule(self.follow(source, href), Phase::Discover);
        } else if value.get("id").is_some() {
            self.add_collection(value, source, false)?;
        } else if self.requests == 1 {
            return self.schedule(append_path(source, "collections"), Phase::Discover);
        } else {
            return Err(invalid("response has no OGC API collection metadata"));
        }
        for wanted in &self.options.layers {
            if !self.collections.iter().any(|c| c.id == *wanted) {
                return Err(invalid(format!("collection '{wanted}' was not advertised")));
            }
        }
        if !self.options.layers.is_empty() {
            self.collections.retain(|c| self.options.layers.contains(&c.id));
        }
        if self.metadata_only {
            self.phase = Phase::Done;
            return Ok(());
        }
        self.begin_collection()
    }
    fn add_collection(&mut self, value: &Value, source: &str, listing: bool) -> Result<()> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("collection has no nonempty string id"))?;
        if self.collections.iter().any(|c| c.id == id) {
            return Err(invalid("collection listing repeats an identifier"));
        }
        let base = if listing { append_path(source, &encode(id)) } else { source.into() };
        let items = link(value, "items", true).map(|href| self.follow(source, href)).unwrap_or_else(|| append_path(&base, "items"));
        let schema = if self.options.oapif.ignore_schema { None } else { schema_link(value).map(|href| self.follow(source, href)) };
        let (crs_uri, crs, swap) = select_crs(value, &self.options.oapif)?;
        let bbox = value.pointer("/extent/spatial/bbox/0").and_then(Value::as_array);
        let extent_crs = value.pointer("/extent/spatial/crs").and_then(Value::as_str).unwrap_or(CRS84);
        let wgs84_extent = if is_crs84(extent_crs) {
            bbox.and_then(|b| {
                let indexes = match b.len() {
                    4 => [0, 1, 2, 3],
                    6 => [0, 1, 3, 4],
                    _ => return None,
                };
                let a = indexes.map(|i| b[i].as_f64());
                Some([a[0]?, a[1]?, a[2]?, a[3]?])
            })
            .filter(|b| b.iter().all(|v| v.is_finite()) && b[0] <= b[2] && b[1] <= b[3])
        } else {
            None
        };
        self.collections.push(Collection {
            id: id.into(),
            title: value.get("title").and_then(Value::as_str).map(str::to_owned),
            crs,
            wgs84_extent,
            items,
            schema,
            crs_uri,
            swap,
        });
        Ok(())
    }
    fn begin_collection(&mut self) -> Result<()> {
        if self.index == self.collections.len() {
            self.phase = Phase::Done;
            return Ok(());
        }
        self.features.clear();
        self.ids.clear();
        self.fields.clear();
        self.previous_page_len = 0;
        if let Some(url) = self.collections[self.index].schema.clone() { self.schedule(url, Phase::Schema) } else { self.begin_items() }
    }
    fn begin_items(&mut self) -> Result<()> {
        if self.options.max_features == Some(0) {
            return self.end_collection();
        }
        let c = &self.collections[self.index];
        let mut url = c.items.clone();
        if let Some(query) = split_url(&self.initial).1 {
            for pair in query.split('&') {
                if let Some((key, value)) = pair.split_once('=') {
                    let key = crate::vfs::decode_query_component(key);
                    if ["bbox", "datetime", "crs", "bbox-crs"].contains(&key.as_ref()) {
                        url = query_value(&url, &key, &crate::vfs::decode_query_component(value));
                    }
                }
            }
        }
        let stop = self.options.max_features.map(|n| n.saturating_add(self.options.skip_features));
        let limit = stop.map_or(self.options.oapif.page_size, |n| n.min(self.options.oapif.page_size));
        url = query_value(&url, "limit", &limit.to_string());
        if !is_crs84(&c.crs_uri) {
            url = query_value(&url, "crs", &c.crs_uri);
        }
        if let Some(datetime) = &self.options.oapif.datetime {
            url = query_value(&url, "datetime", datetime);
        }
        if let Some((a, b, d, e)) = self.options.spatial_filter {
            let bounds = if c.swap { [b, a, e, d] } else { [a, b, d, e] };
            url = query_value(&url, "bbox", &bounds.iter().map(f64::to_string).collect::<Vec<_>>().join(","));
            if !is_crs84(&c.crs_uri) {
                url = query_value(&url, "bbox-crs", &c.crs_uri);
            }
        }
        self.schedule(url, Phase::Items)
    }
    fn items(&mut self, mut value: Value, source: &str) -> Result<()> {
        if value.get("type").and_then(Value::as_str) != Some("FeatureCollection") {
            return Err(invalid("items response is not a GeoJSON FeatureCollection"));
        }
        let next = link(&value, "next", true).map(|href| self.follow(source, href));
        let matched = value.get("numberMatched").and_then(Value::as_u64);
        let returned = value.get("numberReturned").and_then(Value::as_u64);
        let page =
            value.get_mut("features").and_then(Value::as_array_mut).ok_or_else(|| invalid("items response has no features array"))?;
        if returned.is_some_and(|n| n != page.len() as u64) {
            return Err(invalid("numberReturned differs from the page feature count"));
        }
        if next.is_some() && page.is_empty() {
            return Err(invalid("empty feature page advertises another page"));
        }
        if self.previous_page_len > 0
            && self.previous_page_len == page.len()
            && self.features[self.features.len() - self.previous_page_len..] == page[..]
        {
            return Err(invalid("service repeats a feature page instead of advancing pagination"));
        }
        for feature in page.iter() {
            if feature.get("type").and_then(Value::as_str) != Some("Feature") {
                return Err(invalid("items array contains a non-Feature value"));
            }
            if let Some(id) = feature.get("id").filter(|id| !id.is_null()) {
                if !(id.is_string() || id.is_i64() || id.is_u64()) {
                    return Err(invalid("feature id must be a string or integer"));
                }
                if !self.ids.insert(id.to_string()) {
                    return Err(invalid("service repeats a feature identifier across pages"));
                }
            }
        }
        self.previous_page_len = page.len();
        self.features.append(page);
        let stop = self.options.max_features.map(|n| n.saturating_add(self.options.skip_features));
        if stop.is_some_and(|n| self.features.len() >= n) {
            return self.end_collection();
        }
        if let Some(next) = next {
            return self.schedule(next, Phase::Items);
        }
        if matched.is_some_and(|n| n > self.features.len() as u64) {
            return Err(invalid("service reports more matching features without a next link"));
        }
        self.end_collection()
    }
    fn end_collection(&mut self) -> Result<()> {
        let c = &self.collections[self.index];
        let mut fields: HashSet<String> = self.fields.iter().map(|f| f.name.clone()).collect();
        for feature in &self.features {
            if let Some(properties) = feature.get("properties").and_then(Value::as_object) {
                fields.extend(properties.keys().cloned());
            }
        }
        self.check_memory(self.features.len().saturating_mul(fields.len().saturating_add(1)))?;
        // GeoJSON's shared decoder flattens nested properties. Preserve their complete
        // JSON value as text here, so the advertised top-level schema remains meaningful.
        for feature in &mut self.features {
            if let Some(properties) = feature.get_mut("properties").and_then(Value::as_object_mut) {
                for value in properties.values_mut() {
                    if value.is_array() || value.is_object() {
                        *value = Value::String(value.to_string());
                    }
                }
            }
        }
        // The core model has unsigned FIDs, and its GeoJSON decoder uses a leading
        // id property for string IDs. Preserve signed IDs and conflicting properties
        // separately instead of silently losing either identifier or property data.
        let mixed_ids = self.features.iter().any(|f| f["id"].is_string()) && self.features.iter().any(|f| f["id"].is_number());
        let separate_ids = mixed_ids
            || self.features.iter().any(|f| {
                let id = &f["id"];
                id.as_i64().is_some_and(|n| n < 0)
                    || (id.is_string() && f.get("properties").and_then(|p| p.get("id")).is_some_and(|property| property != id))
            });
        if separate_ids {
            let mut name = "_oapif_id".to_owned();
            while fields.contains(name.as_str()) {
                name.insert(0, '_');
            }
            for feature in &mut self.features {
                if let Some(id) = feature.as_object_mut().and_then(|f| f.remove("id")) {
                    let text = if mixed_ids { id.to_string() } else { id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string()) };
                    let properties = feature.as_object_mut().unwrap().entry("properties").or_insert_with(|| json!({}));
                    if !properties.is_object() {
                        *properties = json!({});
                    }
                    properties.as_object_mut().unwrap().insert(name.clone(), Value::String(text));
                }
            }
        }
        let doc = json!({"type":"FeatureCollection", "name":c.id, "features":std::mem::take(&mut self.features)});
        let bytes = serde_json::to_vec(&doc).map_err(|_| limited("cannot assemble feature pages"))?;
        let mut layer = super::geojson::parse(&bytes, &c.id)?;
        apply_schema(&mut layer, &self.fields)?;
        c.crs.apply_to_layer(&mut layer);
        if c.swap {
            for feature in &mut layer.features {
                if let Some(g) = &mut feature.geometry {
                    crate::vector::map_coords(g, &mut |coord| {
                        std::mem::swap(&mut coord.x, &mut coord.y);
                        Ok(())
                    })?;
                }
            }
            layer.extent = None;
        }
        self.options.keep_range(&mut layer);
        self.layers.push(layer);
        self.index += 1;
        self.begin_collection()
    }
}

/// Recognizes only the explicit service prefix; ordinary GeoJSON URLs keep their I/O behavior.
pub(crate) fn service_target(path: &str) -> Option<&str> {
    path.get(..6).filter(|s| s.eq_ignore_ascii_case("OAPIF:")).map(|_| &path[6..])
}
fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("OAPIF: {message}"))
}
fn limited(message: impl std::fmt::Display) -> Error {
    Error::ResourceLimit(format!("OAPIF: {message}"))
}
fn is_crs84(uri: &str) -> bool {
    uri.eq_ignore_ascii_case(CRS84) || uri.eq_ignore_ascii_case("OGC:CRS84")
}
fn split_url(url: &str) -> (&str, Option<&str>) {
    let url = url.split('#').next().unwrap_or(url);
    match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    }
}
fn resolve(base: &str, href: &str) -> String {
    if href.is_empty() {
        return base.to_string();
    }
    if href.starts_with('?') {
        return format!("{}{href}", split_url(base).0);
    }
    if href.starts_with("//") {
        return format!("{}:{href}", base.split(':').next().unwrap_or("https"));
    }
    crate::stac::resolve_href(base, href)
}
fn append_path(base: &str, segment: &str) -> String {
    let (path, query) = split_url(base);
    format!("{}/{}{}", path.trim_end_matches('/'), segment, query.map(|q| format!("?{q}")).unwrap_or_default())
}
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
fn has_query(url: &str, name: &str) -> bool {
    split_url(url).1.is_some_and(|q| {
        q.split('&').any(|p| crate::vfs::decode_query_component(p.split('=').next().unwrap_or("")).eq_ignore_ascii_case(name))
    })
}
fn query_value(url: &str, name: &str, value: &str) -> String {
    let (path, query) = split_url(url);
    let mut pairs: Vec<String> = query
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty() && !crate::vfs::decode_query_component(p.split('=').next().unwrap_or("")).eq_ignore_ascii_case(name))
        .map(str::to_owned)
        .collect();
    pairs.push(format!("{}={}", encode(name), encode(value)));
    format!("{path}?{}", pairs.join("&"))
}
fn link<'a>(value: &'a Value, rel: &str, geojson: bool) -> Option<&'a str> {
    let links = value.get("links")?.as_array()?;
    let matches = |l: &&Value| {
        l.get("rel").and_then(Value::as_str).is_some_and(|r| r == rel || r == format!("http://www.opengis.net/def/rel/ogc/1.0/{rel}"))
    };
    let preferred = if geojson { "application/geo+json" } else { "application/json" };
    links
        .iter()
        .filter(matches)
        .find(|l| {
            l.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case(preferred)))
        })
        .or_else(|| {
            links
                .iter()
                .filter(matches)
                .find(|l| l.get("type").and_then(Value::as_str).is_none_or(|t| t.to_ascii_lowercase().contains("json")))
        })
        .and_then(|l| l.get("href").and_then(Value::as_str))
}
fn schema_link(value: &Value) -> Option<&str> {
    value
        .get("links")?
        .as_array()?
        .iter()
        .find(|l| {
            l.get("rel").and_then(Value::as_str).is_some_and(|r| {
                r == "describedby"
                    || r == "http://www.opengis.net/def/rel/ogc/1.0/schema"
                    || r == "https://www.opengis.net/def/rel/ogc/1.0/schema"
            }) && l.get("type").and_then(Value::as_str).is_some_and(|t| t.to_ascii_lowercase().contains("json"))
        })?
        .get("href")?
        .as_str()
}
fn select_crs(value: &Value, options: &OapifOpenOptions) -> Result<(String, SpatialRef, bool)> {
    let supported: Vec<&str> = value.get("crs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let available = |asked: &str| -> Option<&str> {
        if is_crs84(asked) {
            return Some(CRS84);
        }
        let crs = SpatialRef::parse(asked).ok()?;
        supported
            .iter()
            .copied()
            .find(|s| !is_crs84(s) && SpatialRef::parse(s).is_ok_and(|c| c.epsg == crs.epsg && c.esri == crs.esri && c.same_as(&crs)))
    };
    let requested = if let Some(asked) = &options.crs {
        Some(available(asked).ok_or_else(|| invalid("requested CRS is not advertised by the collection"))?)
    } else {
        options.preferred_crs.as_deref().and_then(available)
    };
    let uri = requested.or_else(|| value.get("storageCrs").or_else(|| value.get("storageCRS")).and_then(Value::as_str)).unwrap_or(CRS84);
    let crs = SpatialRef::parse(uri).map_err(|_| invalid("collection uses an unsupported CRS"))?;
    let swap = !is_crs84(uri) && !options.gis_friendly && north_first(&crs)?;
    Ok((uri.into(), crs, swap))
}
fn north_first(crs: &SpatialRef) -> Result<bool> {
    let wkt = crs.to_wkt().ok_or_else(|| invalid("cannot determine the authority axis order"))?;
    match first_axis_direction(&wkt) {
        Some(direction) if direction.eq_ignore_ascii_case("north") => Ok(true),
        Some(direction) if direction.eq_ignore_ascii_case("east") => Ok(false),
        Some(_) => Err(invalid("authority axes other than north/east are unsupported")),
        None => Ok(crs.is_geographic()),
    }
}
/// Root coordinate-system axis, ignoring commas/brackets inside quoted names.
fn first_axis_direction(wkt: &str) -> Option<&str> {
    let bytes = wkt.as_bytes();
    let (mut depth, mut quoted) = (0usize, false);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' {
            quoted = !quoted;
        }
        if quoted {
            continue;
        }
        if depth == 1 && bytes.get(i..i + 5).is_some_and(|word| word.eq_ignore_ascii_case(b"AXIS[")) {
            let tail = &bytes[i + 5..];
            let mut in_name = false;
            for (offset, &b) in tail.iter().enumerate() {
                if b == b'"' {
                    in_name = !in_name;
                }
                if !in_name && b == b',' {
                    let direction = &tail[offset + 1..];
                    let end = direction.iter().position(|b| matches!(b, b',' | b']' | b'[')).unwrap_or(direction.len());
                    return std::str::from_utf8(&direction[..end]).ok().map(str::trim);
                }
                if !in_name && b == b']' {
                    return None;
                }
            }
            return None;
        }
        if b == b'[' {
            depth += 1;
        } else if b == b']' {
            depth = depth.saturating_sub(1);
        }
    }
    None
}

fn schema_fields(root: &Value) -> Result<Vec<FieldDef>> {
    let document = root;
    let root = resolve_ref(document, root, 0)?;
    let Some(properties) = root.get("properties").and_then(Value::as_object) else { return Ok(Vec::new()) };
    let properties = if let Some(feature_properties) = properties.get("properties") {
        resolve_ref(document, feature_properties, 0)?.get("properties").and_then(Value::as_object).unwrap_or(properties)
    } else {
        properties
    };
    properties
        .iter()
        .filter(|(_, value)| value.get("x-ogc-role").and_then(Value::as_str) != Some("primary-geometry"))
        .map(|(name, value)| {
            let v = resolve_ref(document, value, 0)?;
            let kind =
                v.get("type").and_then(|t| t.as_str().or_else(|| t.as_array()?.iter().filter_map(Value::as_str).find(|s| *s != "null")));
            let field_type = match (kind, v.get("format").and_then(Value::as_str)) {
                (Some("integer"), _) => FieldType::Integer,
                (Some("number"), _) => FieldType::Float,
                (Some("boolean"), _) => FieldType::Boolean,
                (Some("string"), Some("date")) => FieldType::Date,
                (Some("string"), Some("date-time")) => FieldType::DateTime,
                _ => FieldType::Text,
            };
            Ok(FieldDef::new(name, field_type))
        })
        .collect()
}
fn resolve_ref<'a>(root: &'a Value, value: &'a Value, depth: usize) -> Result<&'a Value> {
    if depth > 32 {
        return Err(invalid("JSON schema references are recursive or too deep"));
    }
    let Some(reference) = value.get("$ref").and_then(Value::as_str) else { return Ok(value) };
    let path =
        reference.strip_prefix('#').ok_or_else(|| invalid("external JSON schema references are unsupported; use IGNORE_SCHEMA=YES"))?;
    resolve_ref(root, root.pointer(path).ok_or_else(|| invalid("JSON schema reference does not exist"))?, depth + 1)
}
fn apply_schema(layer: &mut Layer, declared: &[FieldDef]) -> Result<()> {
    for field in declared {
        if let Some(index) = layer.schema.fields().iter().position(|f| f.name == field.name) {
            let old_type = layer.schema.fields()[index].field_type;
            if old_type != field.field_type {
                for feature in &mut layer.features {
                    let value = &mut feature.attributes[index];
                    match (&*value, field.field_type) {
                        (FieldValue::Null, _) => {}
                        (FieldValue::Integer(v), FieldType::Float) => *value = FieldValue::Float(*v as f64),
                        (FieldValue::Date(v) | FieldValue::DateTime(v), FieldType::Text) => *value = FieldValue::Text(v.clone()),
                        _ => return Err(invalid("feature property disagrees with its advertised JSON schema")),
                    }
                }
                let mut fields = layer.schema.fields().to_vec();
                fields[index] = field.clone();
                let mut schema = wbvector::Schema::new();
                for field in fields {
                    schema.add_field(field);
                }
                layer.schema = schema;
            }
        } else {
            layer.schema.add_field(field.clone());
            for feature in &mut layer.features {
                feature.attributes.push(FieldValue::Null);
            }
        }
    }
    Ok(())
}

#[cfg(remote)]
fn drive(mut reader: Reader) -> Result<Reader> {
    while let Some(request) = reader.next_request() {
        let response = crate::http::get_json_response(&request.url, request.accept, request.byte_limit as u64)?;
        reader.accept_response_from(&response.body, &response.url)?;
    }
    Ok(reader)
}

#[cfg(remote)]
pub(crate) fn open(url: &str, options: &VectorOpenOptions) -> Result<VectorDataset> {
    drive(Reader::new(url, options)?)?.finish()
}
#[cfg(remote)]
pub(crate) fn info(url: &str) -> Result<super::wfs::ServiceInfo> {
    let reader = drive(Reader::metadata(url, &Default::default())?)?;
    Ok(super::wfs::ServiceInfo {
        driver: "OAPIF".into(),
        version: "1.0".into(),
        metadata_only: true,
        layers: reader
            .collections
            .iter()
            .map(|c| super::wfs::ServiceLayerInfo {
                name: c.id.clone(),
                title: c.title.clone(),
                geometry_type: "Unknown".into(),
                feature_count: None,
                crs: crate::info::CrsSummary::new(&c.crs),
                extent: None,
                wgs84_extent: c.wgs84_extent,
                fields: Vec::new(),
            })
            .collect(),
    })
}
#[cfg(remote)]
pub(crate) fn list_layers(url: &str) -> Result<Vec<String>> {
    Ok(drive(Reader::metadata(url, &Default::default())?)?.collections.into_iter().map(|c| c.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn accept(reader: &mut Reader, value: Value) {
        reader.accept_response(&serde_json::to_vec(&value).unwrap()).unwrap();
    }
    fn collection(id: &str) -> Value {
        json!({"id":id,"title":"A collection","links":[{"rel":"items","type":"application/geo+json","href":format!("/collections/{id}/items")} ]})
    }
    fn feature(id: Value, properties: Value) -> Value {
        json!({"type":"Feature","id":id,"properties":properties,"geometry":{"type":"Point","coordinates":[8.,52.,3.]}})
    }
    fn page(features: Vec<Value>, next: Option<&str>) -> Value {
        json!({"type":"FeatureCollection","features":features,"links":next.map(|href|vec![json!({"rel":"next","href":href})]).unwrap_or_default()})
    }
    fn at_items(options: &VectorOpenOptions) -> Reader {
        let mut r = Reader::new("https://example.test/collections/a", options).unwrap();
        accept(&mut r, collection("a"));
        r
    }
    #[test]
    fn landing_discovery_pagination_and_field_union_preserve_identity() {
        let mut r = Reader::new("OAPIF:https://example.test/api?key=private", &Default::default()).unwrap();
        accept(&mut r, json!({"links":[{"rel":"data","href":"/collections","type":"application/json"}]}));
        assert_eq!(r.next_request().unwrap().url, "https://example.test/collections?key=private");
        accept(&mut r, json!({"collections":[collection("a")]}));
        assert!(r.next_request().unwrap().url.contains("limit=1000"));
        accept(&mut r, page(vec![feature(json!("a-1"), json!({"rank":1}))], Some("?offset=1")));
        assert!(r.next_request().unwrap().url.contains("offset=1&key=private"));
        accept(&mut r, page(vec![feature(json!("a-2"), json!({"rank":2.5,"later":true}))], None));
        let result = r.finish().unwrap();
        let layer = &result.layers[0];
        assert_eq!(result.driver, VectorDriver::OAPIF);
        assert_eq!(layer.name, "a");
        assert_eq!(layer.crs_epsg(), Some(4326));
        assert_eq!(layer.features[0].get(&layer.schema, "id").ok(), Some(&FieldValue::Text("a-1".into())));
        assert_eq!(layer.features[0].get(&layer.schema, "rank").ok(), Some(&FieldValue::Float(1.)));
        assert_eq!(layer.features[0].get(&layer.schema, "later").ok(), Some(&FieldValue::Null));
        let Some(crate::Geometry::Point(p)) = &layer.features[1].geometry else { panic!() };
        assert_eq!((p.x, p.y, p.z), (8., 52., Some(3.)));
    }
    #[test]
    fn schema_local_references_and_missing_properties_are_typed() {
        let mut r = Reader::new("https://example.test/collections/a", &Default::default()).unwrap();
        let mut c = collection("a");
        c["links"].as_array_mut().unwrap().push(json!({"rel":"describedby","type":"application/schema+json","href":"/schema"}));
        accept(&mut r, c);
        accept(
            &mut r,
            json!({"$defs":{"props":{"type":"object","properties":{"v":{"type":["number","null"]},"missing":{"type":"integer"}}}},
            "type":"object","properties":{"properties":{"$ref":"#/$defs/props"}}}),
        );
        accept(&mut r, page(vec![feature(json!(7), json!({"v":1}))], None));
        let layer = r.finish().unwrap().layers.remove(0);
        assert_eq!(layer.features[0].fid, 7);
        assert_eq!(layer.features[0].get(&layer.schema, "v").ok(), Some(&FieldValue::Float(1.)));
        assert_eq!(layer.features[0].get(&layer.schema, "missing").ok(), Some(&FieldValue::Null));
        assert_eq!(layer.schema.fields().iter().find(|f| f.name == "missing").unwrap().field_type, FieldType::Integer);
    }
    #[test]
    fn nested_properties_and_conflicting_or_signed_ids_are_lossless() {
        for id in [json!(-7), json!("source-key")] {
            let mut r = at_items(&Default::default());
            accept(&mut r, page(vec![feature(id.clone(), json!({"id":42,"links":[{"href":"a","other":true}]}))], None));
            let layer = r.finish().unwrap().layers.remove(0);
            let f = &layer.features[0];
            assert_eq!(f.get(&layer.schema, "id").unwrap(), &FieldValue::Integer(42));
            let expected = id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string());
            assert_eq!(f.get(&layer.schema, "_oapif_id").unwrap(), &FieldValue::Text(expected));
            let FieldValue::Text(text) = f.get(&layer.schema, "links").unwrap() else { panic!() };
            assert_eq!(serde_json::from_str::<Value>(text).unwrap(), json!([{"href":"a","other":true}]));
        }
    }
    #[test]
    fn unusable_schema_falls_back_and_nonconforming_values_fail_explicitly() {
        assert!(schema_fields(&json!({"type":"object"})).unwrap().is_empty());
        let scalar = schema_fields(&json!({"properties":{"properties":{"type":"string"}}})).unwrap();
        assert_eq!(scalar.len(), 1);
        assert_eq!(scalar[0].name, "properties");
        let mut r = at_items(&Default::default());
        r.fields = vec![FieldDef::new("when", FieldType::Date)];
        let err = r.accept_response(&serde_json::to_vec(&page(vec![feature(json!(1), json!({"when":""}))], None)).unwrap()).unwrap_err();
        assert!(err.to_string().contains("advertised JSON schema"));
        let mut o = VectorOpenOptions::default();
        o.oapif.ignore_schema = true;
        let mut r = at_items(&o);
        accept(&mut r, page(vec![feature(json!(1), json!({"when":""}))], None));
        let l = r.finish().unwrap().layers.remove(0);
        assert_eq!(l.features[0].get(&l.schema, "when").unwrap(), &FieldValue::Text(String::new()));
    }
    #[test]
    fn axis_names_with_unicode_commas_and_escaped_quotes_are_safe() {
        assert_eq!(first_axis_direction(r#"GEOGCS["é",AXIS["north, ""quoted"" name",NORTH]]"#), Some("NORTH"));
        assert_eq!(first_axis_direction(r#"GEOGCS[é,axis["a, b",east]]"#), Some("east"));
        assert_eq!(first_axis_direction(r#"PROJCS["x",GEOGCS["y",AXIS["latitude",NORTH]],AXIS["east",EAST]]"#), Some("EAST"));
    }
    #[test]
    fn mixed_numeric_and_string_ids_remain_distinct_across_pages() {
        let mut r = at_items(&Default::default());
        accept(&mut r, page(vec![feature(json!(1), json!({}))], Some("?offset=1")));
        accept(&mut r, page(vec![feature(json!("1"), json!({}))], None));
        let l = r.finish().unwrap().layers.remove(0);
        for (f, expected) in l.features.iter().zip([json!(1), json!("1")]) {
            let FieldValue::Text(text) = f.get(&l.schema, "_oapif_id").unwrap() else { panic!() };
            assert_eq!(serde_json::from_str::<Value>(text).unwrap(), expected);
        }
    }
    #[test]
    fn authority_axes_bbox_temporal_filter_and_range() {
        let mut options =
            VectorOpenOptions { spatial_filter: Some((7., 51., 9., 53.)), skip_features: 1, max_features: Some(1), ..Default::default() };
        options.oapif.datetime = Some("2020-01-01/..".into());
        let mut r = Reader::new("https://example.test/collections/a", &options).unwrap();
        let mut c = collection("a");
        c["storageCrs"] = json!("http://www.opengis.net/def/crs/EPSG/0/4326");
        accept(&mut r, c);
        let url = &r.next_request().unwrap().url;
        assert!(url.contains("limit=2"));
        assert!(url.contains("bbox=51%2C7%2C53%2C9"));
        assert!(url.contains("datetime=2020-01-01%2F.."));
        let f = |id| json!({"type":"Feature","id":id,"properties":{},"geometry":{"type":"Point","coordinates":[52.,8.]}});
        accept(&mut r, page(vec![f(1), f(2)], Some("?offset=2")));
        assert!(r.next_request().is_none());
        let layer = r.finish().unwrap().layers.remove(0);
        assert_eq!(layer.features.len(), 1);
        assert_eq!(layer.features[0].fid, 2);
        let Some(crate::Geometry::Point(p)) = &layer.features[0].geometry else { panic!() };
        assert_eq!((p.x, p.y), (8., 52.));
    }
    #[test]
    fn metadata_lists_all_catalog_pages_without_reading_features() {
        let mut r = Reader::metadata("https://example.test/collections", &Default::default()).unwrap();
        accept(&mut r, json!({"collections":[collection("a")],"links":[{"rel":"next","href":"?offset=1"}]}));
        accept(&mut r, json!({"collections":[collection("b")]}));
        assert_eq!(r.collections().iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(r.next_request().is_none());
        assert!(r.finish().unwrap().layers.is_empty());
    }
    #[test]
    fn selection_and_empty_layers_do_not_fetch_items() {
        let o = VectorOpenOptions { layers: vec!["b".into()], max_features: Some(0), ..Default::default() };
        let mut r = Reader::new("https://example.test/collections", &o).unwrap();
        accept(&mut r, json!({"collections":[collection("a"),collection("b")]}));
        let ds = r.finish().unwrap();
        assert_eq!(ds.layers.len(), 1);
        assert_eq!(ds.layers[0].name, "b");
        assert!(ds.layers[0].features.is_empty());
    }
    #[test]
    fn paging_errors_and_remote_links_are_terminal() {
        let mut r = at_items(&Default::default());
        accept(&mut r, page(vec![feature(json!(1), json!({}))], Some("?offset=1")));
        assert!(r.accept_response(&serde_json::to_vec(&page(vec![feature(json!(1), json!({"changed":true}))], None)).unwrap()).is_err());
        let mut r = at_items(&Default::default());
        assert!(r.accept_response(&serde_json::to_vec(&page(vec![], Some("?offset=1"))).unwrap()).is_err());
        let mut r = at_items(&Default::default());
        assert!(
            r.accept_response(&serde_json::to_vec(&page(vec![feature(json!(1), json!({}))], Some("https://other.test/items"))).unwrap())
                .is_err()
        );
        let mut r = at_items(&Default::default());
        let mut p = page(vec![feature(json!(1), json!({}))], None);
        p["numberMatched"] = json!(2);
        assert!(r.accept_response(&serde_json::to_vec(&p).unwrap()).is_err());
    }
    #[test]
    fn invalid_discovery_response_stops_after_one_fallback() {
        let mut r = Reader::new("https://example.test/", &Default::default()).unwrap();
        accept(&mut r, json!({"description":"landing without links"}));
        assert_eq!(r.next_request().unwrap().url, "https://example.test/collections");
        assert!(r.accept_response(br#"{"error":"not a collection"}"#).is_err());
        assert!(r.next_request().is_none());
    }
    #[test]
    fn limits_and_errors_never_echo_query_credentials() {
        let mut o = VectorOpenOptions::default();
        o.oapif.max_pages = 1;
        let mut r = Reader::new("https://example.test/?key=private", &o).unwrap();
        let e = r.accept_response(br#"{"links":[{"rel":"data","href":"/collections"}]}"#).unwrap_err();
        assert!(matches!(e, Error::ResourceLimit(_)));
        assert!(!e.to_string().contains("private"));
        let mut o = VectorOpenOptions::default();
        o.oapif.max_vector_bytes = 64;
        let mut r = Reader::new("https://example.test/", &o).unwrap();
        assert!(matches!(r.accept_response(br#"{"extra":"too big"}"#), Err(Error::ResourceLimit(_))));
        assert!(
            Reader::new("https://example.test/", &VectorOpenOptions { spatial_filter: Some((f64::NAN, 0., 1., 1.)), ..Default::default() })
                .is_err()
        );
    }
}
