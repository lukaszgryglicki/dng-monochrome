#!/bin/sh
set -eu

PATH="$HOME/.cargo/bin:$PATH:/opt/homebrew/bin:/usr/local/bin"
export PATH
action="${1:-check}"
goals="${2:-}"
jobs="${JOBS:-4}"
os="$(uname -s)"

fail() {
    printf '%s\nRun make requirements before building/installing.\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "Missing build tool: $1"
}

as_root() {
    if [ "$(id -u)" = 0 ]; then
        "$@"
    else
        need sudo
        sudo "$@"
    fi
}

have_rust() {
    command -v "${CARGO:-cargo}" >/dev/null 2>&1 &&
        command -v rustc >/dev/null 2>&1 &&
        rustc --version | awk 'NR == 1 { split($2, v, "."); ok = v[1] > 1 || (v[1] == 1 && v[2] >= 89) } END { exit !ok }'
}

install_packages() {
    case "$os" in
        FreeBSD)
            packages="cmake pkgconf x265 aom libde265 ninja curl"
            case "$(uname -m)" in amd64|x86_64|i?86) packages="$packages nasm" ;; esac
            if ! have_rust; then packages="$packages rust"; fi
            missing=""
            for package in $packages; do
                if ! pkg info -e "$package"; then missing="$missing $package"; fi
            done
            if [ -n "$missing" ]; then as_root pkg install -y $missing; fi
            ;;
        Darwin)
            xcode-select -p >/dev/null 2>&1 ||
                fail "Install Apple's Command Line Tools with xcode-select --install first."
            need brew
            packages="cmake pkgconf x265 aom libde265 ninja"
            if ! have_rust; then packages="$packages rust"; fi
            missing=""
            for package in $packages; do
                if ! brew list --versions "$package" >/dev/null 2>&1; then missing="$missing $package"; fi
            done
            if [ -n "$missing" ]; then HOMEBREW_NO_AUTO_UPDATE=1 brew install $missing; fi
            ;;
        Linux)
            if command -v apt-get >/dev/null 2>&1; then
                as_root apt-get update
                as_root apt-get install -y --no-install-recommends --no-upgrade \
                    build-essential cmake pkg-config ninja-build nasm curl ca-certificates \
                    file python3 perl util-linux libx265-dev libaom-dev libde265-dev aom-tools libnuma-dev
            elif command -v apk >/dev/null 2>&1; then
                as_root apk add build-base cmake pkgconf ninja nasm curl ca-certificates \
                    file python3 perl linux-headers x265-dev aom-dev aom-static \
                    libde265-dev numactl-dev
            else
                fail "Automatic Linux packages require apt-get (Debian/Ubuntu) or apk (Alpine). See README.md."
            fi
            ;;
        *) fail "Unsupported requirements platform: $os" ;;
    esac
}

install_rust() {
    if [ "${1:-}" != force ] && have_rust; then return; fi
    if ! command -v rustup >/dev/null 2>&1; then
        need curl
        installer="$(mktemp)"
        trap 'rm -f "$installer"' 0
        curl --fail --location --retry 3 --retry-all-errors --output "$installer" https://sh.rustup.rs
        sh "$installer" -y --no-modify-path --profile minimal --component rustfmt,clippy
        rm -f "$installer"
        trap - 0
    else
        rustup toolchain install stable --profile minimal --component rustfmt,clippy
        if ! have_rust; then rustup override set stable; fi
    fi
    have_rust || fail "Rust 1.89+ and Cargo are required; check the active rustup override."
}

resolve_target() {
    need rustc
    target="${STATIC_TARGET:-}"
    if [ -z "$target" ]; then
        host="$(rustc -vV | sed -n 's/^host: //p')"
        case "$host" in
            *-linux-gnu) target="${host%-gnu}-musl" ;;
            *-linux-musl|*-freebsd|*-apple-darwin) target="$host" ;;
            *) fail "Set STATIC_TARGET to a static-capable Rust target" ;;
        esac
    fi
}

have_target() {
    libdir="$(rustc --print target-libdir --target "$target")" || return 1
    for library in "$libdir"/libstd-*.rlib; do
        if [ -f "$library" ]; then return 0; fi
    done
    return 1
}

check_native() {
    for tool in "${CARGO:-cargo}" rustc cc c++ cmake pkg-config make; do need "$tool"; done
    have_rust || fail "Rust 1.89+ is required."
    cmake --version | awk 'NR == 1 { split($3, v, "."); ok = v[1] > 3 || (v[1] == 3 && v[2] >= 22) } END { exit !ok }' ||
        fail "CMake 3.22+ is required."
    pkg-config --print-errors --exists x265 aom libde265 ||
        fail "Missing native x265, libaom or libde265 development files."
}

check_static() {
    resolve_target
    have_target || fail "Missing Rust standard library for $target (rustup target add $target)."
    need file
    host="$(rustc -vV | sed -n 's/^host: //p')"
    case "$host:$target" in
        *-linux-gnu:*-linux-musl)
            sh scripts/musl-sdk.sh check "$target" "$jobs"
            ;;
        *)
            case "$target" in
                *-freebsd|*-linux-musl)
                    for tool in ninja curl; do need "$tool"; done
                    case "$target" in x86_64-*|i?86-*) need nasm ;; esac
                    ;;
                *-apple-darwin) need otool ;;
            esac
            libdir="$(pkg-config --variable=libdir aom)"
            [ -f "$libdir/libaom.a" ] || fail "Missing static libaom archive: $libdir/libaom.a"
            case "$target" in
                *-apple-darwin)
                    libdir="$(pkg-config --variable=libdir x265)"
                    [ -f "$libdir/libx265.a" ] || fail "Missing static x265 archive: $libdir/libx265.a"
                    ;;
            esac
            ;;
    esac
}

case "$action" in
    target)
        resolve_target
        printf '%s\n' "$target"
        ;;
    install)
        install_packages
        install_rust
        check_native
        resolve_target
        if ! have_target; then
            if ! command -v rustup >/dev/null 2>&1; then install_rust force; fi
            rustup target add "$target"
        fi
        host="$(rustc -vV | sed -n 's/^host: //p')"
        case "$host:$target" in
            *-linux-gnu:*-linux-musl) sh scripts/musl-sdk.sh prepare "$target" "$jobs" ;;
        esac
        check_static
        printf 'Requirements ready. Run make install.\n'
        ;;
    check)
        check_native
        case " $goals " in
            *" install "*|*" static "*) check_static ;;
        esac
        ;;
    *) fail "Usage: requirements.sh install|check|target [MAKE_GOALS]" ;;
esac
