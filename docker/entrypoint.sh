#!/usr/bin/env bash
# Terrarium container entrypoint (PLAN §23.3).
#
# 1. Make missing directories under /auth, /state, and /run/terrarium.
# 2. Fix ownership of empty bind mounts (only when started as root).
# 3. Export TERRARIUM_RUNNER (the terrarium-runner binary) when it is not set.
# 4. Install the Copilot system-prompt extension when it is missing, or when
#    the image version changed.
# 5. Run the command (terrariumd by default) as the terrarium user. For
#    terrariumd, this script stays as its parent and restarts it when it
#    crashes, so the agent runners live on (see docker/README.md).
set -euo pipefail

TERRARIUM_USER="${TERRARIUM_USER:-terrarium}"
AUTH_DIRS=(/auth/claude /auth/codex /auth/copilot /auth/pi)
STATE_DIRS=(/state /state/agents /state/artifacts /state/credentials /run/terrarium)
IMAGE_VERSION_FILE=/usr/local/share/terrarium/image-version
EXTENSION_MARKER=/auth/copilot/.terrarium-extension-version

log() { echo "entrypoint: $*" >&2; }

is_empty_dir() { [ -d "$1" ] && [ -z "$(ls -A "$1" 2>/dev/null)" ]; }

if [ "$(id -u)" = "0" ]; then
	for dir in "${AUTH_DIRS[@]}" "${STATE_DIRS[@]}"; do
		if [ ! -d "$dir" ]; then
			mkdir -p "$dir"
			chown "$TERRARIUM_USER:$TERRARIUM_USER" "$dir"
		elif is_empty_dir "$dir"; then
			chown "$TERRARIUM_USER:$TERRARIUM_USER" "$dir"
		fi
	done
	chmod 700 /state/credentials
	# Run the rest of this script, and then the command, as the terrarium user.
	exec setpriv --reuid="$TERRARIUM_USER" --regid="$TERRARIUM_USER" --init-groups \
		env HOME="/home/$TERRARIUM_USER" "$0" "$@"
fi

for dir in "${AUTH_DIRS[@]}" "${STATE_DIRS[@]}"; do
	mkdir -p "$dir" 2>/dev/null || true
	if [ ! -w "$dir" ]; then
		log "$dir is not writable by uid $(id -u). Run 'just docker::volumes-init', or set TERRARIUM_UID and TERRARIUM_GID to the owner of the bind mounts."
		exit 1
	fi
done
chmod 700 /state/credentials 2>/dev/null || true

# The runner processes (PLAN §6). The image sets TERRARIUM_RUNNER; keep a
# value from compose, else find the binary on PATH.
if [ "${TERRARIUM_RUNNER_MODE:-process}" = "process" ] && [ -z "${TERRARIUM_RUNNER:-}" ]; then
	if runner="$(command -v terrarium-runner)"; then
		export TERRARIUM_RUNNER="$runner"
	else
		log "terrarium-runner is not on PATH; terrariumd will refuse to start in runner mode 'process'"
	fi
fi

image_version="$(cat "$IMAGE_VERSION_FILE" 2>/dev/null || echo unknown)"
installed_version="$(cat "$EXTENSION_MARKER" 2>/dev/null || true)"
if command -v copilot-custom-system-prompt > /dev/null 2>&1; then
	if [ -z "$installed_version" ]; then
		log "installing the Copilot system-prompt extension"
		if copilot-custom-system-prompt install; then
			echo "$image_version" > "$EXTENSION_MARKER"
		else
			log "extension install failed; Copilot agents will not get the Terrarium prompt"
		fi
	elif [ "$installed_version" != "$image_version" ]; then
		log "image changed ($installed_version -> $image_version): reinstalling the Copilot extension"
		if copilot-custom-system-prompt install --force; then
			echo "$image_version" > "$EXTENSION_MARKER"
		else
			log "extension reinstall failed; the old extension stays"
		fi
	fi
else
	log "copilot-custom-system-prompt is not installed; skipping the extension"
fi

# ---------------------------------------------------------------------------
# The daemon supervisor (docker/README.md).
#
# tini (PID 1) -> this script -> terrariumd. Each terrarium-runner calls
# setsid. When terrariumd dies, the runners go to tini, and tini reaps them.
# This script starts terrariumd again in the same container, and
# Supervisor::recover finds the live runners through <run>/runners/.

RUN_DIR="${TERRARIUM_RUNTIME_DIR:-${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/terrarium}}"
RUN_DIR="${RUN_DIR:-/run/terrarium}"
STATE_DIR="${TERRARIUM_STATE_DIR:-/state}"
MAX_BACKOFF="${TERRARIUM_SUPERVISOR_MAX_BACKOFF:-30}"
RESET_AFTER="${TERRARIUM_SUPERVISOR_RESET_AFTER:-300}"
DAEMON_STOP_TIMEOUT="${TERRARIUM_DAEMON_STOP_TIMEOUT:-5}"
SHUTDOWN_GRACE="${TERRARIUM_SHUTDOWN_GRACE:-20}"

# Supervise only `terrariumd` with no subcommand. Other commands (a shell,
# `terrariumd sandbox-dry-run`) run once, as before.
supervise=0
if [ "$(basename -- "${1:-}")" = terrariumd ] && [ "${TERRARIUM_SUPERVISOR:-1}" != 0 ]; then
	case "${2:-}" in "" | -*) supervise=1 ;; esac
fi
if [ "$supervise" = 0 ]; then
	exec "$@"
fi

set +e
stop_signal=""
daemon_pid=""
sleep_pid=""

on_stop() {
	stop_signal="$1"
	log "SIG$1: stopping terrariumd"
	if [ -n "$daemon_pid" ]; then kill -s "$1" "$daemon_pid" 2>/dev/null; fi
	if [ -n "$sleep_pid" ]; then kill "$sleep_pid" 2>/dev/null; fi
}
trap 'on_stop TERM' TERM
trap 'on_stop INT' INT

now() { date +%s; }

# True when $1 is a live process (not a zombie).
alive() {
	local stat state
	stat="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
	# The state comes after the command name, which ends with ") ".
	state="${stat##*) }"
	state="${state%% *}"
	[ -n "$state" ] && [ "$state" != Z ]
}

# True when $1 is a live terrarium-runner process.
is_runner() {
	local argv0
	alive "$1" || return 1
	argv0="$(tr '\0' '\n' < "/proc/$1/cmdline" 2>/dev/null | head -n 1)"
	[ "$(basename -- "$argv0")" = terrarium-runner ]
}

# Wait for the daemon to exit, and set daemon_status. A trapped signal
# interrupts `wait`; then wait again, unless $1 is "stop" and a stop signal
# came (wait_daemon_stop then waits with a timeout).
wait_daemon() {
	while :; do
		wait "$daemon_pid"
		daemon_status=$?
		if ! kill -0 "$daemon_pid" 2>/dev/null; then return; fi
		if [ "${1:-}" = stop ] && [ -n "$stop_signal" ]; then return; fi
	done
}

# Wait for the daemon after a stop signal, at most DAEMON_STOP_TIMEOUT s.
wait_daemon_stop() {
	local deadline=$(($(now) + DAEMON_STOP_TIMEOUT))
	while alive "$daemon_pid" && [ "$(now)" -lt "$deadline" ]; do
		sleep 0.2
	done
	if alive "$daemon_pid"; then
		log "terrariumd did not stop in ${DAEMON_STOP_TIMEOUT}s: sending SIGKILL"
		kill -KILL "$daemon_pid" 2>/dev/null
	fi
	wait_daemon
}

# Send SIGTERM to each live runner, and wait up to SHUTDOWN_GRACE s. A
# runner types the exit keys of an idle harness, and stops a busy one.
stop_runners() {
	local f pid agent deadline left
	local -a pids=() agents=()
	for f in "$RUN_DIR"/runners/*.pid; do
		[ -e "$f" ] || continue
		pid="$(tr -d '[:space:]' < "$f" 2>/dev/null)"
		case "$pid" in "" | *[!0-9]*) continue ;; esac
		is_runner "$pid" || continue
		agent="$(basename -- "$f" .pid)"
		kill -TERM "$pid" 2>/dev/null || continue
		pids+=("$pid")
		agents+=("$agent")
	done
	if [ "${#pids[@]}" = 0 ]; then
		log "no live agent runners"
		return
	fi
	log "sent SIGTERM to ${#pids[@]} agent runner(s); waiting up to ${SHUTDOWN_GRACE}s"
	deadline=$(($(now) + SHUTDOWN_GRACE))
	while :; do
		left=0
		for pid in "${pids[@]}"; do
			if is_runner "$pid"; then left=$((left + 1)); fi
		done
		[ "$left" = 0 ] && break
		[ "$(now)" -ge "$deadline" ] && break
		sleep 0.5
	done
	local i
	for i in "${!pids[@]}"; do
		pid="${pids[$i]}"
		agent="${agents[$i]}"
		if is_runner "$pid"; then
			log "runner $pid (agent $agent) did not stop in ${SHUTDOWN_GRACE}s: sending SIGKILL"
			kill -KILL "$pid" 2>/dev/null
		else
			# The container stopped this runner, not the harness or the
			# operator. Remove the exit status it wrote, so the next
			# container start launches the agent again with resume (PLAN §18).
			rm -f "$STATE_DIR/agents/$agent/runner-exit.json"
			log "runner $pid (agent $agent) stopped"
		fi
	done
}

backoff=1
while :; do
	started="$(now)"
	# A background job of a shell without job control ignores SIGINT and
	# SIGQUIT; `env --default-signal` gives terrariumd and the runners the
	# default dispositions back.
	env --default-signal "$@" &
	daemon_pid=$!
	if [ -n "$stop_signal" ]; then kill -s "$stop_signal" "$daemon_pid" 2>/dev/null; fi
	log "started terrariumd (pid $daemon_pid)"
	wait_daemon stop
	if [ -n "$stop_signal" ]; then
		wait_daemon_stop
		log "terrariumd stopped (status $daemon_status)"
		break
	fi
	uptime=$(($(now) - started))
	if [ "$daemon_status" = 0 ]; then
		log "terrariumd exited with status 0 after ${uptime}s: stopping the container"
		break
	fi
	how="status $daemon_status"
	if [ "$daemon_status" -gt 128 ]; then
		how="signal $(kill -l "$((daemon_status - 128))" 2>/dev/null || echo "$((daemon_status - 128))")"
	fi
	if [ "$uptime" -ge "$RESET_AFTER" ]; then backoff=1; fi
	log "terrariumd ended ($how) after ${uptime}s; starting it again in ${backoff}s. The agent runners keep running."
	daemon_pid=""
	sleep "$backoff" &
	sleep_pid=$!
	wait "$sleep_pid"
	sleep_pid=""
	if [ -n "$stop_signal" ]; then break; fi
	backoff=$((backoff * 2))
	if [ "$backoff" -gt "$MAX_BACKOFF" ]; then backoff="$MAX_BACKOFF"; fi
done

stop_runners
log "exit"
exit 0
