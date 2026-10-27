#!/bin/sh
# Make the login user, then run xrdp-sesman and xrdp. RDP_USER and
# RDP_PASSWORD (required) are the login.
set -eu
: "${RDP_USER:?set RDP_USER}"
: "${RDP_PASSWORD:?set RDP_PASSWORD}"
id "$RDP_USER" >/dev/null 2>&1 || useradd -m -s /bin/sh "$RDP_USER"
printf '%s:%s\n' "$RDP_USER" "$RDP_PASSWORD" | chpasswd
unset RDP_PASSWORD
rm -f /var/run/xrdp/xrdp*.pid /var/run/xrdp-sesman.pid
xrdp-sesman
exec xrdp --nodaemon
