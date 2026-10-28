#!/bin/sh
# Install the Terrarium host agent (terrarium-host) on this machine.
#
# The daemon serves this file at GET /install/host.sh, with its own URLs
# filled in below. Run it on the host as root:
#
#   curl -fsSL http://terrarium.lan:8080/install/host.sh | sudo sh -s -- --token <enrol token>
#
# `terrarium admin host new <name> --enrol`, or "Add host" on the Hosts page
# of the dashboard, prints the full line. The steps are the ones of
# `just host::install` (docs/host-protocol.md, "Install"); the config and the
# unit are the same:
#
#  1. Download terrarium-host for this arch from the daemon, check its
#     SHA-256, and install it as /usr/local/bin/terrarium-host.
#  2. Make /etc/terrarium-host (mode 0700, owned by the service user) and,
#     when it does not exist, config.toml (daemon_url, token_file, name, and
#     the certificate pin of the daemon's host listener).
#  3. Write the one-time enrol token to /etc/terrarium-host/token (mode 0600)
#     as the service user. The host agent swaps it for its long-lived token
#     at the first connect.
#  4. Install, enable, and restart the systemd unit terrarium-host, then run
#     `terrarium-host check --probe`.
#
# --upgrade replaces the binary only. --uninstall removes the unit and the
# binary (--purge also removes /etc/terrarium-host). --dry-run prints the
# plan and changes nothing. A second run is safe: it keeps the config, and
# keeps the token file unless you give --token.
#
# POSIX sh. The enrol token never goes to the output.

set -eu

# The daemon fills these in when it serves the script (crates/terrariumd/src/api/install.rs).
DEFAULT_URL='@TERRARIUM_URL@'
DEFAULT_HOST_URL='@TERRARIUM_HOST_URL@'
DEFAULT_PIN='@TERRARIUM_HOST_PIN@'
SERVED_VERSION='@TERRARIUM_VERSION@'

# The systemd unit: crates/terrarium-host/terrarium-host.service, filled in
# by the daemon. `User=` and `Group=` change to the service user below.
unit_template() {
	cat <<'TERRARIUM_UNIT_END'
@TERRARIUM_HOST_UNIT@
TERRARIUM_UNIT_END
}

# A copy that the daemon did not fill in has the bare placeholders.
case $DEFAULT_URL in @*@) DEFAULT_URL= ;; esac
case $DEFAULT_HOST_URL in @*@) DEFAULT_HOST_URL= ;; esac
case $DEFAULT_PIN in @*@) DEFAULT_PIN= ;; esac
case $SERVED_VERSION in @*@) SERVED_VERSION=unknown ;; esac

# TERRARIUM_INSTALL_ROOT puts every system path under a directory. Tests only.
ROOT=${TERRARIUM_INSTALL_ROOT:-}
CONFIG_DIR=/etc/terrarium-host
CONFIG_PATH=$CONFIG_DIR/config.toml
TOKEN_PATH=$CONFIG_DIR/token
BIN_PATH=/usr/local/bin/terrarium-host
UNIT_PATH=/etc/systemd/system/terrarium-host.service
UNIT_NAME=terrarium-host
HOST_PORT=8082

say() { printf '%s\n' "$*"; }
note() { printf '# %s\n' "$*"; }
warn() { printf 'install-host: warning: %s\n' "$*" >&2; }
die() {
	printf 'install-host: %s\n' "$*" >&2
	exit 2
}

usage() {
	cat <<'EOF'
Install the Terrarium host agent on this machine.

usage: install-host.sh --token <enrol token> [options]
       install-host.sh --upgrade [options]
       install-host.sh --uninstall [--purge] [options]

  --token TOKEN      the one-time enrol token (`terrarium admin host new <name> --enrol`);
                     or set TERRARIUM_ENROL_TOKEN. Needed on a new host.
  --name NAME        the host agent name (default: the short host name)
  --url URL          the daemon's dashboard URL, for the downloads
  --host-url URL     the daemon's host listener, written as daemon_url
                     (default: from the daemon; a bare name becomes wss://NAME:8082)
  --fingerprint FP   the SHA-256 of the host listener certificate to pin
  --no-pin           write no pin: the system roots check the daemon
  --user USER        the service user (default: the user that ran sudo, else terrarium)
  --enable-display   a new config gets [display] enabled = true: desktop targets may
                     see this machine's screen and use its mouse and keyboard (default: off)
  --dry-run          print the plan; change nothing
  --upgrade          replace the binary; keep the config and the token; restart
  --uninstall        stop and remove the unit and the binary; keep the config
  --purge            with --uninstall: remove /etc/terrarium-host too
  -h, --help         show this help
EOF
}

# ---------------------------------------------------------------------------
# Arguments

MODE=install
DRY=0
PURGE=0
NO_PIN=0
TOKEN=${TERRARIUM_ENROL_TOKEN:-}
NAME=
URL=
HOST_URL=
PIN=
SERVICE_USER=
ENABLE_DISPLAY=0
DISPLAY_RISK="an agent with an exclusive lease on a desktop target can see this machine's screen and use its mouse and keyboard as the service user"

need_value() {
	if [ "$#" -lt 2 ] || [ -z "$2" ]; then
		die "$1 needs a value"
	fi
}

while [ "$#" -gt 0 ]; do
	case $1 in
	--token) need_value "$@"; TOKEN=$2; shift 2 ;;
	--token=*) TOKEN=${1#*=}; shift ;;
	--name) need_value "$@"; NAME=$2; shift 2 ;;
	--name=*) NAME=${1#*=}; shift ;;
	--url) need_value "$@"; URL=$2; shift 2 ;;
	--url=*) URL=${1#*=}; shift ;;
	--host-url) need_value "$@"; HOST_URL=$2; shift 2 ;;
	--host-url=*) HOST_URL=${1#*=}; shift ;;
	--fingerprint) need_value "$@"; PIN=$2; shift 2 ;;
	--fingerprint=*) PIN=${1#*=}; shift ;;
	--no-pin) NO_PIN=1; shift ;;
	--user) need_value "$@"; SERVICE_USER=$2; shift 2 ;;
	--user=*) SERVICE_USER=${1#*=}; shift ;;
	--enable-display) ENABLE_DISPLAY=1; shift ;;
	--dry-run) DRY=1; shift ;;
	--upgrade) MODE=upgrade; shift ;;
	--uninstall) MODE=uninstall; shift ;;
	--purge) PURGE=1; shift ;;
	-h | --help) usage; exit 0 ;;
	*) die "unknown argument \`$1\` (see --help)" ;;
	esac
done

[ "$PURGE" = 0 ] || [ "$MODE" = uninstall ] || die "--purge goes with --uninstall"

# ---------------------------------------------------------------------------
# Checks of values. Every value that goes into a file or a command is
# checked here, so no quoting is needed later.

matches() { printf '%s' "$1" | grep -Eq "$2"; }

check_name() {
	matches "$1" '^[a-z0-9][a-z0-9_-]{0,62}$' ||
		die "bad host name \`$1\`: use 1-63 of a-z, 0-9, \`-\`, \`_\`, starting with a letter or digit"
}

check_url() {
	matches "$2" '^[a-z][a-z0-9+.-]*://[][A-Za-z0-9._~%:/-]+$' ||
		die "bad $1 \`$2\`"
}

# The short host name in lower case, with other characters as `-`.
default_name() {
	n=$(hostname 2>/dev/null || uname -n)
	n=${n%%.*}
	printf '%s' "$n" | tr '[:upper:]' '[:lower:]' | tr -c 'a-z0-9_-' '-'
}

# `daemon_url` as host-install.py makes it: a bare name becomes wss://NAME,
# and a URL with no port gets 8082.
normalize_host_url() {
	u=$1
	case $u in *://*) ;; *) u=wss://$u ;; esac
	scheme=${u%%://*}
	rest=${u#*://}
	authority=${rest%%/*}
	path=${rest#"$authority"}
	case $scheme in ws | wss | http | https) ;; *) die "bad daemon URL \`$1\`: use wss://host:port" ;; esac
	case $authority in
	\[*\]:*) ;;
	\[*\]) authority=$authority:$HOST_PORT ;;
	*:*) ;;
	*) authority=$authority:$HOST_PORT ;;
	esac
	printf '%s://%s%s' "$scheme" "$authority" "$path"
}

# ---------------------------------------------------------------------------
# Running. In a dry run, each change prints as `+ command` and runs not.

run() {
	if [ "$DRY" = 1 ]; then
		say "+ $*"
	else
		"$@"
	fi
}

# write_file PATH MODE OWNER GROUP < content. Through PATH.new and a rename.
write_file() {
	if [ "$DRY" = 1 ]; then
		note "write $1 (mode $2, $3:$4):"
		sed 's/^/#   /'
		return
	fi
	cat >"$ROOT$1.new"
	chmod "$2" "$ROOT$1.new"
	chown "$3:$4" "$ROOT$1.new"
	mv -f "$ROOT$1.new" "$ROOT$1"
}

# Run a command as the service user.
as_user() {
	if [ "$SERVICE_USER" = root ]; then
		"$@"
	elif command -v runuser >/dev/null 2>&1; then
		runuser -u "$SERVICE_USER" -- "$@"
	elif command -v sudo >/dev/null 2>&1; then
		sudo -u "$SERVICE_USER" -- "$@"
	else
		die "no runuser and no sudo: cannot run a command as $SERVICE_USER"
	fi
}

have() { command -v "$1" >/dev/null 2>&1; }

# fetch URL FILE. A refusal from the daemon (404 for an unknown arch)
# prints its message.
fetch() {
	if have curl; then
		code=$(curl -sS -L -o "$2" -w '%{http_code}' "$1") || die "cannot download $1"
		if [ "$code" != 200 ]; then
			msg=$(head -c 400 "$2" 2>/dev/null || true)
			die "the daemon answered $code for $1: $msg"
		fi
	elif have wget; then
		wget -q -O "$2" "$1" || die "cannot download $1 (wget)"
	else
		die "curl or wget is needed to download terrarium-host"
	fi
}

sha256_of() {
	if have sha256sum; then
		sha256sum "$1" | cut -d' ' -f1
	elif have shasum; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "sha256sum or shasum is needed to check the download"
	fi
}

# ---------------------------------------------------------------------------
# What the host has

is_root() { [ "$(id -u)" = 0 ]; }
user_exists() { id -u "$1" >/dev/null 2>&1; }
group_exists() { getent group "$1" >/dev/null 2>&1; }
in_group() { id -nG "$1" 2>/dev/null | tr ' ' '\n' | grep -qx "$2"; }
user_home() { getent passwd "$1" 2>/dev/null | cut -d: -f6; }
has_systemd() { [ -d "$ROOT/run/systemd/system" ] && have systemctl; }

# Is the program on the PATH of the unit (/usr/local/bin:/usr/bin:/bin:~/.local/bin:~/.cargo/bin)?
on_service_path() {
	for d in "$ROOT/usr/local/bin" "$ROOT/usr/bin" "$ROOT/bin" "$SERVICE_HOME/.local/bin" "$SERVICE_HOME/.cargo/bin"; do
		[ -n "$d" ] && [ -x "$d/$1" ] && return 0
	done
	return 1
}

arch() {
	m=$(uname -m)
	case $m in
	x86_64 | amd64) echo x86_64 ;;
	aarch64 | arm64) echo aarch64 ;;
	*) echo "$m" ;;
	esac
}

# The name in an existing config, for --uninstall.
config_name() {
	sed -n 's/^name = "\(.*\)"$/\1/p' "$ROOT$CONFIG_PATH" 2>/dev/null | head -n 1
}

# ---------------------------------------------------------------------------
# Generated files. render_config is host-install.py's render_config.

render_config() {
	say '# Written by the Terrarium host installer (docs/host-protocol.md).'
	say "daemon_url = \"$HOST_URL\""
	say "token_file = \"$TOKEN_PATH\""
	say "name = \"$NAME\""
	if [ -n "$PIN" ]; then
		say "server_fingerprint_sha256 = \"$PIN\""
	fi
	if [ "$ENABLE_DISPLAY" = 1 ]; then
		say ''
		say '[display]'
		say 'enabled = true'
	fi
}

render_unit() {
	# `%h` in a system unit is root's home, not the service user's.
	unit_template | sed -e "s/^User=.*/User=$SERVICE_USER/" -e "s/^Group=.*/Group=$SERVICE_GROUP/" \
		-e "/^Environment=PATH=/s|%h|$SERVICE_HOME|g"
}

# ---------------------------------------------------------------------------
# The steps

install_binary() {
	a=$(arch)
	base=${URL%/}/install/terrarium-host
	if [ "$DRY" = 1 ]; then
		say "+ curl -fsSL $base?arch=$a -o terrarium-host"
		say "+ curl -fsSL $base.sha256?arch=$a -o terrarium-host.sha256"
		note "check the SHA-256 of the download"
		say "+ install -m 0755 -o root -g root terrarium-host $BIN_PATH"
		return
	fi
	tmp=$(mktemp -d)
	# shellcheck disable=SC2064 # expand $tmp now
	trap "rm -rf '$tmp'" EXIT
	note "download terrarium-host ($a) from $URL"
	fetch "$base?arch=$a" "$tmp/terrarium-host"
	fetch "$base.sha256?arch=$a" "$tmp/terrarium-host.sha256"
	want=$(cut -d' ' -f1 <"$tmp/terrarium-host.sha256" | tr '[:upper:]' '[:lower:]')
	got=$(sha256_of "$tmp/terrarium-host")
	matches "$want" '^[0-9a-f]{64}$' || die "the daemon sent no SHA-256 for terrarium-host"
	[ "$want" = "$got" ] || die "the SHA-256 of the download is $got, the daemon says $want: not installed"
	note "SHA-256 $got: ok"
	mkdir -p "$ROOT${BIN_PATH%/*}"
	cp "$tmp/terrarium-host" "$ROOT$BIN_PATH.new"
	chmod 0755 "$ROOT$BIN_PATH.new"
	chown root:root "$ROOT$BIN_PATH.new"
	mv -f "$ROOT$BIN_PATH.new" "$ROOT$BIN_PATH"
}

ensure_user() {
	if user_exists "$SERVICE_USER"; then
		return
	fi
	note "make the system user $SERVICE_USER"
	run useradd --system --user-group --home-dir /var/lib/$UNIT_NAME --create-home --shell /usr/sbin/nologin "$SERVICE_USER"
}

# Docker: the socket belongs to the group `docker`. A member of it can
# start a root container, so the group is as strong as root on this host.
# The host agent needs it to run docker targets.
ensure_groups() {
	[ "$SERVICE_USER" = root ] && return
	if [ "$DOCKER" = yes ]; then
		if ! group_exists docker; then
			warn "docker is installed, but there is no group \`docker\`: give $SERVICE_USER access to the Docker socket by hand"
		elif in_group "$SERVICE_USER" docker; then
			note "$SERVICE_USER is in the group docker"
		else
			note "add $SERVICE_USER to the group docker, for docker targets (this group is as strong as root on this host)"
			run usermod -aG docker "$SERVICE_USER"
		fi
	fi
	if [ "$VMLAB" = yes ] && group_exists kvm && ! in_group "$SERVICE_USER" kvm; then
		note "add $SERVICE_USER to the group kvm, for vmlab"
		run usermod -aG kvm "$SERVICE_USER"
	fi
}

write_token() {
	note "write the enrol token to $TOKEN_PATH (mode 0600, as $SERVICE_USER)"
	if [ "$DRY" = 1 ]; then
		say "+ $BIN_PATH token-file $TOKEN_PATH  < <enrol token>"
		return
	fi
	printf '%s\n' "$TOKEN" | as_user "$ROOT$BIN_PATH" token-file "$ROOT$TOKEN_PATH" >/dev/null
}

start_service() {
	if [ "$SYSTEMD" = no ]; then
		note "no systemd: start the host agent yourself, as $SERVICE_USER, for example:"
		say "  runuser -u $SERVICE_USER -- $BIN_PATH --config $CONFIG_PATH run"
		return
	fi
	# `restart`, not `enable --now`: a running agent must pick up the new
	# binary and the new token.
	run systemctl daemon-reload
	run systemctl enable $UNIT_NAME
	run systemctl restart $UNIT_NAME
}

probe() {
	note "probe the link: terrarium-host check --probe"
	if [ "$DRY" = 1 ]; then
		say "+ $BIN_PATH --config $CONFIG_PATH check --probe  (as $SERVICE_USER)"
		return 0
	fi
	if as_user "$ROOT$BIN_PATH" --config "$ROOT$CONFIG_PATH" check --probe; then
		return 0
	fi
	if [ "$SYSTEMD" = yes ]; then
		warn "check --probe failed: see \`journalctl -u $UNIT_NAME\`"
	else
		warn "check --probe failed"
	fi
	return 1
}

summary() {
	say ""
	say "terrarium-host on this machine:"
	say "  name     $NAME"
	say "  daemon   $HOST_URL"
	say "  user     $SERVICE_USER"
	say "  docker   $DOCKER"
	say "  vmlab    $VMLAB"
	if [ "$SYSTEMD" = yes ]; then
		say "  service  $UNIT_NAME (systemctl status $UNIT_NAME)"
	else
		say "  service  none: no systemd"
	fi
	say "The host shows online in \`terrarium admin host ls\` and on the Hosts page when it connects."
}

# ---------------------------------------------------------------------------
# Main

[ "$(uname -s)" = Linux ] || die "terrarium-host runs on Linux only"
if [ "$DRY" = 0 ] && ! is_root; then
	die "run as root, for example: curl -fsSL <daemon>/install/host.sh | sudo sh -s -- --token <token>"
fi

# The service user. vmlab runs as the user who owns the labs, and the
# host agent runs its commands as the service user. So the default is the
# user who ran sudo, not root and not a new system user.
if [ -z "$SERVICE_USER" ]; then
	invoker=${SUDO_USER:-${DOAS_USER:-}}
	if [ -n "$invoker" ] && [ "$invoker" != root ]; then
		SERVICE_USER=$invoker
	else
		SERVICE_USER=terrarium
	fi
fi
matches "$SERVICE_USER" '^[a-z_][a-z0-9_-]{0,31}$' || die "bad user name \`$SERVICE_USER\`"

SYSTEMD=no
has_systemd && SYSTEMD=yes

if [ "$MODE" = uninstall ]; then
	note "uninstall the host agent"
	if [ "$SYSTEMD" = yes ] && [ -e "$ROOT$UNIT_PATH" ]; then
		run systemctl stop $UNIT_NAME || true
		run systemctl disable $UNIT_NAME || true
		run rm -f "$ROOT$UNIT_PATH"
		run systemctl daemon-reload
	fi
	run rm -f "$ROOT$BIN_PATH"
	old=$(config_name)
	if [ "$PURGE" = 1 ]; then
		run rm -rf "$ROOT$CONFIG_DIR"
	elif [ -e "$ROOT$CONFIG_DIR" ]; then
		note "keep $CONFIG_DIR (add --purge to remove it)"
	fi
	note "done. On the daemon, revoke the token: terrarium admin host rm ${old:-<name>}"
	exit 0
fi

URL=${URL:-$DEFAULT_URL}
[ -n "$URL" ] || die "no daemon URL: pass --url http://<daemon>:8080"
check_url "daemon URL" "$URL"

if [ "$MODE" = upgrade ]; then
	[ -e "$ROOT$BIN_PATH" ] || [ -e "$ROOT$CONFIG_PATH" ] ||
		die "terrarium-host is not installed here: run without --upgrade"
	note "upgrade terrarium-host (served version $SERVED_VERSION); keep the config and the token"
	install_binary
	if [ "$SYSTEMD" = yes ] && [ -e "$ROOT$UNIT_PATH" ]; then
		run systemctl restart $UNIT_NAME
	else
		note "restart the host agent to use the new binary"
	fi
	note "done"
	exit 0
fi

# A new install or a second run.
HAVE_CONFIG=no
[ -e "$ROOT$CONFIG_PATH" ] && HAVE_CONFIG=yes
HAVE_TOKEN=no
[ -e "$ROOT$TOKEN_PATH" ] && HAVE_TOKEN=yes

if [ -n "$TOKEN" ]; then
	matches "$TOKEN" '^[A-Za-z0-9_]{8,200}$' || die "the token has characters that a host token does not have"
elif [ "$HAVE_TOKEN" = no ]; then
	die "no token: pass --token <enrol token> (terrarium admin host new <name> --enrol)"
fi

if [ "$HAVE_CONFIG" = yes ]; then
	old=$(config_name)
	if [ -n "$NAME" ] && [ -n "$old" ] && [ "$NAME" != "$old" ]; then
		warn "the existing $CONFIG_PATH names the host \`$old\`: it stays (edit it, or --uninstall --purge first)"
	fi
	NAME=${old:-$NAME}
fi
NAME=${NAME:-$(default_name)}
check_name "$NAME"

HOST_URL=${HOST_URL:-$DEFAULT_HOST_URL}
if [ "$HAVE_CONFIG" = no ]; then
	[ -n "$HOST_URL" ] || die "no host listener URL: the daemon has no TLS host listener \
(set TERRARIUM_HOST_TLS_CERT and TERRARIUM_HOST_TLS_KEY, or TERRARIUM_HOST_DAEMON_URL, on the daemon), \
or pass --host-url wss://<daemon>:8082"
	HOST_URL=$(normalize_host_url "$HOST_URL")
	check_url "host listener URL" "$HOST_URL"
	case $HOST_URL in
	wss://* | https://*)
		if [ "$NO_PIN" = 1 ]; then
			PIN=
		else
			PIN=${PIN:-$DEFAULT_PIN}
		fi
		;;
	*) PIN= ;; # ws:// never gets a pin: the host agent refuses that.
	esac
	if [ -n "$PIN" ]; then
		matches "$PIN" '^[A-Za-z0-9: ]{32,200}$' || die "bad fingerprint \`$PIN\`"
	fi
else
	HOST_URL=$(sed -n 's/^daemon_url = "\(.*\)"$/\1/p' "$ROOT$CONFIG_PATH" | head -n 1)
fi

if user_exists "$SERVICE_USER"; then
	SERVICE_GROUP=$(id -gn "$SERVICE_USER")
	SERVICE_HOME=$(user_home "$SERVICE_USER")
else
	SERVICE_GROUP=$SERVICE_USER
	SERVICE_HOME=/var/lib/$UNIT_NAME
fi

DOCKER=no
on_service_path docker && DOCKER=yes
VMLAB=no
on_service_path vmlab && VMLAB=yes

note "install the host agent \`$NAME\` (terrarium-host $SERVED_VERSION)"
note "daemon     = $URL"
note "daemon_url = $HOST_URL"
if [ -n "$PIN" ]; then
	note "pin        = $PIN"
elif [ "$HAVE_CONFIG" = no ]; then
	case $HOST_URL in wss://* | https://*) note "pin        = none (the host checks the daemon against the system roots)" ;; esac
fi
note "user       = $SERVICE_USER ($SERVICE_GROUP)"
note "docker     = $DOCKER, vmlab = $VMLAB, systemd = $SYSTEMD"
[ "$SERVICE_USER" = root ] && warn "the host agent runs its commands as root; use --user <name> unless the targets need root"
if [ "$VMLAB" = no ]; then
	note "no vmlab on the PATH of $SERVICE_USER (/usr/local/bin, /usr/bin, /bin, ~/.local/bin): vmlab targets stay off"
fi
if [ "$ENABLE_DISPLAY" = 1 ]; then
	warn "--enable-display: $DISPLAY_RISK"
	if ! user_exists "$SERVICE_USER" || [ "$SERVICE_HOME" = /var/lib/$UNIT_NAME ]; then
		note "$SERVICE_USER is a system user with no desktop session: a desktop target needs it to own (or be allowed into) the X or Wayland session; set xauthority or runtime_dir in [display]"
	fi
fi
[ "$DRY" = 1 ] && note "dry run: nothing changes"

ensure_user
ensure_groups
install_binary

# The agent writes the token through a temporary file and a rename, so it
# must own the directory.
run mkdir -p "$ROOT$CONFIG_DIR"
run chmod 0700 "$ROOT$CONFIG_DIR"
run chown "$SERVICE_USER:$SERVICE_GROUP" "$ROOT$CONFIG_DIR"
if [ "$HAVE_CONFIG" = yes ]; then
	note "keep the existing $CONFIG_PATH"
	if [ "$ENABLE_DISPLAY" = 1 ]; then
		note "--enable-display: the existing $CONFIG_PATH does not change; add \`[display]\` with \`enabled = true\` to it by hand (the daemon cannot turn it on)"
	fi
else
	render_config | write_file "$CONFIG_PATH" 0640 root "$SERVICE_GROUP"
fi

if [ -n "$TOKEN" ]; then
	[ "$HAVE_TOKEN" = yes ] && note "replace the existing $TOKEN_PATH"
	write_token
else
	note "keep the existing $TOKEN_PATH"
fi

if [ "$SYSTEMD" = yes ]; then
	run mkdir -p "$ROOT${UNIT_PATH%/*}"
	render_unit | write_file "$UNIT_PATH" 0644 root root
fi
start_service

status=0
probe || status=1
summary
exit "$status"
