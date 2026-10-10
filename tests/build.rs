#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Output},
};
use tempfile::TempDir;

fn static_build(host: &str, format: &str, libraries: &str) -> (TempDir, Output) {
    make_build(host, format, libraries, &["static"])
}

fn make_build(host: &str, format: &str, libraries: &str, goals: &[&str]) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("Makefile"),
        include_str!("../Makefile"),
    )
    .unwrap();
    let tools = directory.path().join("tools");
    fs::create_dir(&tools).unwrap();
    fs::create_dir(directory.path().join("scripts")).unwrap();
    fs::write(
        directory.path().join("scripts/build-static-x265.sh"),
        "set -eu\n\
         test \"$1\" = target/static/x265\n\
         test \"$2\" = 2\n\
         mkdir -p \"$1/install/lib/pkgconfig\"\n\
         : > \"$1/install/lib/pkgconfig/x265.pc\"\n",
    )
    .unwrap();
    for (name, body) in [
        ("rustc", "printf 'host: %s\\n' \"$MOCK_HOST\"\n"),
        ("file", "printf '%s\\n' \"$MOCK_FORMAT\"\n"),
        ("otool", "printf '%s\\n' \"$MOCK_LIBRARIES\"\n"),
        (
            "cargo",
            "printf '%s %s\\n' \"${CARGO_TARGET_DIR:-target}\" \"$*\" >> calls\n\
             if test \"$1\" = clean; then\n\
               rm -rf target\n\
               exit 0\n\
             fi\n\
             if test \"${CARGO_TARGET_DIR:-target}\" = target; then\n\
               mkdir -p target/release\n\
               printf 'dynamic executable\\n' > target/release/dng-monochrome\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$RUSTFLAGS\" > flags\n\
             printf '%s\\n' \"$@\" > arguments\n\
             test \"$CARGO_TARGET_DIR\" = target/static\n\
             test \"$DNG_MONO_STATIC\" = 1\n\
             test \"$SYSTEM_DEPS_LIBHEIF_LINK\" = static\n\
             test \"$PKG_CONFIG_ALL_STATIC\" = 1\n\
             test \"$CMAKE_BUILD_PARALLEL_LEVEL\" = 2\n\
             if test \"$MOCK_HOST\" = x86_64-unknown-freebsd; then\n\
               case \"$PKG_CONFIG_PATH\" in \"$PWD/target/static/x265/install/lib/pkgconfig\"*) ;; *) exit 1 ;; esac\n\
               test -f target/static/x265/install/lib/pkgconfig/x265.pc\n\
             fi\n\
             while test \"$#\" -gt 0; do\n\
               if test \"$1\" = --target; then shift; target=\"$1\"; fi\n\
               shift\n\
             done\n\
             mkdir -p \"target/static/$target/release\"\n\
             printf 'static:%s\\n' \"$target\" > \"target/static/$target/release/dng-monochrome\"\n",
        ),
    ] {
        let path = tools.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![tools];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let result = Command::new("make")
        .args([
            "CARGO=cargo",
            "JOBS=2",
            "STATIC_TARGET=",
            "INSTALL_DIR=installed scripts",
        ])
        .args(goals)
        .current_dir(directory.path())
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env_remove("CARGO_TARGET_DIR")
        .env("MOCK_HOST", host)
        .env("MOCK_FORMAT", format)
        .env("MOCK_LIBRARIES", libraries)
        .env("RUSTFLAGS", "-C debuginfo=1")
        .output()
        .unwrap();
    (directory, result)
}

#[test]
fn install_uses_selected_target_and_clean_preserves_all_three_executables() {
    assert!(include_str!("../Makefile").contains("INSTALL_DIR ?= /data/scripts"));
    for (host, selection, target) in [
        ("x86_64-unknown-linux-gnu", "", "x86_64-unknown-linux-musl"),
        ("x86_64-unknown-linux-musl", "", "x86_64-unknown-linux-musl"),
        ("x86_64-unknown-freebsd", "", "x86_64-unknown-freebsd"),
        ("aarch64-apple-darwin", "", "aarch64-apple-darwin"),
        ("x86_64-apple-darwin", "", "x86_64-apple-darwin"),
        (
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ),
    ] {
        let format = if target.ends_with("-apple-darwin") {
            "Mach-O 64-bit executable"
        } else {
            "ELF 64-bit executable, statically linked"
        };
        for goals in [
            &["install"][..],
            &["release", "static", "install", "clean"][..],
        ] {
            let selection = format!("STATIC_TARGET={selection}");
            let mut arguments = goals.to_vec();
            arguments.push(&selection);
            let (directory, result) = make_build(
                host,
                format,
                "program:\n/usr/lib/libSystem.B.dylib",
                &arguments,
            );
            assert!(result.status.success(), "{host}: {result:?}");
            let expected_static = format!("static:{target}\n");
            for (name, expected) in [
                ("dng-monochrome", "dynamic executable\n"),
                ("dng-monochrome.static", expected_static.as_str()),
                ("installed scripts/dng-monochrome", expected_static.as_str()),
            ] {
                let path = directory.path().join(name);
                assert_eq!(fs::read_to_string(&path).unwrap(), expected);
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }
            assert_eq!(
                fs::read_to_string(directory.path().join("calls"))
                    .unwrap()
                    .lines()
                    .filter(|line| line.contains(" build "))
                    .count(),
                2
            );
            assert_eq!(
                directory.path().join("target").exists(),
                !goals.contains(&"clean")
            );
            assert!(
                !directory
                    .path()
                    .join("installed scripts/dng-monochrome.static")
                    .exists()
            );
        }
    }
}

#[test]
fn install_reports_destination_errors() {
    let (_directory, result) = make_build(
        "x86_64-unknown-linux-musl",
        "ELF executable, statically linked",
        "",
        &["install", "INSTALL_DIR=Makefile"],
    );
    assert!(!result.status.success());
    assert!(!result.stderr.is_empty());
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
        let expected = if target.ends_with("-freebsd") {
            "-C debuginfo=1 -C target-feature=+crt-static -C link-arg=-Wl,-Bstatic -C link-arg=-lcxxrt\n"
        } else {
            "-C debuginfo=1 -C target-feature=+crt-static\n"
        };
        assert_eq!(
            fs::read_to_string(directory.path().join("flags")).unwrap(),
            expected
        );
        let arguments = fs::read_to_string(directory.path().join("arguments")).unwrap();
        assert!(arguments.contains(&format!("--target\n{target}\n")));
    }
}

fn x265_build(checksum: &str) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("x265");
    let source = root.join("x265_4.1");
    let tools = directory.path().join("tools");
    fs::create_dir_all(source.join("source")).unwrap();
    fs::create_dir(&tools).unwrap();
    fs::write(root.join("x265_4.1.tar.gz"), b"cached source").unwrap();
    fs::write(source.join(".unpacked"), b"").unwrap();
    fs::write(source.join("COPYING"), b"license").unwrap();
    fs::write(
        source.join("source/CMakeLists.txt"),
        "cmake_policy(SET CMP0025 OLD)\ncmake_policy(SET CMP0054 OLD)\n",
    )
    .unwrap();
    let script = directory.path().join("build-static-x265.sh");
    fs::write(&script, include_str!("../scripts/build-static-x265.sh")).unwrap();
    for (name, body) in [
        ("curl", "echo 'unexpected download' >&2; exit 1\n"),
        (
            "cmake",
            "if test \"$1\" = -E && test \"$2\" = sha256sum; then\n\
               printf '%s  %s\\n' \"$MOCK_CHECKSUM\" \"$3\"\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$*\" >> \"$MOCK_ROOT/commands\"\n\
                 case \"$1\" in\n\
                   --build) printf archive > \"$2/libx265.a\" ;;\n\
                   --install)\n\
                     mkdir -p \"$MOCK_ROOT/install/lib/pkgconfig\"\n\
                     printf 'Libs: -lx265\\nLibs.private: -lc++ -lgcc_s -lm -lgcc_s\\n' > \"$MOCK_ROOT/install/lib/pkgconfig/x265.pc\" ;;\n\
                   -S)\n\
                     while test \"$#\" -gt 0; do\n\
                       if test \"$1\" = -B; then shift; mkdir -p \"$1\"; fi\n\
                       shift\n\
                     done ;;\n\
                   *) exit 1 ;;\n\
                 esac\n",
        ),
    ] {
        let path = tools.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![tools];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let result = Command::new("sh")
        .arg(script)
        .arg(&root)
        .arg("2")
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("MOCK_ROOT", &root)
        .env("MOCK_CHECKSUM", checksum)
        .output()
        .unwrap();
    (directory, result)
}

#[test]
fn freebsd_x265_recipe_builds_all_depths_and_complete_static_metadata() {
    let (directory, result) =
        x265_build("a31699c6a89806b74b0151e5e6a7df65de4b49050482fe5ebf8a4379d7af8f29");
    assert!(result.status.success(), "{:?}", result);
    let root = directory.path().join("x265");
    assert_eq!(
        fs::read_to_string(root.join("x265_4.1/source/CMakeLists.txt")).unwrap(),
        "cmake_policy(SET CMP0025 NEW)\ncmake_policy(SET CMP0054 NEW)\n",
    );
    let commands = fs::read_to_string(root.join("commands")).unwrap();
    let configurations: Vec<_> = commands
        .lines()
        .filter(|line| line.starts_with("-S "))
        .collect();
    assert_eq!(configurations.len(), 3);
    for line in &configurations {
        assert!(line.contains("-DENABLE_ASSEMBLY=ON"));
        assert!(line.contains("-DENABLE_SHARED=OFF"));
        assert!(line.contains("-DCMAKE_BUILD_TYPE=Release"));
    }
    assert!(configurations[0].contains("-DHIGH_BIT_DEPTH=ON -DMAIN12=ON -DEXPORT_C_API=OFF"));
    assert!(configurations[1].contains("-DHIGH_BIT_DEPTH=ON -DMAIN12=OFF -DEXPORT_C_API=OFF"));
    assert!(configurations[2].contains("-DHIGH_BIT_DEPTH=OFF -DEXPORT_C_API=ON"));
    assert!(configurations[2].contains("-DLINKED_10BIT=ON -DLINKED_12BIT=ON"));
    assert!(configurations[2].contains("-DEXTRA_LIB=x265_main10;x265_main12"));
    assert_eq!(
        commands
            .matches("--target x265-static --parallel 2")
            .count(),
        3
    );
    for bits in [10, 12] {
        assert_eq!(
            fs::read(root.join(format!("install/lib/libx265_main{bits}.a"))).unwrap(),
            b"archive"
        );
    }
    assert_eq!(
        fs::read_to_string(root.join("install/lib/pkgconfig/x265.pc")).unwrap(),
        "Libs: -lx265\nLibs.private: -lx265_main10 -lx265_main12 -lc++ -lgcc_eh -lm -lgcc_eh\n"
    );
    assert_eq!(
        fs::read(root.join("install/COPYING.x265")).unwrap(),
        b"license"
    );
}

#[test]
fn freebsd_x265_recipe_rejects_unverified_sources_before_building() {
    let (directory, result) = x265_build("incorrect");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("checksum mismatch"));
    assert!(!directory.path().join("x265/commands").exists());
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
