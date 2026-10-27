#!/bin/sh
# Start a headless sway with a foot terminal, then run the container's
# command. sway makes its socket in $XDG_RUNTIME_DIR.
set -eu
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"
printf 'output HEADLESS-1 resolution 1280x800\nexec foot\n' > /tmp/sway.conf
sway --config /tmp/sway.conf >/tmp/sway.log 2>&1 &
# ydotoold needs /dev/uinput; without it, pointer input is "failed".
if [ -w /dev/uinput ]; then
  ydotoold >/tmp/ydotoold.log 2>&1 &
fi
exec "$@"
