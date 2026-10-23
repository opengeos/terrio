"""GDAL's data files in the ``terrio-gdal`` wheels.

The ``terrio-gdal`` wheels bundle GDAL with its data files (``gdal_data``) and PROJ's database
(``proj_data``) inside the ``terrio`` package, where no GDAL installation would look for them.
On import, :func:`configure` points the bundled GDAL at them, unless the ``GDAL_DATA``,
``PROJ_DATA`` or ``PROJ_LIB`` environment variables say otherwise, and points GDAL's HTTPS
requests at the system's certificates. It does nothing for a ``terrio`` built without GDAL, or
with a GDAL installed on the system (``maturin build --features gdal``), which knows its own
data files.
"""

import os
from pathlib import Path

# Where Linux distributions and macOS keep their bundle of trusted CA certificates. The bundled
# GDAL's curl uses OpenSSL on Linux and macOS, which looks only where it was built; on Windows it
# uses the system's certificate store.
_CA_BUNDLES = (
    "/etc/ssl/certs/ca-certificates.crt",  # Debian, Ubuntu, Arch, Alpine
    "/etc/pki/tls/certs/ca-bundle.crt",  # Fedora, RHEL
    "/etc/ssl/ca-bundle.pem",  # openSUSE
    "/etc/pki/tls/cacert.pem",  # OpenELEC
    "/etc/ssl/cert.pem",  # macOS, Alpine
)


def _ca_bundle() -> str | None:
    """Finds the certificates GDAL's HTTPS requests should trust.

    Returns:
        The path of a CA certificate bundle, or ``None`` when the environment already names one
        (``CURL_CA_BUNDLE`` or ``SSL_CERT_FILE``, which GDAL reads), the system's store is used
        (Windows), or none is found.
    """
    if os.name == "nt" or os.environ.get("CURL_CA_BUNDLE") or os.environ.get("SSL_CERT_FILE"):
        return None
    for path in _CA_BUNDLES:
        if os.path.isfile(path):
            return path
    try:
        import certifi
    except ImportError:
        return None
    return certifi.where()


def configure(package_dir: Path | None = None) -> None:
    """Points the bundled GDAL at the data files shipped with it.

    Args:
        package_dir: The ``terrio`` package's directory, where the ``terrio-gdal`` wheels put
            ``gdal_data`` and ``proj_data``. By default, this module's directory.
    """
    from terrio import _terrio

    set_paths = getattr(_terrio, "_set_gdal_data_paths", None)
    package_dir = Path(__file__).parent if package_dir is None else package_dir
    gdal_data = package_dir / "gdal_data"
    proj_data = package_dir / "proj_data"
    if set_paths is None or not gdal_data.is_dir():
        return
    set_paths(
        gdal_data=None if os.environ.get("GDAL_DATA") else str(gdal_data),
        proj_data=(
            None
            if os.environ.get("PROJ_DATA") or os.environ.get("PROJ_LIB") or not proj_data.is_dir()
            else str(proj_data)
        ),
        ca_bundle=_ca_bundle(),
    )
