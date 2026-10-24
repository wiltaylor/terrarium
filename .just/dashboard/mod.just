set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list dashboard

# Install the dashboard's npm dependencies from the lockfile
[group('dashboard')]
install: require-node require-dashboard
	npm --prefix dashboard ci

# Start the Vite dev server, which proxies the API to the daemon
[group('dashboard')]
dev: require-node require-dashboard
	npm --prefix dashboard run dev

# Build the dashboard into dashboard/dist for the daemon to embed
[group('dashboard')]
build: require-node require-dashboard
	npm --prefix dashboard run build

# Run the dashboard unit tests
[group('dashboard')]
test: require-node require-dashboard
	npm --prefix dashboard run test

# Typecheck and lint the dashboard
[group('dashboard')]
check: require-node require-dashboard
	npm --prefix dashboard run check
