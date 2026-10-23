# terrio-gdal wheels: a release-only build, GDAL as a shared library with its dependencies linked
# into it statically, so that the wheel ships one library (as pyogrio's wheels do).
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_CMAKE_SYSTEM_NAME Linux)
set(VCPKG_BUILD_TYPE release)

set(VCPKG_LIBRARY_LINKAGE static)
if(PORT MATCHES "gdal")
    set(VCPKG_LIBRARY_LINKAGE dynamic)
endif()
