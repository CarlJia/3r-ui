# 3r-ui — build commands
#
# Quick reference:
#   make build             # release build for the host (whatever arch you're on)
#   make build-linux-x64   # x86_64-unknown-linux-musl release
#   make build-linux-arm64 # aarch64-unknown-linux-musl release
#   make test              # cargo test (ignored e2e excluded)
#   make test-e2e          # cargo test --release -- --ignored
#   make clippy fmt clean  # lint / format / tidy
#
# Cross builds need a Linux sysroot. On macOS the supported paths are:
#   1. `cross` + Docker or podman-machine (see notes below)
#   2. A real Linux machine or VM — `make build-linux-x64` from there
#   3. CI: push to a branch, the release.yml workflow builds and uploads the
#      binary as a workflow artifact (see .github/workflows/release.yml).
#
# `cross` is the most ergonomic path when Docker or a running podman machine
# is available. Without a container runtime, fall back to (2) or (3).

CARGO ?= cargo
TARGET_X64 ?= x86_64-unknown-linux-musl
TARGET_ARM64 ?= aarch64-unknown-linux-musl
CROSS ?= cross
CROSS_ENGINE ?= $(shell command -v podman >/dev/null && echo podman || echo docker)

.PHONY: build
build:
	$(CARGO) build --release

.PHONY: build-debug
build-debug:
	$(CARGO) build

# Cross build via cross(1). Requires Docker or a running `podman machine`.
.PHONY: build-linux-x64
build-linux-x64:
	CROSS_CONTAINER_ENGINE=$(CROSS_ENGINE) \
		$(CROSS) build --release --target $(TARGET_X64)

.PHONY: build-linux-arm64
build-linux-arm64:
	CROSS_CONTAINER_ENGINE=$(CROSS_ENGINE) \
		$(CROSS) build --release --target $(TARGET_ARM64)

# Cross for glibc instead of musl. Use this when targeting distros whose
# libc expects dynamic linking (musl works everywhere statically; glibc is
# sometimes preferred for debug symbols or distro package compatibility).
.PHONY: build-linux-x64-gnu
build-linux-x64-gnu:
	CROSS_CONTAINER_ENGINE=$(CROSS_ENGINE) \
		$(CROSS) build --release --target x86_64-unknown-linux-gnu

.PHONY: build-all-linux
build-all-linux: build-linux-x64 build-linux-arm64 build-linux-x64-gnu

# Build every target on a Linux machine or VM. Direct cargo, no container.
.PHONY: build-linux-native
build-linux-native:
	$(CARGO) build --release --target $(TARGET_X64)
	$(CARGO) build --release --target $(TARGET_ARM64)

.PHONY: test
test:
	$(CARGO) test

.PHONY: test-e2e
test-e2e:
	$(CARGO) test --release -- --ignored --test-threads=1

.PHONY: install
install:
	sudo ./install.sh

.PHONY: install-dry-run
install-dry-run:
	./install.sh --dry-run

.PHONY: clippy
clippy:
	$(CARGO) clippy --all-targets -- -D warnings

.PHONY: fmt
fmt:
	$(CARGO) fmt

.PHONY: fmt-check
fmt-check:
	$(CARGO) fmt --check

.PHONY: clean
clean:
	$(CARGO) clean

# Local one-shot launcher (scripts/dev.sh). Keeps state under .run/dev so
# admin password + inbound + users survive across rebuilds. Override with
# DATA_DIR=/tmp/foo or pass FRESH=1 to wipe and start over.
.PHONY: dev dev-rebuild
dev:
	$(MAKE) build
	scripts/dev.sh --data-dir .run/dev

dev-rebuild:
	$(MAKE) build
	scripts/dev.sh --rebuild --data-dir .run/dev

# Print where the linux binaries landed after a cross build.
.PHONY: linux-output
linux-output:
	@echo "x86_64:  target/$(TARGET_X64)/release/3r-ui"
	@echo "arm64:   target/$(TARGET_ARM64)/release/3r-ui"
	@file target/$(TARGET_X64)/release/3r-ui 2>/dev/null || true

# Fast VPS test deployer: build + scp + remote restart
.PHONY: deploy-test
deploy-test:
	./scripts/deploy-test.sh $(VPS) $(ARCH)
