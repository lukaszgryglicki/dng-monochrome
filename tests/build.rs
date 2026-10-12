#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn static_build(host: &str, format: &str, libraries: &str) -> (TempDir, Output) {
    make_build(host, format, libraries, &["static"])
}

fn make_build(host: &str, format: &str, libraries: &str, goals: &[&str]) -> (TempDir, Output) {
    let directory = make_fixture();
    let result = make_command(directory.path(), host, format, libraries)
        .args(goals)
        .output()
        .unwrap();
    (directory, result)
}

fn make_fixture() -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("Makefile"),
        include_str!("../Makefile"),
    )
    .unwrap();
    let tools = directory.path().join("tools");
    fs::create_dir(&tools).unwrap();
    fs::create_dir(directory.path().join("scripts")).unwrap();
    fs::create_dir(directory.path().join("mock-libs")).unwrap();
    for file in ["libstd-fixture.rlib", "libaom.a", "libx265.a"] {
        fs::write(directory.path().join("mock-libs").join(file), []).unwrap();
    }
    fs::write(
        directory.path().join("scripts/requirements.sh"),
        include_str!("../scripts/requirements.sh"),
    )
    .unwrap();
    fs::write(
        directory.path().join("scripts/musl-sdk.sh"),
        "set -eu\n\
         case \"$1\" in\n\
           check) if test \"${MOCK_SDK_MISSING:-0}\" = 1 && ! test -f prepared-sdk; then\n\
                    echo 'Missing musl SDK. Run make requirements.' >&2; exit 1\n\
                  fi ;;\n\
           prepare) printf '%s\\n' \"$*\" >> sdk-calls; touch prepared-sdk ;;\n\
           run) printf '%s\\n' \"$*\" >> sdk-calls; shift 3; exec \"$@\" ;;\n\
           *) exit 1 ;;\n\
         esac\n",
    )
    .unwrap();
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
        (
            "rustc",
            "case \"$1\" in\n\
               -vV) printf 'host: %s\\n' \"$MOCK_HOST\" ;;\n\
               --version) printf 'rustc %s (fixture)\\n' \"${MOCK_RUST_VERSION:-1.98.1}\" ;;\n\
               --print) printf '%s\\n' \"$MOCK_LIBDIR\" ;;\n\
               *) exit 1 ;;\n\
             esac\n",
        ),
        (
            "uname",
            "if test \"$1\" = -m; then printf '%s\\n' \"${MOCK_HOST%%-*}\"; exit; fi\n\
             case \"$MOCK_HOST\" in\n\
               *-freebsd) echo FreeBSD ;;\n\
               *-apple-darwin) echo Darwin ;;\n\
               *) echo Linux ;;\n\
             esac\n",
        ),
        (
            "cmake",
            "test \"$1\" = --version\n\
             printf 'cmake version %s\\n' \"${MOCK_CMAKE_VERSION:-3.31.0}\"\n",
        ),
        (
            "pkg-config",
            "if test \"$1\" = --variable=libdir; then echo \"$MOCK_LIBDIR\"; exit; fi\n\
             for package in \"$@\"; do\n\
               if test \"$package\" = \"${MOCK_MISSING_PACKAGE:-}\"; then\n\
                 echo \"Missing package: $package\" >&2; exit 1\n\
               fi\n\
             done\n",
        ),
        ("ninja", "exit 0\n"),
        ("nasm", "exit 0\n"),
        (
            "rustup",
            "printf '%s\\n' \"$*\" >> rustup-calls\n\
             test \"$1\" = target && test \"$2\" = add\n\
             touch \"$MOCK_LIBDIR/libstd-fixture.rlib\"\n",
        ),
        ("id", "echo 1000\n"),
        (
            "sudo",
            "printf '%s\\n' \"$*\" >> privilege-calls\nexec \"$@\"\n",
        ),
        ("xcode-select", "exit 0\n"),
        (
            "apt-get",
            "printf '%s\\n' \"$*\" >> package-calls\n\
             exit \"${MOCK_PACKAGE_STATUS:-0}\"\n",
        ),
        (
            "pkg",
            "if test \"$1\" = info; then exit 1; fi\n\
             printf '%s\\n' \"$*\" >> package-calls\n\
             exit \"${MOCK_PACKAGE_STATUS:-0}\"\n",
        ),
        (
            "brew",
            "if test \"$1\" = list; then exit 1; fi\n\
             printf '%s\\n' \"$*\" >> package-calls\n\
             exit \"${MOCK_PACKAGE_STATUS:-0}\"\n",
        ),
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
    directory
}

fn make_command(directory: &Path, host: &str, format: &str, libraries: &str) -> Command {
    let mut paths = vec![directory.join("tools")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut command = Command::new("make");
    command
        .args([
            "CARGO=cargo",
            "JOBS=2",
            "STATIC_TARGET=",
            "INSTALL_DIR=installed scripts",
        ])
        .current_dir(directory)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("HOME", directory)
        .env_remove("CARGO_TARGET_DIR")
        .env("MOCK_LIBDIR", directory.join("mock-libs"))
        .env("MOCK_HOST", host)
        .env("MOCK_FORMAT", format)
        .env("MOCK_LIBRARIES", libraries)
        .env("RUSTFLAGS", "-C debuginfo=1");
    command
}

#[test]
fn install_checks_missing_prerequisites_before_either_build_without_installing_packages() {
    for (variable, value, message) in [
        ("MOCK_MISSING_PACKAGE", "aom", "Missing native"),
        ("MOCK_RUST_VERSION", "1.88.0", "Rust 1.89+"),
        ("MOCK_CMAKE_VERSION", "3.21.0", "CMake 3.22+"),
        ("MOCK_SDK_MISSING", "1", "Missing musl SDK"),
    ] {
        let dir = make_fixture();
        let result = make_command(dir.path(), "x86_64-unknown-linux-gnu", "", "")
            .arg("install")
            .env(variable, value)
            .output()
            .unwrap();
        assert!(!result.status.success(), "{variable}: {result:?}");
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains(message), "{error}");
        assert!(error.contains("make requirements"), "{error}");
        assert!(!dir.path().join("calls").exists());
        assert!(!dir.path().join("package-calls").exists());
        assert!(!dir.path().join("privilege-calls").exists());
    }
}

#[test]
fn install_reports_missing_rust_target_before_building() {
    let dir = make_fixture();
    fs::remove_file(dir.path().join("mock-libs/libstd-fixture.rlib")).unwrap();
    let result = make_command(dir.path(), "x86_64-unknown-linux-gnu", "", "")
        .arg("install")
        .output()
        .unwrap();
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("Missing Rust standard library for x86_64-unknown-linux-musl"));
    assert!(error.contains("make requirements"));
    assert!(!dir.path().join("calls").exists());
}

#[test]
fn release_does_not_require_the_musl_sdk() {
    let dir = make_fixture();
    let result = make_command(dir.path(), "x86_64-unknown-linux-gnu", "", "")
        .arg("release")
        .env("MOCK_SDK_MISSING", "1")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(!dir.path().join("sdk-calls").exists());
}

#[test]
fn requirements_installs_platform_packages_and_prepares_only_gnu_linux_musl_sdk() {
    for host in [
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-freebsd",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    ] {
        let dir = make_fixture();
        let result = make_command(dir.path(), host, "", "")
            .arg("requirements")
            .env("MOCK_SDK_MISSING", "1")
            .output()
            .unwrap();
        assert!(result.status.success(), "{host}: {result:?}");
        let packages = fs::read_to_string(dir.path().join("package-calls")).unwrap();
        assert!(packages.contains("cmake"), "{packages}");
        assert!(packages.contains("x265"), "{packages}");
        assert!(packages.contains("aom"), "{packages}");
        assert!(packages.contains("libde265"), "{packages}");
        if host.ends_with("-linux-gnu") {
            assert!(packages.contains("aom-tools"));
            assert!(packages.contains("libnuma-dev"));
            assert_eq!(
                fs::read_to_string(dir.path().join("sdk-calls")).unwrap(),
                "prepare x86_64-unknown-linux-musl 2\n"
            );
        } else {
            assert!(!dir.path().join("sdk-calls").exists());
        }
        assert_eq!(
            dir.path().join("privilege-calls").exists(),
            !host.ends_with("-apple-darwin")
        );
        assert!(!dir.path().join("calls").exists());
    }
}

#[test]
fn requirements_propagates_package_installation_errors_without_building() {
    let dir = make_fixture();
    let result = make_command(dir.path(), "x86_64-unknown-linux-gnu", "", "")
        .arg("requirements")
        .env("MOCK_PACKAGE_STATUS", "23")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!dir.path().join("calls").exists());
    assert!(!dir.path().join("sdk-calls").exists());
}

#[test]
fn requirements_adds_the_missing_selected_rust_target() {
    let dir = make_fixture();
    fs::remove_file(dir.path().join("mock-libs/libstd-fixture.rlib")).unwrap();
    let result = make_command(dir.path(), "x86_64-unknown-linux-gnu", "", "")
        .arg("requirements")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        fs::read_to_string(dir.path().join("rustup-calls")).unwrap(),
        "target add x86_64-unknown-linux-musl\n"
    );
}

fn musl_sdk_fixture() -> (TempDir, std::path::PathBuf) {
    let dir = make_fixture();
    fs::write(
        dir.path().join("scripts/musl-sdk.sh"),
        include_str!("../scripts/musl-sdk.sh"),
    )
    .unwrap();
    fs::write(
        dir.path().join("scripts/build-static-x265.sh"),
        include_str!("../scripts/build-static-x265.sh"),
    )
    .unwrap();
    let root = dir
        .path()
        .join("sdk/muslcc-11.2.1-aom-3.12.1-x265-4.1/x86_64-unknown-linux-musl");
    for file in [
        "x86_64-linux-musl-native/bin/x86_64-linux-musl-gcc",
        "x86_64-linux-musl-native/bin/x86_64-linux-musl-g++",
        "x86_64-linux-musl-native/bin/x86_64-linux-musl-gcc-ar",
        "x265/install/lib/libx265.a",
        "x265/install/lib/libx265_main10.a",
        "x265/install/lib/libx265_main12.a",
        "aom/lib/libaom.a",
    ] {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"fixture").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let fingerprint = Command::new("sh")
        .args([
            "-c",
            "cat scripts/musl-sdk.sh scripts/build-static-x265.sh | cksum",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(fingerprint.status.success());
    fs::write(root.join(".ready"), fingerprint.stdout).unwrap();
    (dir, root)
}

fn musl_sdk_command(dir: &Path, action: &str) -> Command {
    let mut paths = vec![dir.join("tools")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut command = Command::new("sh");
    command
        .args([
            "scripts/musl-sdk.sh",
            action,
            "x86_64-unknown-linux-musl",
            "2",
        ])
        .current_dir(dir)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("HOME", dir)
        .env("DNG_MONO_SDK_ROOT", dir.join("sdk"))
        .env("MOCK_HOST", "x86_64-unknown-linux-gnu");
    command
}

#[test]
fn musl_sdk_check_requires_all_static_archives_and_current_recipe() {
    let (dir, root) = musl_sdk_fixture();
    assert!(
        musl_sdk_command(dir.path(), "check")
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::remove_file(root.join("x265/install/lib/libx265_main12.a")).unwrap();
    let result = musl_sdk_command(dir.path(), "check").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("make requirements"));
    fs::write(root.join("x265/install/lib/libx265_main12.a"), b"archive").unwrap();
    fs::write(root.join(".ready"), b"old recipe").unwrap();
    assert!(
        !musl_sdk_command(dir.path(), "check")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn musl_sdk_scopes_compilers_and_pkg_config_without_changing_rust_flags() {
    let (dir, root) = musl_sdk_fixture();
    let result = musl_sdk_command(dir.path(), "run")
        .arg("env")
        .env("PKG_CONFIG_PATH", "/host/glibc")
        .env("PKG_CONFIG_LIBDIR", "/host/glibc")
        .env("CC_x86_64-unknown-linux-musl", "/host/glibc-gcc")
        .env("PKG_CONFIG_PATH_x86_64-unknown-linux-musl", "/host/glibc")
        .env("PKG_CONFIG_PATH_x86_64_unknown_linux_musl", "/host/glibc")
        .env("TARGET_PKG_CONFIG_PATH", "/host/glibc")
        .env(
            "PKG_CONFIG_SYSROOT_DIR_x86_64-unknown-linux-musl",
            "/host/glibc",
        )
        .env("RUSTFLAGS", "-C target-feature=+crt-static")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let output = String::from_utf8(result.stdout).unwrap();
    let environment: std::collections::HashMap<_, _> = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    for (variable, executable) in [("CC", "gcc"), ("CXX", "g++"), ("AR", "gcc-ar")] {
        let expected = root.join(format!(
            "x86_64-linux-musl-native/bin/x86_64-linux-musl-{executable}"
        ));
        for suffix in ["x86_64_unknown_linux_musl", "x86_64-unknown-linux-musl"] {
            let key = format!("{variable}_{suffix}");
            assert_eq!(environment[key.as_str()], expected.to_str().unwrap());
        }
    }
    assert_eq!(
        environment["CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER"],
        root.join("x86_64-linux-musl-native/bin/x86_64-linux-musl-gcc")
            .to_str()
            .unwrap()
    );
    let pc = format!(
        "{}/x265/install/lib/pkgconfig:{}/aom/lib/pkgconfig",
        root.display(),
        root.display()
    );
    assert_eq!(environment["PKG_CONFIG_PATH"], pc);
    for key in [
        "PKG_CONFIG_PATH_x86_64_unknown_linux_musl",
        "PKG_CONFIG_PATH_x86_64-unknown-linux-musl",
        "TARGET_PKG_CONFIG_PATH",
    ] {
        assert!(
            !environment.contains_key(key),
            "{key} masks bundled libraries"
        );
    }
    for (variable, expected) in [
        ("PKG_CONFIG_LIBDIR", pc.as_str()),
        ("PKG_CONFIG_SYSROOT_DIR", "/"),
    ] {
        for suffix in [
            "",
            "_x86_64_unknown_linux_musl",
            "_x86_64-unknown-linux-musl",
        ] {
            let key = format!("{variable}{suffix}");
            assert_eq!(environment[key.as_str()], expected);
        }
    }
    assert_eq!(environment["PKG_CONFIG_ALLOW_CROSS"], "1");
    assert_eq!(environment["DNG_MONO_MUSL_ROOT"], root.to_str().unwrap());
    assert_eq!(environment["RUSTFLAGS"], "-C target-feature=+crt-static");

    let bundled = dir.path().join("bundled/lib/pkgconfig");
    fs::create_dir_all(&bundled).unwrap();
    fs::write(
        bundled.join("dng-sdk-bundled-test.pc"),
        "Name: dng-sdk-bundled-test\nDescription: bundled fixture\nVersion: 1.0\n",
    )
    .unwrap();
    fs::remove_file(dir.path().join("tools/pkg-config")).unwrap();
    let result = Command::new("pkg-config")
        .args(["--modversion", "dng-sdk-bundled-test"])
        .envs(&environment)
        .env("PKG_CONFIG_PATH", format!("{}:{pc}", bundled.display()))
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "1.0");
}

#[test]
fn musl_sdk_rejects_unverified_toolchains_before_unpacking() {
    let (dir, root) = musl_sdk_fixture();
    fs::remove_file(root.join(".ready")).unwrap();
    fs::write(
        root.join("x86_64-linux-musl-native.tgz"),
        b"invalid archive",
    )
    .unwrap();
    for (tool, body) in [
        ("flock", "exit 0\n"),
        (
            "cmake",
            "test \"$1\" = -E && test \"$2\" = sha512sum\n\
             printf 'incorrect  %s\\n' \"$3\"\n",
        ),
        ("tar", "echo unexpected-unpack >> forbidden-calls; exit 1\n"),
        (
            "curl",
            "echo unexpected-download >> forbidden-calls; exit 1\n",
        ),
    ] {
        let path = dir.path().join("tools").join(tool);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let result = musl_sdk_command(dir.path(), "prepare").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Checksum mismatch"));
    assert!(!root.join(".ready").exists());
    assert!(!dir.path().join("forbidden-calls").exists());
}

#[test]
fn musl_sdk_preserves_aom_feature_checks_with_nasm_2_and_3() {
    let (dir, root) = musl_sdk_fixture();
    let source = root.join("aom-source");
    fs::create_dir_all(source.join("build/cmake")).unwrap();
    fs::write(source.join(".unpacked"), []).unwrap();
    fs::write(root.join("x86_64-linux-musl-native/.unpacked"), []).unwrap();
    fs::write(
        source.join("build/cmake/aom_optimization.cmake"),
        "function(test_nasm)\n\
           execute_process(COMMAND ${CMAKE_ASM_NASM_COMPILER} -hf\n\
                           OUTPUT_VARIABLE nasm_helptext)\n\
           if(NOT \"${nasm_helptext}\" MATCHES \"-Ox\" OR NOT \"${nasm_helptext}\" MATCHES \"elf64\")\n\
             message(FATAL_ERROR \"missing NASM optimization or object format\")\n\
           endif()\n\
         endfunction()\n",
    ).unwrap();
    fs::write(
        source.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.22)\n\
         project(nasm_check NONE)\n\
         include(build/cmake/aom_optimization.cmake)\n\
         test_nasm()\n",
    )
    .unwrap();
    let nasm = dir.path().join("tools/nasm");
    fs::write(
        &nasm,
        "#!/bin/sh\n\
         if test \"$*\" = '-h all' || test \"$MOCK_NASM_STYLE\" = 2; then\n\
           printf 'elf64 -Ox\\n'\n\
         else\n\
           printf 'elf64\\n'\n\
         fi\n",
    )
    .unwrap();
    let configure = |name: &str, style: &str| {
        Command::new("cmake")
            .arg("-S")
            .arg(&source)
            .arg("-B")
            .arg(root.join(name))
            .arg(format!("-DCMAKE_ASM_NASM_COMPILER={}", nasm.display()))
            .env("MOCK_NASM_STYLE", style)
            .output()
            .unwrap()
    };
    assert!(!configure("before", "3").status.success());
    fs::remove_file(root.join(".ready")).unwrap();
    for archive in ["x86_64-linux-musl-native.tgz", "libaom-3.12.1.tar.gz"] {
        fs::write(root.join(archive), b"fixture archive").unwrap();
    }
    for (tool, body) in [
        ("flock", "exit 0\n"),
        (
            "cmake",
            "if test \"$1\" = -S; then exit 17; fi\n\
             test \"$1\" = -E && test \"$2\" = sha512sum\n\
             case \"$3\" in\n\
               *.tgz) echo '44d441ad9aa11a06feddf3daa4c9f53ad7d9ca37af1f5a61379aca07793703d179410cea723c1b7fca94c4de19a321228bdb3656bc5cbdb5e3bea8e2d6dac6c7  archive' ;;\n\
               *.tar.gz) echo '27521fe1cffd89a8875552f1758de89c19a47aa1640ee20930ac420a03d964eb9ae10c4b0f55e518c37d4d59f06657aee2bfa84eedad35683648bd658e06da73  archive' ;;\n\
               *) exit 1 ;;\n\
             esac\n",
        ),
    ] {
        let path = dir.path().join("tools").join(tool);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(
        musl_sdk_command(dir.path(), "prepare")
            .output()
            .unwrap()
            .status
            .code(),
        Some(17)
    );
    assert!(!root.join(".ready").exists());
    for style in ["2", "3"] {
        let result = configure(&format!("after-{style}"), style);
        assert!(result.status.success(), "{style}: {result:?}");
    }
}

#[test]
fn musl_cmake_searches_only_the_prepared_sdk_for_headers_and_libraries() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("heif.cmake"),
        include_str!("../.cargo/heif.cmake"),
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("sdk/aom/include")).unwrap();
    fs::create_dir_all(dir.path().join("host/include")).unwrap();
    fs::write(dir.path().join("sdk/aom/include/sdk-only.h"), []).unwrap();
    fs::write(dir.path().join("host/include/host-only.h"), []).unwrap();
    fs::write(
        dir.path().join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.22)\n\
         project(sdk_check NONE)\n\
         include(${CMAKE_CURRENT_SOURCE_DIR}/heif.cmake)\n\
         set(expected_compiler \"$ENV{DNG_MONO_MUSL_TOOLCHAIN}/bin/x86_64-linux-musl-\")\n\
         if(NOT CMAKE_C_COMPILER STREQUAL \"${expected_compiler}gcc\" OR\n\
            NOT CMAKE_CXX_COMPILER STREQUAL \"${expected_compiler}g++\" OR\n\
            NOT CMAKE_AR STREQUAL \"${expected_compiler}gcc-ar\")\n\
           message(FATAL_ERROR \"musl SDK compiler tools not selected\")\n\
         endif()\n\
         find_path(SDK_HEADER sdk-only.h REQUIRED)\n\
         find_path(HOST_HEADER host-only.h)\n\
         find_program(HOST_SHELL sh REQUIRED)\n\
         if(HOST_HEADER)\n\
           message(FATAL_ERROR \"host headers leaked into musl configuration\")\n\
         endif()\n",
    )
    .unwrap();
    let result = Command::new("cmake")
        .arg("-S")
        .arg(dir.path())
        .arg("-B")
        .arg(dir.path().join("build"))
        .env("CMAKE_PREFIX_PATH", dir.path().join("host"))
        .env("DNG_MONO_MUSL_ROOT", dir.path().join("sdk"))
        .env("DNG_MONO_MUSL_TOOLCHAIN", dir.path().join("toolchain"))
        .env("DNG_MONO_MUSL_PROCESSOR", "x86_64")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn musl_cmake_replaces_cached_host_compilers_without_cleaning() {
    let dir = tempfile::tempdir().unwrap();
    let toolchain = dir.path().join("toolchain");
    fs::create_dir_all(toolchain.join("bin")).unwrap();
    for (name, tool) in [("gcc", "cc"), ("g++", "c++"), ("gcc-ar", "ar")] {
        let path = toolchain.join(format!("bin/x86_64-linux-musl-{name}"));
        fs::write(&path, format!("#!/bin/sh\nexec {tool} \"$@\"\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        dir.path().join("heif.cmake"),
        include_str!("../.cargo/heif.cmake"),
    )
    .unwrap();
    fs::write(
        dir.path().join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.22)\n\
         project(sdk_switch C CXX)\n\
         foreach(setting BUILD_SHARED_LIBS BUILD_TESTING BUILD_DOCUMENTATION WITH_EXAMPLES ENABLE_PLUGIN_LOADING)\n\
           option(${setting} \"\" ON)\n\
           if(${setting})\n\
             message(FATAL_ERROR \"bundled configuration lost: ${setting}\")\n\
           endif()\n\
         endforeach()\n\
         if(NOT CMAKE_INSTALL_PREFIX STREQUAL \"$ENV{OUT_DIR}/libheif_build\" OR\n\
            NOT CMAKE_INSTALL_LIBDIR STREQUAL \"lib\" OR\n\
            NOT CMAKE_BUILD_TYPE STREQUAL \"Release\")\n\
           message(FATAL_ERROR \"bundled configuration: ${CMAKE_INSTALL_PREFIX};${CMAKE_INSTALL_LIBDIR};${CMAKE_BUILD_TYPE}\")\n\
         endif()\n\
         add_library(sample STATIC sample.c sample.cc)\n\
         file(WRITE \"${CMAKE_BINARY_DIR}/compilers\" \"${CMAKE_C_COMPILER}\\n${CMAKE_CXX_COMPILER}\\n${CMAKE_AR}\\n\")\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("sample.c"),
        "int c_value(void) { return 1; }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("sample.cc"),
        "extern \"C\" int cxx_value() { return 2; }\n",
    )
    .unwrap();
    let build = dir.path().join("build");
    for use_sdk in [false, true, true] {
        let mut configure = Command::new("cmake");
        configure
            .arg("-S")
            .arg(dir.path())
            .arg("-B")
            .arg(&build)
            .arg(format!(
                "-DCMAKE_TOOLCHAIN_FILE={}",
                dir.path().join("heif.cmake").display()
            ))
            .arg(format!(
                "-DCMAKE_INSTALL_PREFIX={}/cargo-out/libheif_build",
                dir.path().display()
            ))
            .args([
                "-DCMAKE_INSTALL_LIBDIR=lib",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DBUILD_SHARED_LIBS=OFF",
                "-DBUILD_TESTING=OFF",
                "-DBUILD_DOCUMENTATION=OFF",
                "-DWITH_EXAMPLES=OFF",
                "-DENABLE_PLUGIN_LOADING=OFF",
            ])
            .env("OUT_DIR", dir.path().join("cargo-out"))
            .env("CARGO_PKG_NAME", "libheif-sys")
            .env_remove("DNG_MONO_MUSL_ROOT");
        if use_sdk {
            configure
                .env("DNG_MONO_MUSL_ROOT", dir.path().join("sdk"))
                .env("DNG_MONO_MUSL_TOOLCHAIN", &toolchain)
                .env("DNG_MONO_MUSL_PROCESSOR", "x86_64");
        }
        let result = configure.output().unwrap();
        assert!(result.status.success(), "{result:?}");
        if use_sdk {
            let selected = fs::read_to_string(build.join("compilers")).unwrap();
            let expected = ["gcc", "g++", "gcc-ar"]
                .map(|name| format!("{}/bin/x86_64-linux-musl-{name}\n", toolchain.display()))
                .concat();
            assert_eq!(selected, expected);
        }
        let result = Command::new("cmake")
            .arg("--build")
            .arg(&build)
            .args(["--parallel", "2"])
            .env("OUT_DIR", dir.path().join("cargo-out"))
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    }
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
