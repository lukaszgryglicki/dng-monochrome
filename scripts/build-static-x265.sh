#!/bin/sh
set -eu

root="$1"
jobs="$2"
version=4.1
checksum=a31699c6a89806b74b0151e5e6a7df65de4b49050482fe5ebf8a4379d7af8f29
mkdir -p "$root"
root="$(cd "$root" && pwd)"
archive="$root/x265_$version.tar.gz"
source="$root/x265_$version"
prefix="$root/install"

verify_checksum() {
    digest="$(cmake -E sha256sum "$1")"
    if [ "${digest%% *}" != "$checksum" ]; then
        echo "x265 source checksum mismatch: $1" >&2
        exit 1
    fi
}

if [ ! -f "$archive" ]; then
    curl --fail --location --retry 3 --connect-timeout 15 --max-time 180 \
        --output "$archive.part" \
        "https://download.videolan.org/pub/videolan/x265/x265_$version.tar.gz"
    verify_checksum "$archive.part"
    mv "$archive.part" "$archive"
fi
verify_checksum "$archive"
if [ ! -f "$source/.unpacked" ]; then
    mkdir -p "$source"
    tar -xf "$archive" -C "$source" --strip-components=1
    touch "$source/.unpacked"
fi
# CMake 4 no longer permits these obsolete policy modes in x265 4.1.
sed -e 's/SET CMP0025 OLD/SET CMP0025 NEW/' \
    -e 's/SET CMP0054 OLD/SET CMP0054 NEW/' \
    "$source/source/CMakeLists.txt" > "$source/source/CMakeLists.txt.tmp"
mv "$source/source/CMakeLists.txt.tmp" "$source/source/CMakeLists.txt"
mkdir -p "$prefix/lib"

configure() {
    build="$1"
    shift
    cmake -S "$source/source" -B "$build" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$prefix" \
        -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DGIT_ARCHETYPE=1 \
        -DENABLE_SHARED=OFF -DENABLE_CLI=OFF -DENABLE_TESTS=OFF \
        -DENABLE_PIC=ON -DENABLE_ASSEMBLY=ON -DENABLE_HDR10_PLUS=OFF \
        -DENABLE_LIBNUMA=OFF -DENABLE_LIBVMAF=OFF "$@"
    cmake --build "$build" --target x265-static --parallel "$jobs"
}

for bits in 12 10; do
    main12=OFF
    if [ "$bits" = 12 ]; then main12=ON; fi
    configure "$root/${bits}bit" -DHIGH_BIT_DEPTH=ON -DMAIN12="$main12" \
        -DEXPORT_C_API=OFF
    cp "$root/${bits}bit/libx265.a" "$prefix/lib/libx265_main${bits}.a"
done
configure "$root/8bit" -DHIGH_BIT_DEPTH=OFF -DEXPORT_C_API=ON \
    -DLINKED_10BIT=ON -DLINKED_12BIT=ON \
    "-DEXTRA_LIB=x265_main10;x265_main12" "-DEXTRA_LINK_FLAGS=-L$prefix/lib"
cmake --install "$root/8bit"
cp "$source/COPYING" "$prefix/COPYING.x265"

# Upstream metadata omits multilib archives and can name a shared unwinder.
pc="$prefix/lib/pkgconfig/x265.pc"
sed -e 's/^Libs.private: */Libs.private: -lx265_main10 -lx265_main12 /' \
    -e 's/-lgcc_s/-lgcc_eh/g' "$pc" > "$pc.tmp"
mv "$pc.tmp" "$pc"
