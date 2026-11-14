// SPDX-FileCopyrightText: Copyright (c) 2019 Martin Algesten
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Adapted from ureq 3.5.1, src/tls/rustls.rs (connector and transport adapter).
// https://github.com/algesten/ureq/blob/33ca9d35e97b6a10e767fb0fa3139f01d315e89c/src/tls/rustls.rs

//! Additional roots without losing Mozilla-applied trust-anchor constraints.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::time::{Duration, Instant};
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, LazyBuffers, NextTimeout, TcpConnector, Transport, TransportAdapter,
};

use super::Http10CloseConnector;

/// Use the same crypto provider as the ordinary native client, without global state.
pub(super) fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    return Arc::new(rustls_graviola::default_provider());
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Keep default anchors byte-for-byte, including constraints applied outside the DER.
/// Explicit duplicate public roots cannot remove those constraints either.
fn augmented_roots(certificates: impl IntoIterator<Item = CertificateDer<'static>>) -> Result<RootCertStore, String> {
    let defaults = webpki_roots::TLS_SERVER_ROOTS;
    let mut roots = RootCertStore { roots: defaults.to_vec() };
    for certificate in certificates {
        let mut extra = RootCertStore::empty();
        extra.add(certificate).map_err(|_| "TERRIO_HTTP_CAINFO: invalid CA certificate in PEM bundle".to_string())?;
        for anchor in extra.roots {
            if !defaults
                .iter()
                .any(|known| known.subject == anchor.subject && known.subject_public_key_info == anchor.subject_public_key_info)
            {
                roots.roots.push(anchor);
            }
        }
    }
    Ok(roots)
}

fn read_bundle(path: &Path) -> Result<RootCertStore, String> {
    // Avoid the adapter's dataset-not-found mapping for a missing configuration file.
    let pem = std::fs::read(path).map_err(|error| format!("TERRIO_HTTP_CAINFO: cannot read PEM CA bundle ({:?})", error.kind()))?;
    let mut certificates = Vec::new();
    for item in ureq::tls::parse_pem(&pem) {
        match item {
            Ok(ureq::tls::PemItem::Certificate(cert)) => certificates.push(CertificateDer::from(cert.der().to_vec())),
            Ok(_) => {}
            Err(_) => return Err("TERRIO_HTTP_CAINFO: invalid PEM CA bundle".into()),
        }
    }
    if certificates.is_empty() {
        return Err("TERRIO_HTTP_CAINFO: PEM CA bundle contains no certificates".into());
    }
    augmented_roots(certificates)
}

/// ureq's DER-only RootCerts::Specific cannot represent Mozilla's applied constraints.
/// Use its public proxy/TCP adapters with a rustls verifier built from complete anchors.
pub(super) fn with_bundle(config: ureq::config::Config, path: &Path) -> ureq::Agent {
    let configured = read_bundle(path).and_then(|roots| {
        ClientConfig::builder_with_provider(crypto_provider())
            .with_safe_default_protocol_versions()
            .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
            .map_err(|_| "TERRIO_HTTP_CAINFO: cannot configure TLS protocol versions".into())
    });
    let (tls, error) = match configured {
        Ok(mut tls) => {
            tls.enable_sni = config.tls_config().use_sni();
            (Some(Arc::new(tls)), None)
        }
        Err(error) => (None, Some(error)),
    };
    // Same proxy-before-TCP ordering as DefaultConnector. This workspace does not
    // enable ureq's SOCKS feature; refuse it here rather than silently going direct.
    let connector = BundleGuard(error)
        .chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(AdditionalRootsConnector(tls))
        .chain(Http10CloseConnector);
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

#[derive(Debug)]
struct BundleGuard(Option<String>);

impl Connector<()> for BundleGuard {
    type Out = ();

    fn connect(&self, details: &ConnectionDetails, chained: Option<()>) -> Result<Option<()>, ureq::Error> {
        if details.needs_tls()
            && let Some(error) = &self.0
        {
            return Err(ureq::Error::Other(Box::new(io::Error::new(io::ErrorKind::InvalidInput, error.clone()))));
        }
        if let Some(proxy) = details.config.proxy()
            && !proxy.is_no_proxy(details.uri)
            && !matches!(proxy.protocol(), ureq::ProxyProtocol::Http | ureq::ProxyProtocol::Https)
        {
            return Err(ureq::Error::Tls("SOCKS proxies are not enabled for the additional-CA connector"));
        }
        Ok(chained)
    }
}

#[derive(Debug)]
struct AdditionalRootsConnector(Option<Arc<ClientConfig>>);

impl<In: Transport> Connector<In> for AdditionalRootsConnector {
    type Out = Box<dyn Transport>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else { return Ok(None) };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(transport.boxed()));
        }
        let config = self.0.as_ref().ok_or(ureq::Error::Tls("additional CA configuration is unavailable"))?;
        let host = details.uri.host().ok_or(ureq::Error::Tls("missing TLS hostname"))?;
        let host = host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host);
        let name = ServerName::try_from(host.to_owned()).map_err(|_| ureq::Error::Tls("invalid TLS hostname"))?;
        let mut connection = ClientConnection::new(config.clone(), name)?;
        let mut socket = TransportAdapter::new(transport.boxed(), details.current_time.clone());
        // TransportAdapter's public setter starts its clock now. Subtract time
        // already spent resolving/connecting so the handshake cannot restart the budget.
        let mut timeout = details.timeout;
        if let (Duration::Exact(budget), Instant::Exact(start), Instant::Exact(now)) =
            (timeout.after, details.now, (details.current_time)())
        {
            timeout.after = Duration::Exact(budget.saturating_sub(now.saturating_duration_since(start)));
        }
        socket.set_timeout(timeout);
        connection.complete_io(&mut socket)?;
        Ok(Some(Box::new(AdditionalRootsTransport {
            buffers: LazyBuffers::new(details.config.input_buffer_size(), details.config.output_buffer_size()),
            stream: StreamOwned::new(connection, socket),
        })))
    }
}

struct AdditionalRootsTransport {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, TransportAdapter>,
}

impl Transport for AdditionalRootsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        self.stream.write_all(&self.buffers.output()[..amount])?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let amount = self.stream.read(self.buffers.input_append_buf())?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}

impl fmt::Debug for AdditionalRootsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdditionalRootsTransport").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_roots_preserve_mozilla_constraints_even_for_duplicate_public_der() {
        let defaults = webpki_roots::TLS_SERVER_ROOTS;
        let constrained =
            defaults.iter().find(|anchor| anchor.name_constraints.is_some()).expect("snapshot includes a constrained public root");
        let certificate = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .find(|certificate| {
                let mut store = RootCertStore::empty();
                store.add((*certificate).clone()).unwrap();
                let anchor = &store.roots[0];
                anchor.subject == constrained.subject && anchor.subject_public_key_info == constrained.subject_public_key_info
            })
            .expect("DER snapshot includes the constrained root");
        let mut unconstrained = RootCertStore::empty();
        unconstrained.add(certificate.clone()).unwrap();
        assert_ne!(unconstrained.roots[0].name_constraints, constrained.name_constraints);
        let augmented = augmented_roots([certificate.clone()]).unwrap();
        assert_eq!(augmented.roots.as_slice(), defaults);
    }
}
