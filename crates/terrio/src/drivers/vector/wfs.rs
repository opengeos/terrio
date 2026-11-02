// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Read-only OGC Web Feature Services, using GDAL 3.13.3 as the behavior reference.
//! GetFeature responses use the native GML and GeoJSON readers.

/// The raw value of a case-insensitive OGC KVP parameter.
#[cfg(remote)]
pub(crate) fn parameter<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split('#')
        .next()?
        .split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.split_once('=').filter(|(name, _)| name.eq_ignore_ascii_case(key)).map(|(_, value)| value))
}

/// An explicit WFS GetFeature request, rather than a file whose server may use ranges.
#[cfg(remote)]
pub(crate) fn is_feature_request(url: &str) -> bool {
    parameter(url, "request").is_some_and(|request| request.eq_ignore_ascii_case("GetFeature"))
        && parameter(url, "service").is_none_or(|service| service.eq_ignore_ascii_case("WFS"))
}

use crate::{Error, Result};

/// Native WFS open options, used by Rust, CLI/MCP and Python.
#[derive(Debug, Clone)]
pub struct WfsOpenOptions {
    /// Features requested per page. The default is 1000.
    pub page_size: usize,
    /// Paging override. By default enabled for WFS 2, disabled for WFS 1.
    pub paging: Option<bool>,
    /// Estimated retained vector bytes across selected layers (default 1 GiB).
    pub max_vector_bytes: usize,
}

impl Default for WfsOpenOptions {
    fn default() -> Self {
        Self { page_size: 1000, paging: None, max_vector_bytes: 1 << 30 }
    }
}

impl WfsOpenOptions {
    /// Sets WFS_PAGE_SIZE, WFS_PAGING or WFS_MAX_VECTOR_BYTES.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        let positive = || {
            value
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| Error::InvalidArgument(format!("{key} must be a positive integer")))
        };
        match key.to_ascii_uppercase().as_str() {
            "WFS_PAGE_SIZE" => self.page_size = positive()?,
            "WFS_PAGING" => self.paging = Some(crate::tiles::parse_bool(value)?),
            "WFS_MAX_VECTOR_BYTES" => self.max_vector_bytes = positive()?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// A capabilities document, distinguished from GML feature data by its root.
pub(crate) fn is_capabilities(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e)) => {
                return e.local_name().as_ref() == "WFS_Capabilities";
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}

/// The endpoint of a WFS: name or an unprefixed SERVICE=WFS capabilities URL.
pub(crate) fn service_target(path: &str) -> Option<&str> {
    let path = path.trim();
    if path.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("WFS:")) {
        return Some(&path[4..]);
    }
    #[cfg(remote)]
    if (path.starts_with("https://") || path.starts_with("http://"))
        && parameter(path, "service").is_some_and(|v| v.eq_ignore_ascii_case("WFS"))
        && parameter(path, "request").is_none_or(|v| v.eq_ignore_ascii_case("GetCapabilities"))
    {
        return Some(path);
    }
    None
}

#[cfg(remote)]
#[path = "wfs_client.rs"]
mod client;
#[cfg(remote)]
pub use client::{ServiceInfo, ServiceLayerInfo};
#[cfg(remote)]
pub(crate) use client::{info, list_layers, open};
