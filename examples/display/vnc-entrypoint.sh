#!/bin/sh
# Start Xvfb, a window manager, and an xterm, then serve the display with
# x11vnc on port 5900. VNC_PASSWORD (required) is the VNC password; only
# its first 8 characters count.
set -eu
: "${VNC_PASSWORD:?set VNC_PASSWORD}"
display=${DISPLAY:-:0}
Xvfb "$display" -screen 0 "${SCREEN_SIZE:-1280x800x24}" -nolisten tcp >/tmp/xvfb.log 2>&1 &
n=0
while [ ! -S "/tmp/.X11-unix/X${display#:}" ] && [ "$n" -lt 50 ]; do
  sleep 0.1
  n=$((n + 1))
done
openbox >/tmp/openbox.log 2>&1 &
xterm -geometry 100x30+40+40 >/tmp/xterm.log 2>&1 &
# The password goes into a file, not onto the command line.
x11vnc -storepasswd "$VNC_PASSWORD" /tmp/vncpasswd >/dev/null 2>&1
unset VNC_PASSWORD
exec x11vnc -display "$display" -rfbauth /tmp/vncpasswd -rfbport 5900 \
  -forever -shared -noxdamage -add_keysyms -quiet
