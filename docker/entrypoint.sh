#!/usr/bin/env bash
# Terrarium container entrypoint (PLAN §23.3).
#
# 1. Make missing directories under /auth, /state, and /run/terrarium.
# 2. Fix ownership of empty bind mounts (only when started as root).
# 3. Install the Copilot system-prompt extension when it is missing, or when
#    the image version changed.
# 4. Exec the command (terrariumd by default) as the terrarium user.
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

exec "$@"
