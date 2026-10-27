# A test desktop for a docker target with `display = "wayland"` (PLAN §19b):
# a headless sway (a wlroots compositor), foot, grim for screenshots, wtype
# for keys and text, and ydotool for the pointer.
#
#   docker build -t terrarium-wayland-desktop -f examples/display/wayland.Dockerfile examples/display
#
# Target:
#
#   kind = "docker"
#   host = "lab1"
#   image = "terrarium-wayland-desktop"
#   display = "wayland"
#   display_name = "/tmp/xdg/wayland-1"   # an absolute socket path
#
# Limits (see README.md): screenshots, keys, and text work. The pointer
# needs ydotoold with /dev/uinput, which Terrarium does not pass into a
# container; use the X11 image for mouse work, or a vnc target (step 2).
# Not verified live: the X11 image is the tested one.
FROM alpine:3.22

RUN apk add --no-cache sway foot grim wtype ydotool font-dejavu

ENV XDG_RUNTIME_DIR=/tmp/xdg \
    WLR_BACKENDS=headless \
    WLR_LIBINPUT_NO_DEVICES=1 \
    WLR_RENDERER=pixman

COPY wayland-entrypoint.sh /usr/local/bin/terrarium-desktop
RUN chmod 0755 /usr/local/bin/terrarium-desktop

ENTRYPOINT ["/usr/local/bin/terrarium-desktop"]
CMD ["sleep", "infinity"]
