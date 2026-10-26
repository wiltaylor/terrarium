// The host installer (docs/api.md §17.9, scripts/install-host.sh): the line
// that installs terrarium-host on a machine with an enrol token. The daemon
// sends it as `install_command` in the answer of `POST /api/hosts` with
// `enrol`; when it does not know its own URL, the dashboard makes the same
// line from the origin of the page.

import type { NewHostToken } from "../api/types";

/** `curl -fsSL <origin>/install/host.sh | sudo sh -s -- --token <token> --name <name>`, as the daemon's `install_command`. */
export function installCommand(origin: string, token: string, name: string): string {
  return `curl -fsSL ${origin.replace(/\/+$/, "")}/install/host.sh | sudo sh -s -- --token ${token} --name ${name}`;
}

/** The daemon's line, else one made from the page origin. */
export function installLine(made: Pick<NewHostToken, "install_command" | "token" | "name">, origin: string): string {
  return made.install_command ?? installCommand(origin, made.token, made.name);
}

/** What the installer needs and does, for the Add host dialog. */
export const INSTALL_NOTES: readonly string[] = [
  "Run it on the host as a user with sudo. The host needs Linux on x86_64, and systemd for the service (without systemd, the script prints the command to run).",
  "The script finds Docker and vmlab by itself. The host agent runs as the user who ran sudo, because vmlab runs as the user who owns the labs.",
  "The enrol token works one time, for 10 minutes. At its first connect, the host agent swaps it for its long-lived token.",
  "Add --dry-run to see the plan first. Later, run the same script with --upgrade or --uninstall.",
];
