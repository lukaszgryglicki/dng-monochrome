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
			*-linux-musl|*-freebsd) target="$$host" ;; \
			*) echo "Set STATIC_TARGET to a static-capable Rust target" >&2; exit 1 ;; \
		esac; \
	fi; \
	CARGO_TARGET_DIR=target/static RUSTFLAGS="$${RUSTFLAGS:+$$RUSTFLAGS }-C target-feature=+crt-static" \
		$(CARGO) build --locked --release --target "$$target" -j $(JOBS); \
	binary="target/static/$$target/release/dng-monochrome"; \
	file "$$binary" | grep -Eq 'statically linked|static-pie linked' || \
		{ echo "Build did not produce a static executable: $$binary" >&2; exit 1; }; \
	printf 'Static executable: %s\n' "$$binary"

clean:
	$(CARGO) clean

.PHONY: all build release debug test test-real fmt lint static clean
