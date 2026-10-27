# Test desktops for target displays

Images to try target displays (PLAN §19b, docs/api.md §17.10): two for a
docker target, and two servers for a `vnc` and an `rdp` target.
Terrarium does not ship them: a target image is your own.

| File | Display | Status |
|---|---|---|
| `x11.Dockerfile` | Xvfb, openbox, xterm, `xdotool`, ImageMagick | Verified on 2026-10-27: every operation of the docker X11 driver ran against it, as root and as a workspace owner (`docker run -u`). |
| `wayland.Dockerfile` | headless sway, foot, `grim`, `wtype`, `ydotool` | Not verified live. Screenshots, keys, and text need only the compositor. The pointer needs `ydotoold` and `/dev/uinput`, which Terrarium does not pass into a container. |
| `vnc.Dockerfile` | Xvfb, openbox, xterm, x11vnc with VNC authentication | Verified on 2026-10-27 with the host agent's VNC driver (docs/compat.md, "Target displays: VNC and RDP"). |
| `rdp.Dockerfile` | xrdp, xorgxrdp, openbox, xterm | Verified on 2026-10-27 with the host agent's RDP driver: TLS, no NLA (xrdp has none). |

## Use the X11 image

1. Build the image on the host of the host agent:

   ```sh
   docker build -t terrarium-x11-desktop -f examples/display/x11.Dockerfile examples/display
   ```

2. Make a target:

   ```sh
   terrarium admin target new desk --kind docker --host lab1 \
     --image terrarium-x11-desktop --display x11 --ephemeral
   ```

3. Give it to an agent with an exclusive lease, then try it:

   ```sh
   terrarium admin target grant <project>-desk <agent> --exclusive --until 1h
   terrarium screen <project>-desk info
   terrarium screen <project>-desk shot
   terrarium screen <project>-desk click 300 200
   terrarium screen <project>-desk type "echo hello"
   terrarium screen <project>-desk keys Return
   ```

Terrarium runs a docker target with its own command (`sleep infinity`), so
each image starts its display server in the `ENTRYPOINT` and then runs the
command. The X server listens on `:0`, which is the default `display_name`.

## Use the VNC and RDP servers

These are servers, not docker targets: the host agent reaches them by
address, as it reaches a VNC or RDP server on another machine. Run them on
loopback, on any free port.

1. Build and start them on the host of the host agent. Each password is
   an environment variable of the container; the entrypoint unsets it
   after use. `read -rs` keeps it out of the shell history:

   ```sh
   docker build -t terrarium-vnc-desktop -f examples/display/vnc.Dockerfile examples/display
   docker build -t terrarium-rdp-desktop -f examples/display/rdp.Dockerfile examples/display
   read -rs PW
   docker run -d --name vnc-test -e VNC_PASSWORD="$PW" -p 127.0.0.1:5901:5900 terrarium-vnc-desktop
   docker run -d --name rdp-test -e RDP_USER=lab -e RDP_PASSWORD="$PW" -p 127.0.0.1:3390:3389 terrarium-rdp-desktop
   ```

2. Store the password as a `password` credential (`POST /api/credentials`,
   [getting-started.md](../../docs/getting-started.md), "Add a screen
   target"), then make the targets:

   ```sh
   terrarium admin target new vnc-test --kind vnc --host lab1 --address 127.0.0.1:5901 --credential test-pw
   terrarium admin target new rdp-test --kind rdp --host lab1 --address 127.0.0.1:3390 \
     --username lab --credential test-pw
   ```

3. Grant each one with `--exclusive` and use `terrarium screen`, as above.

Notes:

- x11vnc speaks RFB 3.8 with VNC authentication and no TLS. Only the
  first 8 characters of a VNC password count.
- xrdp has no NLA. The host agent connects with TLS and sends the user
  name and the password in the RDP login packet. A wrong password does
  not fail the connection: the screen shows xrdp's "login failed" dialog.
- xrdp starts the desktop at the first login. Input in the first second
  can come before the xterm is on the screen: take a screenshot first.
- The host agent pins the certificate of `rdp-test` at the first
  connection. A new container of the same image has the same
  certificate. After a new image build, run `terrarium-host rdp-forget
  127.0.0.1:3390` on the host agent's machine.
- The VNC client of the host agent can send text outside ASCII, but the
  xterm of this image, with the C locale and the default keymap, does not
  show `é` or `€`.

## What an image needs

| Display | Packages | Notes |
|---|---|---|
| X11 | an X server (`Xvfb`, or a real one), `xdotool`, and ImageMagick (`import`), or `xwd` with `convert`, or `scrot` | A window manager makes clicks and focus behave as on a desktop. |
| Wayland | a wlroots compositor (sway, cage, labwc), `grim`, `wtype`, `ydotool` | GNOME and KDE refuse synthetic input. `ydotool` needs `ydotoold` and `/dev/uinput`. Set `display_name` to the absolute socket path when the image does not set `XDG_RUNTIME_DIR` for `docker exec`. |
| OCR | `tesseract` on the host agent's machine, not in the image | vmlab targets have their own OCR. |

A missing tool is `422` with `details.code` `unsupported`, and the message
names the package to install.
