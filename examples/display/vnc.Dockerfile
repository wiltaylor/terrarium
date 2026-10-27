# A test VNC server for a `vnc` target (PLAN §19b, docs/api.md §17.10):
# Xvfb, the openbox window manager, an xterm, and x11vnc with VNC
# authentication.
#
#   docker build -t terrarium-vnc-desktop -f examples/display/vnc.Dockerfile examples/display
#   docker run -d --name vnc-test -e VNC_PASSWORD=<password> -p 127.0.0.1:5900:5900 terrarium-vnc-desktop
#
# Target:
#
#   terrarium admin target new vnc1 --kind vnc --host lab1 \
#     --address 127.0.0.1:5900 --credential <credential with the password>
#
# x11vnc speaks RFB 3.8 with security type 2 (VNC authentication) and no
# TLS: keep the port on loopback, a trusted network, or a tunnel.
FROM debian:bookworm-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      xvfb openbox xterm x11vnc fonts-dejavu-core \
 && rm -rf /var/lib/apt/lists/*

ENV DISPLAY=:0 \
    SCREEN_SIZE=1280x800x24

COPY vnc-entrypoint.sh /usr/local/bin/terrarium-vnc
RUN chmod 0755 /usr/local/bin/terrarium-vnc

EXPOSE 5900
ENTRYPOINT ["/usr/local/bin/terrarium-vnc"]
