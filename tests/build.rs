#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Output},
};
use tempfile::TempDir;

fn static_build(host: &str, format: &str, libraries: &str) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("Makefile"),
        include_str!("../Makefile"),
    )
    .unwrap();
    let tools = directory.path().join("tools");
    fs::create_dir(&tools).unwrap();
    for (name, body) in [
        ("rustc", "printf 'host: %s\\n' \"$MOCK_HOST\"\n"),
        ("file", "printf '%s\\n' \"$MOCK_FORMAT\"\n"),
        ("otool", "printf '%s\\n' \"$MOCK_LIBRARIES\"\n"),
        (
            "cargo",
            "printf '%s\\n' \"$RUSTFLAGS\" > flags\n\
             printf '%s\\n' \"$@\" > arguments\n\
             test \"$CARGO_TARGET_DIR\" = target/static\n\
             while test \"$#\" -gt 0; do\n\
               if test \"$1\" = --target; then shift; target=\"$1\"; fi\n\
               shift\n\
             done\n\
             mkdir -p \"target/static/$target/release\"\n\
             : > \"target/static/$target/release/dng-monochrome\"\n",
        ),
    ] {
        let path = tools.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![tools];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let result = Command::new("make")
        .args(["static", "CARGO=cargo", "JOBS=2", "STATIC_TARGET="])
        .current_dir(directory.path())
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("MOCK_HOST", host)
        .env("MOCK_FORMAT", format)
        .env("MOCK_LIBRARIES", libraries)
        .env("RUSTFLAGS", "-C debuginfo=1")
        .output()
        .unwrap();
    (directory, result)
}

#[test]
fn static_target_preserves_linux_and_freebsd_crt_linking() {
    for (host, target, format) in [
        (
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "ELF 64-bit LSB pie executable, static-pie linked",
        ),
        (
            "x86_64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
            "ELF 64-bit LSB executable, statically linked",
        ),
        (
            "x86_64-unknown-freebsd",
            "x86_64-unknown-freebsd",
            "ELF 64-bit LSB executable, statically linked",
        ),
    ] {
        let (directory, result) = static_build(host, format, "");
        assert!(result.status.success(), "{:?}", result);
        assert_eq!(
            fs::read_to_string(directory.path().join("flags")).unwrap(),
            "-C debuginfo=1 -C target-feature=+crt-static\n"
        );
        let arguments = fs::read_to_string(directory.path().join("arguments")).unwrap();
        assert!(arguments.contains(&format!("--target\n{target}\n")));
    }
}

#[test]
fn macos_static_target_uses_static_rust_and_only_apple_dynamic_libraries() {
    let libraries = "program:\n\
        /usr/lib/libSystem.B.dylib (compatibility version 1.0.0)\n\
        /System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation (compatibility version 150.0.0)";
    for host in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let (directory, result) = static_build(host, "Mach-O 64-bit arm64 executable", libraries);
        assert!(result.status.success(), "{:?}", result);
        assert_eq!(
            fs::read_to_string(directory.path().join("flags")).unwrap(),
            "-C debuginfo=1 -C prefer-dynamic=no\n"
        );
        assert!(
            String::from_utf8_lossy(&result.stdout)
                .contains("Apple system libraries remain dynamic")
        );
    }
}

#[test]
fn static_target_rejects_dynamic_elf_and_non_system_macos_dependencies() {
    for (host, format, libraries) in [
        (
            "x86_64-unknown-linux-gnu",
            "ELF 64-bit LSB executable, dynamically linked",
            "",
        ),
        (
            "aarch64-apple-darwin",
            "Mach-O 64-bit arm64 executable",
            "program:\n/usr/local/lib/libextra.dylib (compatibility version 1.0.0)",
        ),
        (
            "aarch64-apple-darwin",
            "ELF 64-bit executable, statically linked",
            "program:\n/usr/lib/libSystem.B.dylib (compatibility version 1.0.0)",
        ),
    ] {
        let (_directory, result) = static_build(host, format, libraries);
        assert!(!result.status.success(), "{:?}", result);
    }
}
