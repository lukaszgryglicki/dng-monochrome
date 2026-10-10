#!/bin/sh
set -eu

action="$1"
target="$2"
jobs="$3"
shift 3
scripts="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
base="${DNG_MONO_SDK_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/dng-monochrome}"
root="$base/muslcc-11.2.1-aom-3.12.1-x265-4.1/$target"
arch="${target%%-*}"
host="$(rustc -vV | sed -n 's/^host: //p')"
host_arch="${host%%-*}"

fail() {
    printf '%s\nRun make requirements before building/installing.\n' "$*" >&2
    exit 1
}

case "$target" in
    x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
    *) fail "No automatic musl SDK recipe for $target" ;;
esac
case "$host_arch:$arch" in
    x86_64:x86_64)
        archive=x86_64-linux-musl-native.tgz
        checksum=44d441ad9aa11a06feddf3daa4c9f53ad7d9ca37af1f5a61379aca07793703d179410cea723c1b7fca94c4de19a321228bdb3656bc5cbdb5e3bea8e2d6dac6c7 ;;
    aarch64:aarch64)
        archive=aarch64-linux-musl-native.tgz
        checksum=16d544e09845c9dbba50f29e0cb04dd661e17eb63c56acad6a67fd2a78aa7596b792477c7177d3cd56d408a27dc291a90507df882f2b099c0f25511ce08fd3b5 ;;
    x86_64:aarch64)
        archive=aarch64-linux-musl-cross.tgz
        checksum=8695ff86979cdf30fbbcd33061711f5b1ebc3c48a87822b9ca56cde6d3a22abd4dab30fdcd1789ac27c6febbaeb9e5bde59d79d66552fae53d54cc1377a19272 ;;
    *) fail "No automatic musl toolchain for $host -> $target" ;;
esac
toolchain="$root/${archive%.tgz}"
cc="$toolchain/bin/$arch-linux-musl-gcc"
cxx="$toolchain/bin/$arch-linux-musl-g++"
ar="$toolchain/bin/$arch-linux-musl-gcc-ar"
pc="$root/x265/install/lib/pkgconfig:$root/aom/lib/pkgconfig"
fingerprint="$(cat "$scripts/musl-sdk.sh" "$scripts/build-static-x265.sh" | cksum)"

ready() {
    [ -f "$root/.ready" ] &&
        [ "$(cat "$root/.ready")" = "$fingerprint" ] &&
        [ -x "$cc" ] && [ -x "$cxx" ] && [ -x "$ar" ] &&
        [ -f "$root/x265/install/lib/libx265.a" ] &&
        [ -f "$root/x265/install/lib/libx265_main10.a" ] &&
        [ -f "$root/x265/install/lib/libx265_main12.a" ] &&
        [ -f "$root/aom/lib/libaom.a" ]
}

download() {
    url="$1"
    destination="$2"
    expected="$3"
    if [ ! -f "$destination" ]; then
        curl --fail --location --retry 3 --retry-all-errors --connect-timeout 15 --max-time 300 \
            --output "$destination.part" "$url"
        digest="$(cmake -E sha512sum "$destination.part")"
        [ "${digest%% *}" = "$expected" ] || fail "Checksum mismatch: $destination.part"
        mv "$destination.part" "$destination"
    fi
    digest="$(cmake -E sha512sum "$destination")"
    [ "${digest%% *}" = "$expected" ] || fail "Checksum mismatch: $destination"
}

case "$action" in
    prepare)
        command -v flock >/dev/null 2>&1 || fail "Missing SDK preparation tool: flock"
        mkdir -p "$root"
        exec 9> "$root/.prepare.lock"
        flock 9
        if ready; then
            printf 'Musl C/C++ and codec SDK already prepared: %s\n' "$root"
            exit 0
        fi
        mkdir -p "$root"
        download "https://musl.cc/$archive" "$root/$archive" "$checksum"
        if [ ! -f "$toolchain/.unpacked" ]; then
            tar -xzf "$root/$archive" -C "$root"
            touch "$toolchain/.unpacked"
        fi
        for tool in "$cc" "$cxx" "$ar"; do
            [ -x "$tool" ] || fail "Missing compiler tool in the musl SDK: $tool"
        done
        download https://storage.googleapis.com/aom-releases/libaom-3.12.1.tar.gz \
            "$root/libaom-3.12.1.tar.gz" \
            27521fe1cffd89a8875552f1758de89c19a47aa1640ee20930ac420a03d964eb9ae10c4b0f55e518c37d4d59f06657aee2bfa84eedad35683648bd658e06da73
        source="$root/aom-source"
        if [ ! -f "$source/.unpacked" ]; then
            mkdir -p "$source"
            tar -xzf "$root/libaom-3.12.1.tar.gz" -C "$source" --strip-components=1
            touch "$source/.unpacked"
        fi
        # NASM 3's -hf lists formats only; AOM also checks optimization options.
        optimization="$source/build/cmake/aom_optimization.cmake"
        sed 's/${CMAKE_ASM_NASM_COMPILER} -hf/${CMAKE_ASM_NASM_COMPILER} -h all/' \
            "$optimization" > "$optimization.tmp"
        mv "$optimization.tmp" "$optimization"
        cmake -S "$source" -B "$root/aom-build" -G Ninja \
            -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$root/aom" \
            -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_SYSTEM_NAME=Linux \
            "-DCMAKE_SYSTEM_PROCESSOR=$arch" \
            "-DCMAKE_C_COMPILER=$cc" "-DCMAKE_CXX_COMPILER=$cxx" \
            -DBUILD_SHARED_LIBS=OFF -DCONFIG_PIC=1 \
            -DENABLE_TESTS=OFF -DENABLE_EXAMPLES=OFF -DENABLE_TOOLS=OFF \
            -DENABLE_DOCS=OFF -DCONFIG_TUNE_VMAF=0
        cmake --build "$root/aom-build" --parallel "$jobs"
        cmake --install "$root/aom-build"
        CC="$cc" CXX="$cxx" X265_SYSTEM_PROCESSOR="$arch" \
            sh "$scripts/build-static-x265.sh" "$root/x265" "$jobs"
        cat > "$root/codec-probe.cpp" <<'EOF'
#include <x265.h>
#include <aom/aomcx.h>
int main() {
    return !(x265_api_get(8) && x265_api_get(10) && x265_api_get(12)
             && aom_codec_av1_cx());
}
EOF
        "$cxx" -static -pthread "$root/codec-probe.cpp" \
            "-I$root/x265/install/include" "-I$root/aom/include" \
            "$root/x265/install/lib/libx265.a" \
            "$root/x265/install/lib/libx265_main10.a" \
            "$root/x265/install/lib/libx265_main12.a" \
            "$root/aom/lib/libaom.a" -lm -o "$root/codec-probe"
        file "$root/codec-probe" | grep -Eq 'statically linked|static-pie linked' ||
            fail "The musl codec SDK did not produce a static executable"
        if [ "$host_arch" = "$arch" ]; then
            "$root/codec-probe"
        fi
        printf '%s\n' "$fingerprint" > "$root/.ready"
        ready || fail "The musl SDK is incomplete after preparation: $root"
        printf 'Prepared native musl C/C++ and codec SDK: %s\n' "$root"
        ;;
    check)
        ready || fail "Missing or outdated musl C/C++ and codec SDK: $root"
        ;;
    run)
        ready || fail "Missing or outdated musl C/C++ and codec SDK: $root"
        key="$(printf '%s' "$target" | tr '-' '_')"
        upper="$(printf '%s' "$key" | tr '[:lower:]' '[:upper:]')"
        exec env -u "PKG_CONFIG_PATH_$key" -u "PKG_CONFIG_PATH_$target" \
            -u TARGET_PKG_CONFIG_PATH \
            "CC_$key=$cc" "CXX_$key=$cxx" "AR_$key=$ar" \
            "CC_$target=$cc" "CXX_$target=$cxx" "AR_$target=$ar" \
            "CARGO_TARGET_${upper}_LINKER=$cc" \
            PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH="$pc" PKG_CONFIG_LIBDIR="$pc" \
            "PKG_CONFIG_LIBDIR_$key=$pc" "PKG_CONFIG_LIBDIR_$target=$pc" \
            PKG_CONFIG_SYSROOT_DIR=/ "PKG_CONFIG_SYSROOT_DIR_$key=/" \
            "PKG_CONFIG_SYSROOT_DIR_$target=/" \
            CMAKE_PREFIX_PATH="$root/x265/install:$root/aom" \
            DNG_MONO_MUSL_ROOT="$root" DNG_MONO_MUSL_TOOLCHAIN="$toolchain" \
            DNG_MONO_MUSL_PROCESSOR="$arch" "$@"
        ;;
    *) fail "Usage: musl-sdk.sh prepare|check|run TARGET JOBS [COMMAND ...]" ;;
esac
