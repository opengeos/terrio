# The terrio command, built from the release's source with the distribution's Rust. Built on COPR
# (https://copr.fedorainfracloud.org/coprs/giswqs/terrio/), whose builds may use the network:
# cargo fetches the crates of Cargo.lock.

# The binary carries no debug information worth splitting out.
%global debug_package %{nil}
# Its C dependencies (zstd, ring) are linked by Rust's linker, which cannot read GCC's LTO objects.
%global _lto_cflags %{nil}

Name:           terrio
Version:        0.4.1
Release:        1%{?dist}
Summary:        Pure-Rust geospatial raster and vector data I/O

License:        MIT AND MPL-2.0
URL:            https://terrio.opengeos.org
Source0:        https://github.com/opengeos/terrio/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz

BuildRequires:  cargo >= 1.90
BuildRequires:  rust >= 1.90
BuildRequires:  gcc

%description
terrio reads, writes, converts and reprojects the geospatial formats most workflows use
(rasters, vector data and point clouds) in pure Rust: GeoTIFF and COG, GeoPackage, Shapefile,
GeoParquet, FlatGeobuf, Zarr, netCDF, LAS/LAZ/COPC and more, with warping, mosaics, terrain
analysis and raster algebra matching GDAL. This package installs the terrio command.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked -p terrio-cli

%install
install -Dm755 target/release/terrio %{buildroot}%{_bindir}/terrio

%files
%license LICENSE LICENSE-THIRD-PARTY.md NOTICE
%doc README.md
%{_bindir}/terrio

%changelog
* Thu Oct 22 2026 Qiusheng Wu <giswqs@gmail.com> - 0.4.1-1
- Release 0.4.1.
