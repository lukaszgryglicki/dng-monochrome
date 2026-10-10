SHELL = /bin/sh
CARGO ?= cargo
JOBS ?= 4
TEST_THREADS ?= 4
STATIC_TARGET ?=

all: test build

build: release

release:
	$(CARGO) build --locked --release -j $(JOBS)

debug:
	$(CARGO) build --locked -j $(JOBS)

test:
	RUST_TEST_THREADS=$(TEST_THREADS) $(CARGO) test --locked -j $(JOBS)

test-real:
	@test -n "$(DNG_MONO_SAMPLE)" || { echo "Set DNG_MONO_SAMPLE to an original Leica DNG" >&2; exit 1; }
	DNG_MONO_SAMPLE="$(DNG_MONO_SAMPLE)" RUST_TEST_THREADS=$(TEST_THREADS) $(CARGO) test --locked --release -j $(JOBS) --test decoder --test cli original_leica -- --ignored --nocapture

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --locked --all-targets -j $(JOBS) -- -D warnings

static:
	@set -eu; \
	target="$(STATIC_TARGET)"; \
	if [ -z "$$target" ]; then \
		host="$$(rustc -vV | sed -n 's/^host: //p')"; \
		case "$$host" in \
			*-linux-gnu) target="$${host%-gnu}-musl" ;; \
			*-linux-musl|*-freebsd|*-apple-darwin) target="$$host" ;; \
			*) echo "Set STATIC_TARGET to a static-capable Rust target" >&2; exit 1 ;; \
		esac; \
	fi; \
	case "$$target" in \
		*-apple-darwin) link_flags="-C prefer-dynamic=no" ;; \
		*-freebsd) \
			link_flags="-C target-feature=+crt-static -C link-arg=-Wl,-Bstatic -C link-arg=-lcxxrt"; \
			sh scripts/build-static-x265.sh target/static/x265 $(JOBS); \
			PKG_CONFIG_PATH="$$(pwd)/target/static/x265/install/lib/pkgconfig$${PKG_CONFIG_PATH:+:$$PKG_CONFIG_PATH}"; \
			export PKG_CONFIG_PATH ;; \
		*) link_flags="-C target-feature=+crt-static" ;; \
	esac; \
	DNG_MONO_STATIC=1 SYSTEM_DEPS_LIBHEIF_LINK=static PKG_CONFIG_ALL_STATIC=1 \
		CMAKE_BUILD_PARALLEL_LEVEL=$(JOBS) CARGO_TARGET_DIR=target/static \
		RUSTFLAGS="$${RUSTFLAGS:+$$RUSTFLAGS }$$link_flags" \
		$(CARGO) build --locked --release --target "$$target" -j $(JOBS); \
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

clean:
	$(CARGO) clean

.PHONY: all build release debug test test-real fmt lint static clean
