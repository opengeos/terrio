# terrio-gdal wheels: a release-only build of shared libraries (DLLs), which delvewheel bundles
# into the wheel (as pyogrio's wheels do).
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE dynamic)
set(VCPKG_BUILD_TYPE release)
