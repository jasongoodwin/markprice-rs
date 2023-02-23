test:
	cargo test

fmt:
	cargo fmt

lint:
	cargo clippy

build:
	cargo build --release	
	cp ./Settings.toml target/release

run:
	RUST_LOG=debug cargo run 
