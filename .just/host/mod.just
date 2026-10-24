set working-directory := '../..'

import '../shared.just'

out := "target/host"
version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1`

[default, private]
main:
	@just --list host

# Build a static musl terrarium-host into target/host/ (built in Docker, so no local musl toolchain is needed)
[group('host')]
build: require-docker
	docker build -f docker/Dockerfile --target host-export --output type=local,dest={{out}} .

# Pack the binary and the systemd unit into a tarball in target/host/
[group('host')]
package: build
	cp crates/terrarium-host/terrarium-host.service {{out}}/
	tar -C {{out}} -czf {{out}}/terrarium-host-{{version}}-{{arch()}}-linux-musl.tar.gz terrarium-host terrarium-host.service
	@echo "wrote {{out}}/terrarium-host-{{version}}-{{arch()}}-linux-musl.tar.gz"

# Copy the binary and unit to a host over ssh and enable the service (not implemented yet: Phase 5)
[group('host')]
install host: require-ssh
	@echo "host::install {{host}} is not implemented yet: the host agent has no networking until Phase 5 (PLAN §19)" >&2
	@exit 1

# Register a host agent with the running daemon and print its token alone on stdout (operator only)
[group('host')]
token host: require-cargo
	@cargo run -q -p terrarium -- admin host new {{quote(host)}}
