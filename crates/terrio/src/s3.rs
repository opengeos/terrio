//! Amazon S3: object URLs and AWS Signature Version 4 request signing.
//!
//! Requests are signed when credentials are found, in this order:
//!
//! 1. `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`);
//! 2. the profile named by `AWS_PROFILE` (or `default`) in `~/.aws/credentials` (or the file in
//!    `AWS_SHARED_CREDENTIALS_FILE`), then in `~/.aws/config` (or `AWS_CONFIG_FILE`).
//!
//! `AWS_NO_SIGN_REQUEST=YES` turns signing off, for public buckets. The region comes from
//! `AWS_REGION`, `AWS_DEFAULT_REGION` or the profile's `region`; when the bucket turns out to be in
//! another region, requests are retried there. `AWS_S3_ENDPOINT` points at another S3-compatible
//! service (path-style URLs unless `AWS_VIRTUAL_HOSTING=TRUE`), and `AWS_HTTPS=NO` uses plain HTTP.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// The SHA-256 of an empty payload, sent with every `GET`.
pub(crate) const EMPTY_PAYLOAD_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// AWS credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The access key id.
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: String,
    /// The session token of temporary credentials.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key_id", &self.access_key_id).finish_non_exhaustive()
    }
}

impl Credentials {
    /// The credentials from the environment or the shared AWS files, or `None` when there are none
    /// or `AWS_NO_SIGN_REQUEST` is set.
    pub fn from_env() -> Option<Self> {
        if env_flag("AWS_NO_SIGN_REQUEST") == Some(true) {
            return None;
        }
        if let (Some(id), Some(secret)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
            return Some(Self { access_key_id: id, secret_access_key: secret, session_token: env("AWS_SESSION_TOKEN") });
        }
        let profile = profile_name();
        for (file, section) in [(credentials_file(), profile.clone()), (config_file(), config_section(&profile))] {
            let Some(text) = file.and_then(|f| std::fs::read_to_string(f).ok()) else { continue };
            let get = |key: &str| ini_value(&text, &section, key);
            if let (Some(id), Some(secret)) = (get("aws_access_key_id"), get("aws_secret_access_key")) {
                return Some(Self { access_key_id: id, secret_access_key: secret, session_token: get("aws_session_token") });
            }
        }
        None
    }
}

/// The configured AWS region, if any.
pub fn region_from_env() -> Option<String> {
    env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")).or_else(|| {
        let text = std::fs::read_to_string(config_file()?).ok()?;
        ini_value(&text, &config_section(&profile_name()), "region")
    })
}

/// An object in an S3 bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    /// The bucket.
    pub bucket: String,
    /// The object key.
    pub key: String,
}

/// Region identifiers are interpolated into AWS hostnames after discovery.
pub(crate) fn valid_region(region: &str) -> bool {
    !region.is_empty() && region.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl S3Object {
    /// The object's URL, at the regional endpoint when `region` is given, or at the endpoint in
    /// `AWS_S3_ENDPOINT`.
    pub fn url(&self, region: Option<&str>) -> String {
        let key = uri_encode(&self.key, false);
        let https = env_flag("AWS_HTTPS") != Some(false);
        let scheme = if https { "https" } else { "http" };
        if let Some(endpoint) = env("AWS_S3_ENDPOINT") {
            let endpoint = endpoint.trim_end_matches('/');
            let (scheme, host) = match endpoint.split_once("://") {
                Some((s, h)) => (s, h),
                None => (scheme, endpoint),
            };
            return if env_flag("AWS_VIRTUAL_HOSTING") == Some(true) {
                format!("{scheme}://{}.{host}/{key}", self.bucket)
            } else {
                format!("{scheme}://{host}/{}/{key}", self.bucket)
            };
        }
        let host = match region.filter(|region| valid_region(region)) {
            Some(r) => format!("s3.{r}.amazonaws.com"),
            None => "s3.amazonaws.com".to_string(),
        };
        // Bucket names with dots do not match the endpoint's TLS certificate: use path-style URLs.
        if env_flag("AWS_VIRTUAL_HOSTING") == Some(false) || (https && self.bucket.contains('.')) {
            format!("{scheme}://{host}/{}/{key}", self.bucket)
        } else {
            format!("{scheme}://{}.{host}/{key}", self.bucket)
        }
    }
}

/// Signs an S3 `GET` request: the headers to add to it (`x-amz-date`, `x-amz-content-sha256`,
/// `x-amz-security-token` for temporary credentials, and `Authorization`).
///
/// `headers` are the request's other headers that should be signed (such as `range`); `host` is
/// added from the URL.
pub(crate) fn sign_get(url: &str, headers: &[(&str, &str)], creds: &Credentials, region: &str, amz_date: &str) -> Vec<(String, String)> {
    sign_request("GET", url, headers, EMPTY_PAYLOAD_SHA256, creds, region, amz_date)
}

/// Signs an S3 request of any method whose payload has the SHA-256 `payload_hash` (hex): the
/// headers to add to it, as [`sign_get`] does.
pub(crate) fn sign_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    creds: &Credentials,
    region: &str,
    amz_date: &str,
) -> Vec<(String, String)> {
    let (host, path, query) = split_url(url);
    let mut all: Vec<(String, String)> = vec![
        ("host".into(), host.to_string()),
        ("x-amz-content-sha256".into(), payload_hash.into()),
        ("x-amz-date".into(), amz_date.into()),
    ];
    if let Some(token) = &creds.session_token {
        all.push(("x-amz-security-token".into(), token.clone()));
    }
    all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let authorization = authorization(method, path, query, &all, payload_hash, creds, region, "s3", amz_date);
    let mut out: Vec<(String, String)> = all.into_iter().filter(|(k, _)| k.starts_with("x-amz-")).collect();
    out.push(("Authorization".into(), authorization));
    out
}

/// The SigV4 `Authorization` header value of a request.
///
/// `path` and `query` are as they appear in the URL (already percent-encoded); `headers` are every
/// signed header, `host` included.
#[allow(clippy::too_many_arguments)]
pub(crate) fn authorization(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    let mut canon: Vec<(String, String)> =
        headers.iter().map(|(k, v)| (k.trim().to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    canon.sort();
    let signed_headers = canon.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_headers: String = canon.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let mut params: Vec<(&str, &str)> = query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).collect();
    params.sort();
    let canonical_query = params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let path = if path.is_empty() { "/" } else { path };
    let canonical_request = format!("{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let date = &amz_date[..8.min(amz_date.len())];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(canonical_request.as_bytes())));
    let k_date = hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", creds.access_key_id)
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Splits a URL into its host (with any port), path and query.
fn split_url(url: &str) -> (&str, &str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split('#').next().unwrap_or(rest);
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..], query),
        None => (rest, "/", query),
    }
}

/// Percent-encodes a string as SigV4 requires: everything but `A-Z a-z 0-9 - _ . ~`, and `/`
/// unless `encode_slash`.
pub(crate) fn uri_encode(s: &str, encode_slash: bool) -> String {
    crate::vsi::listing::uri_encode(s, encode_slash)
}

/// The current UTC time as `YYYYMMDDTHHMMSSZ`.
pub(crate) fn amz_date_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    amz_date(secs)
}

/// Formats seconds since the Unix epoch as `YYYYMMDDTHHMMSSZ`.
fn amz_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// A boolean environment variable (`YES`/`TRUE`/`ON`/`1` or `NO`/`FALSE`/`OFF`/`0`).
pub(crate) fn env_flag(name: &str) -> Option<bool> {
    match env(name)?.to_ascii_uppercase().as_str() {
        "YES" | "TRUE" | "ON" | "1" => Some(true),
        "NO" | "FALSE" | "OFF" | "0" => Some(false),
        _ => None,
    }
}

fn profile_name() -> String {
    env("AWS_PROFILE").or_else(|| env("AWS_DEFAULT_PROFILE")).unwrap_or_else(|| "default".into())
}

fn config_section(profile: &str) -> String {
    if profile == "default" { profile.to_string() } else { format!("profile {profile}") }
}

fn aws_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".aws"))
}

fn credentials_file() -> Option<PathBuf> {
    env("AWS_SHARED_CREDENTIALS_FILE").map(PathBuf::from).or_else(|| aws_dir().map(|d| d.join("credentials")))
}

fn config_file() -> Option<PathBuf> {
    env("AWS_CONFIG_FILE").map(PathBuf::from).or_else(|| aws_dir().map(|d| d.join("config")))
}

/// A key's value in a section of an INI file.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name.trim() == section;
        } else if in_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_creds() -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    fn headers(h: &[(&str, &str)]) -> Vec<(String, String)> {
        h.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn discovered_regions_are_hostname_components() {
        for region in ["us-east-1", "eu-west-3", "us-gov-west-1"] {
            assert!(valid_region(region));
        }
        for region in ["", "us-east-1#evil", "us-east-1@evil", "us-east-1/evil", "US-EAST-1", "us east 1"] {
            assert!(!valid_region(region));
        }
    }

    /// `get-vanilla` from AWS's SigV4 test suite.
    #[test]
    fn sigv4_get_vanilla() {
        let h = headers(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")]);
        let auth = authorization("GET", "/", "", &h, EMPTY_PAYLOAD_SHA256, &example_creds(), "us-east-1", "service", "20150830T123600Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// `get-vanilla-query-order-key-case` from AWS's SigV4 test suite.
    #[test]
    fn sigv4_query_order() {
        let h = headers(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")]);
        let auth = authorization(
            "GET",
            "/",
            "Param2=value2&Param1=value1",
            &h,
            EMPTY_PAYLOAD_SHA256,
            &example_creds(),
            "us-east-1",
            "service",
            "20150830T123600Z",
        );
        assert!(auth.ends_with("Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"), "{auth}");
    }

    /// `get-header-value-trim` from AWS's SigV4 test suite: header values are trimmed and inner
    /// runs of spaces collapsed.
    #[test]
    fn sigv4_header_value_trim() {
        let h = headers(&[
            ("Host", "example.amazonaws.com"),
            ("My-Header1", " value1"),
            ("My-Header2", " \"a   b   c\""),
            ("X-Amz-Date", "20150830T123600Z"),
        ]);
        let auth = authorization("GET", "/", "", &h, EMPTY_PAYLOAD_SHA256, &example_creds(), "us-east-1", "service", "20150830T123600Z");
        assert!(auth.ends_with("Signature=acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736"), "{auth}");
    }

    /// The `GET Object` example of the Amazon S3 documentation ("Signature Calculations for the
    /// Authorization Header: Transferring Payload in a Single Chunk").
    #[test]
    fn sigv4_s3_get_object() {
        let creds = Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let added =
            sign_get("https://examplebucket.s3.amazonaws.com/test.txt", &[("range", "bytes=0-9")], &creds, "us-east-1", "20130524T000000Z");
        let auth = &added.iter().find(|(k, _)| k == "Authorization").unwrap().1;
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(added.iter().any(|(k, v)| k == "x-amz-content-sha256" && v == EMPTY_PAYLOAD_SHA256));
    }

    #[test]
    fn formats_dates() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
    }

    #[test]
    fn encodes_keys() {
        assert_eq!(uri_encode("a b/c+d~e.tif", false), "a%20b/c%2Bd~e.tif");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn reads_ini_sections() {
        let text = "[default]\naws_access_key_id = A\n\n[profile dev]\nregion=eu-west-1\n";
        assert_eq!(ini_value(text, "default", "aws_access_key_id").as_deref(), Some("A"));
        assert_eq!(ini_value(text, "profile dev", "region").as_deref(), Some("eu-west-1"));
        assert_eq!(ini_value(text, "default", "region"), None);
    }
}
