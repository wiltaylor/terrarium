set working-directory := '../..'

import '../shared.just'

dev_listen := env("TERRARIUM_DEV_LISTEN", "127.0.0.1:8090")
dev_state := env("TERRARIUM_DEV_STATE_DIR", "target/dev-state")

[default, private]
main:
	@just --list daemon

# Build every crate in the workspace (debug)
[group('daemon')]
build: require-cargo
	cargo build --workspace

# Run terrariumd on this machine, outside Docker, for fast iteration
[group('daemon')]
run: require-cargo
	TERRARIUM_LISTEN={{dev_listen}} TERRARIUM_STATE_DIR={{dev_state}} TERRARIUM_CONFIG_DIR=. cargo run -p terrariumd

# Run the Rust tests
[group('daemon')]
test: require-cargo
	cargo test --workspace

# Run the operator CLI against a running daemon, for example `just daemon::cli daemon status`
[group('daemon')]
cli *args: require-cargo
	cargo run -q -p terrarium -- {{args}}
