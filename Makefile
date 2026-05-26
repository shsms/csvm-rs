install:
	cargo build --release
	install -m 555 target/release/csvm $(shell systemd-path user-binaries)/
