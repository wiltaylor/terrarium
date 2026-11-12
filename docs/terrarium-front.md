# Maintenance front

`terrarium-front` is a separate, unprivileged container in the same image. It
owns ports 8080 and 8083. The daemon keeps 8081 (artifacts) and 8082 (host dial-in).
The front proxies HTTP bodies as streams and tunnels WebSocket upgrades. It does
not log requests, rewrite authentication headers, or retry failed requests.
A failed connection or two-second response-header timeout returns 503 with
`Retry-After: 2`. A browser navigation receives the maintenance page. API, CLI,
and webhook requests receive JSON. The page polls every two seconds and reloads
when the daemon answers. A timeout does not prove a mutating request was not
applied. Clients must not blindly replay non-idempotent requests.

Only the updater's `front/current-run.json` export directory is mounted read-only,
never its socket, Docker socket, or main state directory (which contains private
container settings). The export retains the last run marked ended after it
finishes. Only known step names and elapsed time are public. An existing browser
operator sees sanitized failures, never command output or container settings.
Detailed diagnostics remain in authenticated updater status. The updater pin
must be advanced with this release to provide the atomic live progress export.
An existing browser
operator session can also see the step list, versions, and errors. The daemon
exports SHA-256 hashes of validated browser tokens, with their expiration, to
`state/front/sessions.json` using atomic rename and mode 0600. Logout removes
all exported tokens of that session. Password changes clear the export. No
password, bearer token, signing key, or raw session token is exported. The
front compares hashes in constant time and never creates sessions. Missing,
invalid, future-dated, or older-than-five-minute exports give only the public
view. Idle sessions not seen recently may therefore see only the public view.
Login with a password waits until the daemon returns. No crashcart action or
new daemon HTTP route is added.

## First installation and updates

This release requires the full container-swap procedure, not a hot swap.
Install the new compose file, create `state/front` (mode 0700) and
`updater/state/front` owned by the daemon/updater UID and GID, then recreate
the daemon and start the front together. Never
start the front while an old daemon still publishes 8080/8083. The deploy helper
and `just docker::volumes-init` create the directory for fresh installations.

The front stays up during daemon swaps. Update it separately and rarely, with
`docker compose up -d --no-deps terrarium-front`, only in a planned maintenance
window. Pin `TERRARIUM_FRONT_IMAGE` to keep it on its installed version while
changing the daemon image. Its binary comes from the bundled image payload,
not the daemon's mutable release directory. Updating the front itself briefly
interrupts the published ports. Roll back the front by restoring its image pin.
