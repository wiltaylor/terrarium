set working-directory := '../..'

import '../shared.just'

out := "target/host"
build_cmd := "docker build -f docker/Dockerfile --target host-export --output type=local,dest=" + out + " ."
version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1`

[default, private]
main:
	@just --list host

# Build a static musl terrarium-host into target/host/ (built in Docker, so no local musl toolchain is needed)
[group('host')]
build: require-docker
	{{build_cmd}}

# Pack the binary and the systemd unit into a tarball in target/host/
[group('host')]
package: build
	cp crates/terrarium-host/terrarium-host.service {{out}}/
	tar -C {{out}} -czf {{out}}/terrarium-host-{{version}}-{{arch()}}-linux-musl.tar.gz terrarium-host terrarium-host.service
	@echo "wrote {{out}}/terrarium-host-{{version}}-{{arch()}}-linux-musl.tar.gz"

# Install the host agent on user@host over ssh: binary, unit, config, enrol token, start, probe (flags: --dry-run, --yes, --daemon-url, see scripts/host-install.py --help)
[group('host')]
install host name='' *flags: require-ssh require-uv (install-build flags)
	uv run python scripts/host-install.py {{quote(host)}} {{quote(name)}} {{flags}}

# host::build for install, skipped for a dry run so that the plan contacts nothing
[private]
install-build flags:
	@{{ if flags =~ '(^|\s)--dry-run(\s|$)' { "echo '# dry run: skip host::build' >&2" } else { "(command -v docker > /dev/null 2>&1 || (echo 'docker is required' >&2; exit 1)) && echo '" + build_cmd + "' >&2 && " + build_cmd } }}

# Test the two host installers: host-install.py (ssh) and install-host.sh (GET /install/host.sh), with fakes only
[group('host')]
install-test: require-uv
	uv run python -m unittest discover -s scripts -p 'test_*host*.py'

# Register a host agent and print its token alone on stdout; `enrol` gives a one-time enrol token and prints the install command on stderr (operator only)
[group('host')]
token host mode='': require-cargo
	@cargo run -q -p terrarium -- admin host new {{quote(host)}} {{ if mode == "enrol" { "--enrol" } else if mode == "" { "" } else { error("host::token: the second argument is `enrol` or nothing") } }}

# Make a self-signed TLS certificate and key for the host agent listener in config/tls/, and print the SHA-256 to pin
[group('host')]
cert name *names: require-cargo
	@cargo run -q -p terrarium-host -- cert --out config/tls {{quote(name)}} {{names}}
