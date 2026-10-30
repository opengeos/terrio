//! Engine errors and capability refusals as terrio errors.
//!
//! The engine decides what is unsupported, corrupt, absent or a caller error; this module only
//! chooses the matching [`crate::Error`] variant and keeps the engine's stable spellings
//! (`ErrorKind::as_str`, `UnsupportedCode::as_str`) at the start of the message, so a caller can
//! still tell the engine's categories apart through terrio's string-carrying variants.

use ras_commander_fast_hdf::{Capability, Error as EngineError, ErrorKind, Reported, UnsupportedCode};

/// The terrio error for an engine error.
///
/// | Engine kind | terrio variant |
/// |---|---|
/// | `file_not_found` | [`crate::Error::Io`] (`NotFound`) |
/// | `io`, `internal` | [`crate::Error::Io`] |
/// | `unsupported`, or `invalid_request` with code `resource_limit` | [`crate::Error::Unsupported`] |
/// | `invalid_request`, `not_found` | [`crate::Error::InvalidArgument`] |
/// | `corrupt`, `structural_topology`, `invalid_geometry`, others | [`crate::Error::InvalidData`] |
pub fn from_engine(e: EngineError) -> crate::Error {
    let text = describe(e.kind.as_str(), e.code, &e.message, e.path.as_deref(), e.subject.as_deref());
    match e.kind {
        ErrorKind::FileNotFound => crate::Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, text)),
        ErrorKind::Io | ErrorKind::Internal => crate::Error::Io(std::io::Error::other(text)),
        ErrorKind::Unsupported => crate::Error::Unsupported(text),
        ErrorKind::InvalidRequest if e.code == Some(UnsupportedCode::ResourceLimit) => crate::Error::Unsupported(text),
        ErrorKind::InvalidRequest | ErrorKind::NotFound => crate::Error::InvalidArgument(text),
        _ => crate::Error::InvalidData(text),
    }
}

/// `Ok(())` for a supported capability; otherwise the refusal as a terrio error. `what` names the
/// object the capability describes. Unavailable data (canonically absent) is a caller error:
/// the request asked for something the file does not store. It is never an empty layer.
pub fn require(capability: &Capability, what: &str) -> crate::Result<()> {
    let text = |reason: &str| describe(capability.as_str(), capability_code(capability), &format!("{what}: {reason}"), None, None);
    match capability {
        Capability::Supported => Ok(()),
        Capability::Unavailable { reason } => Err(crate::Error::InvalidArgument(text(reason))),
        Capability::Corrupt { reason } => Err(crate::Error::InvalidData(text(reason))),
        // Unsupported, Delegated and any later status: refuse.
        other => Err(crate::Error::Unsupported(text(other.reason().unwrap_or("not supported")))),
    }
}

/// `Reported` engine data as a value, or its refusal as a terrio error ([`require`]). Unavailable
/// (canonically absent) data is a caller error: the request asked for something the file does
/// not store.
pub fn take<T: Clone>(r: Reported<T>, what: &str) -> crate::Result<T> {
    match r.value() {
        Some(v) if r.is_supported() => Ok(v.clone()),
        _ => {
            require(r.capability(), what)?;
            Err(crate::Error::InvalidData(format!("HEC-RAS {what}: supported without a value")))
        }
    }
}

fn capability_code(capability: &Capability) -> Option<UnsupportedCode> {
    match capability {
        Capability::Unsupported { code, .. } => Some(*code),
        _ => None,
    }
}

fn describe(kind: &str, code: Option<UnsupportedCode>, message: &str, path: Option<&std::path::Path>, subject: Option<&str>) -> String {
    let mut s = format!("HEC-RAS {kind}");
    if let Some(code) = code {
        s.push_str(&format!("({})", code.as_str()));
    }
    s.push_str(": ");
    s.push_str(message);
    if let Some(subject) = subject {
        s.push_str(&format!(" [{subject}]"));
    }
    if let Some(path) = path {
        s.push_str(&format!(" ({})", path.display()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_kinds_map_to_terrio_variants_with_stable_spellings() {
        let e = from_engine(EngineError::unsupported(UnsupportedCode::Encoding, "vlen text"));
        assert!(matches!(&e, crate::Error::Unsupported(m) if m == "HEC-RAS unsupported(encoding): vlen text"), "{e}");
        let e = from_engine(EngineError::new(ErrorKind::InvalidRequest, "too big").with_code(UnsupportedCode::ResourceLimit));
        assert!(matches!(&e, crate::Error::Unsupported(m) if m.starts_with("HEC-RAS invalid_request(resource_limit)")), "{e}");
        let e = from_engine(EngineError::new(ErrorKind::NotFound, "no mesh"));
        assert!(matches!(e, crate::Error::InvalidArgument(_)));
        for kind in [ErrorKind::Corrupt, ErrorKind::StructuralTopology, ErrorKind::InvalidGeometry] {
            assert!(matches!(from_engine(EngineError::new(kind, "x")), crate::Error::InvalidData(_)), "{kind:?}");
        }
        let e = from_engine(EngineError::new(ErrorKind::FileNotFound, "gone").with_path("a.hdf"));
        assert!(matches!(&e, crate::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound && io.to_string().contains("a.hdf")));
    }

    #[test]
    fn refusals_are_errors_never_success() {
        assert!(require(&Capability::Supported, "x").is_ok());
        assert!(matches!(require(&Capability::unavailable("absent"), "x"), Err(crate::Error::InvalidArgument(_))));
        assert!(matches!(require(&Capability::corrupt("bad"), "x"), Err(crate::Error::InvalidData(_))));
        let e = require(&Capability::unsupported(UnsupportedCode::IdentityLayout, "no Location"), "Maximum Water Surface").unwrap_err();
        assert!(
            matches!(&e, crate::Error::Unsupported(m) if m == "HEC-RAS unsupported(identity_layout): Maximum Water Surface: no Location"),
            "{e}"
        );
    }
}
