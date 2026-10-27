#!/bin/sh
# Start Xvfb, a window manager, and an xterm, then run the container's
# command. Works as root and as the workspace owner (`docker run -u`).
set -eu
display=${DISPLAY:-:0}
Xvfb "$display" -screen 0 "${SCREEN_SIZE:-1280x800x24}" -nolisten tcp >/tmp/xvfb.log 2>&1 &
# Wait for the X server socket (at most 5 s).
n=0
while [ ! -S "/tmp/.X11-unix/X${display#:}" ] && [ "$n" -lt 50 ]; do
  sleep 0.1
  n=$((n + 1))
done
openbox >/tmp/openbox.log 2>&1 &
xterm -geometry 100x30+40+40 >/tmp/xterm.log 2>&1 &
exec "$@"
