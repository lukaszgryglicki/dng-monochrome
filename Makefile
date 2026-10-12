SHELL = /bin/sh
CARGO ?= cargo
JOBS ?= 4
TEST_THREADS ?= 4
STATIC_TARGET ?=
INSTALL_DIR ?= /data/scripts
BUILD_PATH = $(HOME)/.cargo/bin:$(PATH):/opt/homebrew/bin:/usr/local/bin
CHECK_REQUIREMENTS = PATH="$(BUILD_PATH)" CARGO="$(CARGO)" JOBS="$(JOBS)" \
	STATIC_TARGET="$(STATIC_TARGET)" sh scripts/requirements.sh check "$(MAKECMDGOALS) $(.TARGETS)"

RESOLVE_STATIC_TARGET = \
	export PATH="$(BUILD_PATH)"; \
	target="$$(STATIC_TARGET="$(STATIC_TARGET)" sh scripts/requirements.sh target)"; \
	host="$$(rustc -vV | sed -n 's/^host: //p')"

all: test build

build: release

requirements:
	@PATH="$(BUILD_PATH)" CARGO="$(CARGO)" JOBS="$(JOBS)" STATIC_TARGET="$(STATIC_TARGET)" \
		sh scripts/requirements.sh install

release:
	@$(CHECK_REQUIREMENTS)
	PATH="$(BUILD_PATH)" $(CARGO) build --locked --release -j $(JOBS)

debug:
	@$(CHECK_REQUIREMENTS)
	PATH="$(BUILD_PATH)" $(CARGO) build --locked -j $(JOBS)

test:
	@$(CHECK_REQUIREMENTS)
	PATH="$(BUILD_PATH)" RUST_TEST_THREADS=$(TEST_THREADS) $(CARGO) test --locked -j $(JOBS)

test-real:
	@test -n "$(DNG_MONO_SAMPLE)" || { echo "Set DNG_MONO_SAMPLE to an original Leica DNG" >&2; exit 1; }
	@$(CHECK_REQUIREMENTS)
	PATH="$(BUILD_PATH)" DNG_MONO_SAMPLE="$(DNG_MONO_SAMPLE)" RUST_TEST_THREADS=$(TEST_THREADS) $(CARGO) test --locked --release -j $(JOBS) --test decoder --test cli original_leica -- --ignored --nocapture

fmt:
	PATH="$(BUILD_PATH)" $(CARGO) fmt --all

lint:
	@$(CHECK_REQUIREMENTS)
	PATH="$(BUILD_PATH)" $(CARGO) fmt --all -- --check
	PATH="$(BUILD_PATH)" $(CARGO) clippy --locked --all-targets -j $(JOBS) -- -D warnings

static:
	@$(CHECK_REQUIREMENTS)
	@set -eu; \
	$(RESOLVE_STATIC_TARGET); \
	set --; \
	case "$$host:$$target" in \
		*-linux-gnu:*-linux-musl) set -- sh scripts/musl-sdk.sh run "$$target" $(JOBS) ;; \
	esac; \
	case "$$target" in \
		*-apple-darwin) link_flags="-C prefer-dynamic=no" ;; \
		*-freebsd) \
			link_flags="-C target-feature=+crt-static -C link-arg=-Wl,-Bstatic -C link-arg=-lcxxrt"; \
			sh scripts/build-static-x265.sh target/static/x265 $(JOBS); \
			PKG_CONFIG_PATH="$$(pwd)/target/static/x265/install/lib/pkgconfig$${PKG_CONFIG_PATH:+:$$PKG_CONFIG_PATH}"; \
			export PKG_CONFIG_PATH ;; \
		*-linux-musl) \
			link_flags="-C target-feature=+crt-static"; \
			if [ "$$#" -eq 0 ]; then \
				sh scripts/build-static-x265.sh target/static/x265 $(JOBS); \
				PKG_CONFIG_PATH="$$(pwd)/target/static/x265/install/lib/pkgconfig$${PKG_CONFIG_PATH:+:$$PKG_CONFIG_PATH}"; \
				export PKG_CONFIG_PATH; \
			fi ;; \
		*) link_flags="-C target-feature=+crt-static" ;; \
	esac; \
	DNG_MONO_STATIC=1 SYSTEM_DEPS_LIBHEIF_LINK=static PKG_CONFIG_ALL_STATIC=1 \
		CMAKE_BUILD_PARALLEL_LEVEL=$(JOBS) CARGO_TARGET_DIR=target/static \
		RUSTFLAGS="$${RUSTFLAGS:+$$RUSTFLAGS }$$link_flags" \
		"$$@" $(CARGO) build --locked --release --target "$$target" -j $(JOBS); \
	binary="target/static/$$target/release/dng-monochrome"; \
	if [ "$${target%-apple-darwin}" != "$$target" ]; then \
		file "$$binary" | grep -Eq 'Mach-O.*executable' || \
			{ echo "Build did not produce a macOS executable: $$binary" >&2; exit 1; }; \
		libraries="$$(otool -L "$$binary")"; \
		printf '%s\n' "$$libraries" | awk 'NR > 1 && $$1 !~ /^\/(usr\/lib|System\/Library)\// { bad = 1 } END { exit bad || NR < 2 }' || \
			{ echo "macOS executable has non-system dynamic dependencies: $$libraries" >&2; exit 1; }; \
		printf 'Standalone macOS executable (Rust dependencies static; Apple system libraries remain dynamic): %s\n' "$$binary"; \
		exit 0; \
	fi; \
	file "$$binary" | grep -Eq 'statically linked|static-pie linked' || \
		{ echo "Build did not produce a static executable: $$binary" >&2; exit 1; }; \
	printf 'Static executable: %s\n' "$$binary"

install: release static
	@set -eu; \
	$(RESOLVE_STATIC_TARGET); \
	install -m 755 target/release/dng-monochrome dng-monochrome; \
	install -m 755 "target/static/$$target/release/dng-monochrome" dng-monochrome.static; \
	mkdir -p "$(INSTALL_DIR)"; \
	install -m 755 dng-monochrome.static "$(INSTALL_DIR)/dng-monochrome"

clean:
	PATH="$(BUILD_PATH)" $(CARGO) clean

.PHONY: all build requirements release debug test test-real fmt lint static install clean
