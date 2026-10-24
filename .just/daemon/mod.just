set working-directory := '../..'

import '../shared.just'

# Dev defaults for `run`. Each one gives way to the variable when it is set.
# Paths are absolute, so the daemon finds them from any cwd. 8091 keeps the
# artifact origin off 8081, where the Docker stack listens.
dev_listen := env("TERRARIUM_DEV_LISTEN", "127.0.0.1:8090")
dev_state := env("TERRARIUM_DEV_STATE_DIR", justfile_directory() / "target/dev-state")
dev_sandbox := env("TERRARIUM_SANDBOX", "none")
dev_prompts := env("TERRARIUM_PROMPTS_DIR", justfile_directory() / "prompts")
dev_artifact_listen := env("TERRARIUM_ARTIFACT_LISTEN", "127.0.0.1:8091")
dev_home := env("HOME", "")
dev_harness_home := env("TERRARIUM_HARNESS_HOME", "claude=" + dev_home / ".claude" + ",codex=" + dev_home / ".codex" + ",copilot=" + dev_home / ".copilot" + ",pi=" + dev_home / ".pi/agent")

[default, private]
main:
	@just --list daemon

# Build every crate in the workspace (debug)
[group('daemon')]
build: require-cargo
	cargo build --workspace

# Run terrariumd on this machine, outside Docker, with no sandbox and your own harness logins
[group('daemon')]
run: require-cargo runner-build
	TERRARIUM_LISTEN={{dev_listen}} TERRARIUM_STATE_DIR={{quote(dev_state)}} TERRARIUM_CONFIG_DIR={{quote(justfile_directory())}} TERRARIUM_SANDBOX={{dev_sandbox}} TERRARIUM_PROMPTS_DIR={{quote(dev_prompts)}} TERRARIUM_ARTIFACT_LISTEN={{dev_artifact_listen}} TERRARIUM_HARNESS_HOME={{quote(dev_harness_home)}} cargo run -p terrariumd

# Build terrarium-runner next to terrariumd: the daemon runs each agent in one runner process
[private]
runner-build: require-cargo
	cargo build -q -p terrarium-runner

# Run the Rust tests
[group('daemon')]
test: require-cargo
	cargo test --workspace

# Run the operator CLI against a running daemon, for example `just daemon::cli daemon status`
[group('daemon')]
cli *args: require-cargo
	cargo run -q -p terrarium -- {{args}}
