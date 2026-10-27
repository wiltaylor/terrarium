# A test desktop for a docker target with `display = "x11"` (PLAN §19b,
# docs/api.md §17.10): Xvfb, the openbox window manager, an xterm, xdotool
# for input, and ImageMagick (`import`) for screenshots.
#
#   docker build -t terrarium-x11-desktop -f examples/display/x11.Dockerfile examples/display
#
# Target:
#
#   kind = "docker"
#   host = "lab1"
#   image = "terrarium-x11-desktop"
#   display = "x11"            # display_name defaults to ":0"
#
# Terrarium runs the image with its own command (`sleep infinity`), so the
# X server starts in the entrypoint, not in CMD.
FROM debian:bookworm-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      xvfb openbox xterm xdotool imagemagick x11-apps fonts-dejavu-core \
 && rm -rf /var/lib/apt/lists/*

ENV DISPLAY=:0 \
    SCREEN_SIZE=1280x800x24

COPY x11-entrypoint.sh /usr/local/bin/terrarium-desktop
RUN chmod 0755 /usr/local/bin/terrarium-desktop

ENTRYPOINT ["/usr/local/bin/terrarium-desktop"]
CMD ["sleep", "infinity"]
