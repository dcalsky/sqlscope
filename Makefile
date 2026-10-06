# Development entry points; CI runs the same recipes.
.PHONY: help fmt fmt-check lint test test-rust test-feature-gates build-ffi build-ffi-release \
        test-c test-go test-python check bump-version

ifeq ($(shell uname -s),Darwin)
FFI_LIB_NAME := libsqlscope_ffi.dylib
else ifeq ($(OS),Windows_NT)
FFI_LIB_NAME := sqlscope_ffi.dll
else
FFI_LIB_NAME := libsqlscope_ffi.so
endif
FFI_DIR ?= target/debug
FFI_LIB := $(abspath $(FFI_DIR)/$(FFI_LIB_NAME))

help:
	@echo "make fmt                  format Rust and Go code"
	@echo "make lint                 rustfmt check, clippy -D warnings, go vet"
	@echo "make test                 Rust, feature-gate, C, Go and Python tests"
	@echo "make build-ffi            debug FFI library (target/debug)"
	@echo "make build-ffi-release    release FFI library (target/ffi), as shipped"
	@echo "make check                lint + test: run before pushing"
	@echo "make bump-version V=x.y.z bump every version reference"
	@echo ""
	@echo "SDK tests use FFI_DIR=$(FFI_DIR); e.g. make test-go FFI_DIR=target/ffi"

fmt:
	cargo fmt --all
	cd go && gofmt -w .

fmt-check:
	cargo fmt --all --check
	@test -z "$$(cd go && gofmt -l .)" || (cd go && gofmt -l . && exit 1)

lint: fmt-check
	cargo clippy --workspace --all-targets -- -D warnings
	cd go && go vet ./...

test: test-rust test-feature-gates test-c test-go test-python

test-rust:
	cargo test --workspace

# Checked one by one so Cargo feature unification cannot hide a missing gate.
test-feature-gates:
	cargo check -p sqlscope --no-default-features
	cargo check -p sqlscope --no-default-features --features stacker

build-ffi:
	cargo build -p sqlscope-ffi

build-ffi-release:
	cargo build -p sqlscope-ffi --profile ffi

test-c: build-ffi
	cc -I crates/sqlscope-ffi/include crates/sqlscope-ffi/tests/c/smoke.c -L $(FFI_DIR) -lsqlscope_ffi -o $(FFI_DIR)/smoke
	LD_LIBRARY_PATH=$(FFI_DIR) DYLD_LIBRARY_PATH=$(FFI_DIR) $(FFI_DIR)/smoke

test-go: build-ffi
	cd go && SQLSCOPE_LIBRARY_PATH=$(FFI_LIB) go test -count=1 ./...

test-python: build-ffi
	cd python && SQLSCOPE_LIBRARY_PATH=$(FFI_LIB) python3 -m pytest -q

check: lint test

bump-version:
ifndef V
	$(error Usage: make bump-version V=x.y.z)
endif
	sed -i.bak '/^\[workspace\.package\]/,/^\[/s/^version = ".*"/version = "$(V)"/' Cargo.toml && rm Cargo.toml.bak
	sed -i.bak 's/^__version__ = ".*"/__version__ = "$(V)"/' python/src/sqlscope/__init__.py && rm python/src/sqlscope/__init__.py.bak
	cargo update --workspace --offline
	@grep -n '^version' Cargo.toml; grep -n '^__version__' python/src/sqlscope/__init__.py
