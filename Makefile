AIRUN_BIN := $(PWD)/target/debug/airun

build:
	cargo build
.PHONY: build

test:
	cargo test
.PHONY: test

test-integration: build
	AIRUN_BIN=$(AIRUN_BIN) python3 ./tests/airun_integration_test.py
.PHONY: test-integration

lint:
	cargo check
	cargo clippy
.PHONY: lint

format:
	cargo fmt
.PHONY: format

precommit: lint test build test-integration
.PHONY: precommit
