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
use std::sync::{Arc, OnceLock};

use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::time::{Duration, Instant};
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, LazyBuffers, NextTimeout, TcpConnector, Transport, TransportAdapter,
};

use super::Http10CloseConnector;

/// The TLS cryptography of every native client, or why this build has none on this CPU.
pub(super) type Provider = Result<Arc<CryptoProvider>, String>;

/// The CPU features `rustls-graviola` needs, as `std::arch` names them. graviola 0.4 asserts
/// `aes`, `pclmulqdq`, `bmi1`, `adx`, `avx` and `avx2` at each of its entry points (a panic at the
/// first handshake, not when the provider is made). Its README also requires `ssse3` and `bmi2`,
/// which its assembly uses (`mulx`) without checking.
#[cfg(target_arch = "x86_64")]
const GRAVIOLA_CPU_FEATURES: [&str; 8] = ["aes", "pclmulqdq", "ssse3", "avx", "avx2", "adx", "bmi1", "bmi2"];
/// The CPU features `rustls-graviola` asserts on AArch64.
#[cfg(target_arch = "aarch64")]
const GRAVIOLA_CPU_FEATURES: [&str; 4] = ["neon", "aes", "pmull", "sha2"];

/// Whether this CPU has `feature`, one of [`GRAVIOLA_CPU_FEATURES`].
#[cfg(target_arch = "x86_64")]
fn cpu_has(feature: &str) -> bool {
    match feature {
        "aes" => std::arch::is_x86_feature_detected!("aes"),
        "pclmulqdq" => std::arch::is_x86_feature_detected!("pclmulqdq"),
        "ssse3" => std::arch::is_x86_feature_detected!("ssse3"),
        "avx" => std::arch::is_x86_feature_detected!("avx"),
        "avx2" => std::arch::is_x86_feature_detected!("avx2"),
        "adx" => std::arch::is_x86_feature_detected!("adx"),
        "bmi1" => std::arch::is_x86_feature_detected!("bmi1"),
        "bmi2" => std::arch::is_x86_feature_detected!("bmi2"),
        _ => false,
    }
}

/// Whether this CPU has `feature`, one of [`GRAVIOLA_CPU_FEATURES`].
#[cfg(target_arch = "aarch64")]
fn cpu_has(feature: &str) -> bool {
    match feature {
        "neon" => std::arch::is_aarch64_feature_detected!("neon"),
        "aes" => std::arch::is_aarch64_feature_detected!("aes"),
        "pmull" => std::arch::is_aarch64_feature_detected!("pmull"),
        "sha2" => std::arch::is_aarch64_feature_detected!("sha2"),
        _ => false,
    }
}

/// The features graviola needs that `has` reports missing, in [`GRAVIOLA_CPU_FEATURES`] order.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn graviola_missing(has: impl Fn(&str) -> bool) -> Vec<&'static str> {
    GRAVIOLA_CPU_FEATURES.into_iter().filter(|feature| !has(feature)).collect()
}

/// graviola when no feature it needs is missing, otherwise the fallback when the build has one.
/// Without one, the missing features, so that HTTPS fails naming them instead of panicking.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn select<T>(
    missing: Vec<&'static str>,
    graviola: impl FnOnce() -> T,
    fallback: Option<impl FnOnce() -> T>,
) -> Result<T, Vec<&'static str>> {
    match (missing.is_empty(), fallback) {
        (true, _) => Ok(graviola()),
        (false, Some(fallback)) => Ok(fallback()),
        (false, None) => Err(missing),
    }
}

/// Why HTTPS is unavailable on a CPU that lacks `missing`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn unsupported_cpu(missing: &[&str]) -> String {
    let architecture = if cfg!(target_arch = "x86_64") { "x86-64" } else { "AArch64" };
    format!(
        "HTTPS is unavailable on this CPU: its TLS cryptography (rustls-graviola) needs the {architecture} features {}, \
         and this CPU lacks {}. Plain HTTP works; terrio built with the `ring-fallback` feature (which needs a C compiler) \
         uses ring here instead",
        GRAVIOLA_CPU_FEATURES.join(", "),
        missing.join(", ")
    )
}

/// graviola where the CPU has what it needs; elsewhere ring, when the build has it.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn detect_provider() -> Provider {
    #[cfg(feature = "ring-fallback")]
    let fallback = Some(rustls::crypto::ring::default_provider);
    #[cfg(not(feature = "ring-fallback"))]
    let fallback = None::<fn() -> CryptoProvider>;
    select(graviola_missing(cpu_has), rustls_graviola::default_provider, fallback)
        .map(Arc::new)
        .map_err(|missing| unsupported_cpu(&missing))
}

/// ring, on the architectures graviola does not support.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect_provider() -> Provider {
    Ok(Arc::new(rustls::crypto::ring::default_provider()))
}

/// The same crypto provider for the ordinary and additional-CA clients, without global state.
/// The CPU is checked once per process.
pub(super) fn crypto_provider() -> Provider {
    static PROVIDER: OnceLock<Provider> = OnceLock::new();
    PROVIDER.get_or_init(detect_provider).clone()
}

/// Fails each HTTPS connection with why there is no TLS cryptography, before it is opened; plain
/// HTTP connections go on. `None` lets every connection through.
#[derive(Debug)]
pub(super) struct TlsUnavailable(pub(super) Option<String>);

impl Connector<()> for TlsUnavailable {
    type Out = ();

    fn connect(&self, details: &ConnectionDetails, chained: Option<()>) -> Result<Option<()>, ureq::Error> {
        match &self.0 {
            Some(reason) if details.needs_tls() => {
                Err(ureq::Error::Other(Box::new(io::Error::new(io::ErrorKind::Unsupported, reason.clone()))))
            }
            _ => Ok(chained),
        }
    }
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
    if !path.is_absolute() {
        return Err("TERRIO_HTTP_CAINFO: PEM CA bundle path must be absolute".into());
    }
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
/// Without TLS cryptography (`provider` is an error), HTTPS fails with its reason, as it does with
/// a bundle that cannot be read.
pub(super) fn with_bundle(config: ureq::config::Config, path: &Path, provider: Provider) -> ureq::Agent {
    let configured = provider.and_then(|provider| read_bundle(path).map(|roots| (provider, roots))).and_then(|(provider, roots)| {
        ClientConfig::builder_with_provider(provider)
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
    agent_with_tls(config, tls, error)
}

/// A client that does not verify the server's certificate or match its name, for
/// `TERRIO_HTTP_UNSAFESSL=YES` (`GDAL_HTTP_UNSAFESSL`, which GDAL 3.13.3 turns into curl's
/// `CURLOPT_SSL_VERIFYPEER=0` and `CURLOPT_SSL_VERIFYHOST=0`). It takes precedence over a CA
/// bundle, as in GDAL. Without TLS cryptography (`provider` is an error), HTTPS fails with its
/// reason, as with a bundle that cannot be read.
pub(super) fn without_verification(config: ureq::config::Config, provider: Provider) -> ureq::Agent {
    let configured = provider.and_then(|provider| {
        ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map(|builder| builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify { provider })).with_no_client_auth())
            .map_err(|_| "TERRIO_HTTP_UNSAFESSL: cannot configure TLS protocol versions".into())
    });
    let (tls, error) = match configured {
        Ok(mut tls) => {
            tls.enable_sni = config.tls_config().use_sni();
            (Some(Arc::new(tls)), None)
        }
        Err(error) => (None, Some(error)),
    };
    agent_with_tls(config, tls, error)
}

/// Accepts any server certificate and any handshake signature: `NoVerify` is only installed by
/// [`without_verification`], for `TERRIO_HTTP_UNSAFESSL=YES`.
#[derive(Debug)]
struct NoVerify {
    provider: Arc<CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// An agent whose TLS connections use `tls` (a custom `ClientConfig`), failing HTTPS with `error`
/// when there is none. Same proxy-before-TCP ordering as DefaultConnector; this workspace does not
/// enable ureq's SOCKS feature, so it is refused here rather than silently going direct.
fn agent_with_tls(config: ureq::config::Config, tls: Option<Arc<ClientConfig>>, error: Option<String>) -> ureq::Agent {
    let connector = BundleGuard(error)
        .chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(CustomTlsConnector(tls))
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
            return Err(ureq::Error::Other(Box::new(io::Error::new(
                io::ErrorKind::Unsupported,
                "TERRIO_HTTP_CAINFO/TERRIO_HTTP_UNSAFESSL: SOCKS proxies are not supported with a custom TLS configuration",
            ))));
        }
        Ok(chained)
    }
}

/// TLS with a custom `ClientConfig`: additional roots ([`with_bundle`]) or no verification
/// ([`without_verification`]).
#[derive(Debug)]
struct CustomTlsConnector(Option<Arc<ClientConfig>>);

impl<In: Transport> Connector<In> for CustomTlsConnector {
    type Out = Box<dyn Transport>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else { return Ok(None) };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(transport.boxed()));
        }
        let config = self.0.as_ref().ok_or(ureq::Error::Tls("custom TLS configuration is unavailable"))?;
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
        Ok(Some(Box::new(CustomTlsTransport {
            buffers: LazyBuffers::new(details.config.input_buffer_size(), details.config.output_buffer_size()),
            stream: StreamOwned::new(connection, socket),
        })))
    }
}

struct CustomTlsTransport {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, TransportAdapter>,
}

impl Transport for CustomTlsTransport {
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

impl fmt::Debug for CustomTlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustomTlsTransport").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CPU without AVX2, ADX and BMI2 (an Ivy Bridge, say) gets the fallback when the build has
    /// one, and otherwise the features it lacks, not graviola, which would panic.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn a_cpu_without_avx2_adx_and_bmi2_gets_the_fallback_or_the_missing_features() {
        let ivy_bridge = |feature: &str| !matches!(feature, "avx2" | "adx" | "bmi2");
        assert_eq!(graviola_missing(ivy_bridge), ["avx2", "adx", "bmi2"]);
        assert_eq!(select(graviola_missing(ivy_bridge), || "graviola", Some(|| "ring")), Ok("ring"));
        assert_eq!(select(graviola_missing(ivy_bridge), || "graviola", None::<fn() -> &'static str>), Err(vec!["avx2", "adx", "bmi2"]));
        // One missing feature is enough: graviola asserts each.
        let no_adx = |feature: &str| feature != "adx";
        assert_eq!(select(graviola_missing(no_adx), || "graviola", None::<fn() -> &'static str>), Err(vec!["adx"]));
        let message = unsupported_cpu(&graviola_missing(ivy_bridge));
        assert!(message.contains("needs the x86-64 features aes, pclmulqdq, ssse3, avx, avx2, adx, bmi1, bmi2"), "{message}");
        assert!(message.contains("this CPU lacks avx2, adx, bmi2"), "{message}");
        assert!(message.contains("`ring-fallback`"), "{message}");
    }

    /// An AArch64 CPU without the cryptography extensions (a Cortex-A53 without them, say).
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn a_cpu_without_the_cryptography_extensions_gets_the_fallback_or_the_missing_features() {
        let no_crypto = |feature: &str| feature == "neon";
        assert_eq!(graviola_missing(no_crypto), ["aes", "pmull", "sha2"]);
        assert_eq!(select(graviola_missing(no_crypto), || "graviola", Some(|| "ring")), Ok("ring"));
        assert_eq!(select(graviola_missing(no_crypto), || "graviola", None::<fn() -> &'static str>), Err(vec!["aes", "pmull", "sha2"]));
        assert!(unsupported_cpu(&["aes"]).contains("needs the AArch64 features neon, aes, pmull, sha2"));
    }

    /// A CPU with every feature keeps graviola, whether the build has a fallback or not.
    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn a_cpu_with_every_feature_keeps_graviola() {
        assert!(graviola_missing(|_| true).is_empty());
        assert_eq!(select(graviola_missing(|_| true), || "graviola", Some(|| "ring")), Ok("graviola"));
        assert_eq!(select(graviola_missing(|_| true), || "graviola", None::<fn() -> &'static str>), Ok("graviola"));
    }

    /// The provider this process gets agrees with its CPU: graviola's, which then completes a
    /// handshake's first cryptography (random bytes and a key share) without panicking, or ring's
    /// with `ring-fallback`, or the error naming what is missing.
    #[test]
    fn the_process_provider_follows_the_cpu() {
        match crypto_provider() {
            Ok(provider) => {
                let mut random = [0u8; 32];
                provider.secure_random.fill(&mut random).unwrap();
                let group = provider.kx_groups[0];
                group.start().unwrap();
                #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(feature = "ring-fallback")))]
                assert!(graviola_missing(cpu_has).is_empty());
            }
            #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(feature = "ring-fallback")))]
            Err(reason) => assert!(!graviola_missing(cpu_has).is_empty(), "{reason}"),
            #[cfg(any(feature = "ring-fallback", not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
            Err(reason) => panic!("ring is available to this build: {reason}"),
        }
    }

    #[test]
    fn relative_ca_paths_are_refused_before_reading_a_file() {
        assert_eq!(read_bundle(Path::new("ca.pem")).unwrap_err(), "TERRIO_HTTP_CAINFO: PEM CA bundle path must be absolute");
    }

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
