# Test desktops for target displays

Two images to try target displays (PLAN §19b, docs/api.md §17.10) on a
docker target. Terrarium does not ship them: a target image is your own.

| File | Display | Status |
|---|---|---|
| `x11.Dockerfile` | Xvfb, openbox, xterm, `xdotool`, ImageMagick | Verified on 2026-10-27: every operation of the docker X11 driver ran against it, as root and as a workspace owner (`docker run -u`). |
| `wayland.Dockerfile` | headless sway, foot, `grim`, `wtype`, `ydotool` | Not verified live. Screenshots, keys, and text need only the compositor. The pointer needs `ydotoold` and `/dev/uinput`, which Terrarium does not pass into a container. |

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

## What an image needs

| Display | Packages | Notes |
|---|---|---|
| X11 | an X server (`Xvfb`, or a real one), `xdotool`, and ImageMagick (`import`), or `xwd` with `convert`, or `scrot` | A window manager makes clicks and focus behave as on a desktop. |
| Wayland | a wlroots compositor (sway, cage, labwc), `grim`, `wtype`, `ydotool` | GNOME and KDE refuse synthetic input. `ydotool` needs `ydotoold` and `/dev/uinput`. Set `display_name` to the absolute socket path when the image does not set `XDG_RUNTIME_DIR` for `docker exec`. |
| OCR | `tesseract` on the host agent's machine, not in the image | vmlab targets have their own OCR. |

A missing tool is `422` with `details.code` `unsupported`, and the message
names the package to install.
