set working-directory := '../..'

import '../shared.just'

# The config repo and the state dir of the Docker stack (compose.yaml binds them).
config_repo := env("TERRARIUM_DEV_CONFIG_REPO", justfile_directory() / "config/terrarium")
state_dir := env("TERRARIUM_DEV_STACK_STATE_DIR", justfile_directory() / "state")

[default, private]
main:
	@just --list defaults

# Back up config/terrarium to state/backup/config-terrarium-<time>/, then write every packaged default over it (dev: the current prompts and settings are dev data)
[group('defaults')]
apply: require-cargo
	cargo run -q -p terrariumd -- --config-repo {{quote(config_repo)}} --defaults-dir {{quote(justfile_directory() / "defaults")}} --state-dir {{quote(state_dir)}} defaults apply

# Write defaults/manifest.json from the files in defaults/ (run it after each change of a default)
[group('defaults')]
manifest: require-cargo
	cargo run -q -p terrariumd -- defaults manifest {{quote(justfile_directory() / "defaults")}}
