set working-directory := '../..'

import '../shared.just'

# Dev defaults for `run`. Each one gives way to the variable when it is set.
# Paths are absolute, so the daemon finds them from any cwd. 8091 keeps the
# artifact origin off 8081, where the Docker stack listens.
dev_listen := env("TERRARIUM_DEV_LISTEN", "127.0.0.1:8090")
dev_state := env("TERRARIUM_DEV_STATE_DIR", justfile_directory() / "target/dev-state")
dev_sandbox := env("TERRARIUM_SANDBOX", "none")
# The config repo of the dev daemon: kept apart from the stack's config/terrarium.
dev_config_repo := env("TERRARIUM_CONFIG_REPO", justfile_directory() / "target/dev-config-repo")
dev_artifact_listen := env("TERRARIUM_ARTIFACT_LISTEN", "127.0.0.1:8091")
dev_home := env("HOME", "")
dev_harness_home := env("TERRARIUM_HARNESS_HOME", "claude=" + dev_home / ".claude" + ",codex=" + dev_home / ".codex" + ",copilot=" + dev_home / ".copilot" + ",pi=" + dev_home / ".pi/agent")
# The tools dir: the daemon installs the harness pins here at start and puts
# its bin/ first on PATH. Empty: use the harnesses on your PATH as they are.
dev_tools := env("TERRARIUM_TOOLS_DIR", justfile_directory() / "tools")

[default, private]
main:
	@just --list daemon

# Build every crate in the workspace (debug)
[group('daemon')]
build: require-cargo
	cargo build --workspace

# Run terrariumd on this machine, outside Docker, with no sandbox and your own harness logins; it installs the harness pins into ./tools first (flag: --no-harness-install)
[group('daemon')]
run *flags: require-cargo runner-build
	TERRARIUM_LISTEN={{dev_listen}} TERRARIUM_STATE_DIR={{quote(dev_state)}} TERRARIUM_CONFIG_DIR={{quote(justfile_directory())}} TERRARIUM_SANDBOX={{dev_sandbox}} TERRARIUM_CONFIG_REPO={{quote(dev_config_repo)}} TERRARIUM_DEFAULTS_DIR={{quote(justfile_directory() / "defaults")}} TERRARIUM_ARTIFACT_LISTEN={{dev_artifact_listen}} TERRARIUM_HARNESS_HOME={{quote(dev_harness_home)}} TERRARIUM_TOOLS_DIR={{quote(dev_tools)}} cargo run -p terrariumd -- {{flags}}

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
