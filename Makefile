build:
	cargo build
.PHONY: build

test:
	cargo test
.PHONY: test

lint:
	cargo check
	cargo clippy
.PHONY: lint

format:
	cargo fmt
.PHONY: format
