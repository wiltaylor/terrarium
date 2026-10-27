# A test RDP server for an `rdp` target (PLAN §19b, docs/api.md §17.10):
# xrdp with the xorgxrdp X server, the openbox window manager, and an
# xterm.
#
#   docker build -t terrarium-rdp-desktop -f examples/display/rdp.Dockerfile examples/display
#   docker run -d --name rdp-test -e RDP_USER=lab -e RDP_PASSWORD=<password> \
#     -p 127.0.0.1:3389:3389 terrarium-rdp-desktop
#
# Target:
#
#   terrarium admin target new rdp1 --kind rdp --host lab1 \
#     --address 127.0.0.1:3389 --username lab --credential <credential with the password>
#
# xrdp has no NLA (CredSSP): it speaks TLS, and the client logs in with
# the user name and the password in the RDP login packet. Its certificate
# is self-signed: the host agent pins it on the first connection.
FROM debian:bookworm-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      xrdp xorgxrdp openbox xterm dbus-x11 fonts-dejavu-core ssl-cert \
 && rm -rf /var/lib/apt/lists/*

# Each session runs openbox and an xterm.
RUN printf '#!/bin/sh\nxterm -geometry 100x30+40+40 &\nexec openbox-session\n' > /etc/xrdp/startwm.sh \
 && chmod 0755 /etc/xrdp/startwm.sh

COPY rdp-entrypoint.sh /usr/local/bin/terrarium-rdp
RUN chmod 0755 /usr/local/bin/terrarium-rdp

EXPOSE 3389
ENTRYPOINT ["/usr/local/bin/terrarium-rdp"]
