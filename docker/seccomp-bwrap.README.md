# seccomp-bwrap.json

`seccomp-bwrap.json` is Docker's default seccomp profile with one change: it
lets `bwrap` (bubblewrap) make user namespaces inside the container. The
agent sandbox (PLAN §11) needs this.

## Source

- Repository: <https://github.com/moby/profiles>, file `seccomp/default.json`.
  Moby vendors this file as its built-in default profile.
- Commit: `4b8214fc4971e06172938a1f1554ccff6647ee80` (2026-10-12).
- SHA-256 of the source file:
  `6416b47770785a41ac59073cdc77d9fe98517df2799dc83ef207e622de3053f6`.

## What changed

1. A new rule, second in the list, allows these syscalls with no conditions:
   `clone`, `clone3`, `unshare`, `setns`, `mount`, `mount_setattr`, `umount`,
   `umount2`, `pivot_root`, `chroot`, `sethostname`, `setdomainname`.
2. The `CAP_SYS_ADMIN` rule no longer lists those syscalls. The rest of that
   rule (`bpf`, `perf_event_open`, `fsopen`, `syslog`, and others) stays as it
   was.
3. The `CAP_SYS_CHROOT` rule is removed. It listed only `chroot`, which rule 1
   now allows.
4. Three rules are removed:
   - `clone` allowed only without namespace flags, when the container has no
     `CAP_SYS_ADMIN` (two rules: one for most arches, one for s390).
   - `clone3` fails with `ENOSYS` when the container has no `CAP_SYS_ADMIN`.

Nothing else changed. The default action is still `SCMP_ACT_ERRNO`.

## Why each syscall

- `clone`, `clone3`, `unshare`: make the new user, mount, PID, IPC, and UTS
  namespaces (`--unshare-*`).
- `mount`, `umount`, `umount2`, `pivot_root`, `chroot`, `mount_setattr`:
  build the sandbox root inside the new mount namespace.
- `setns`: join a namespace (used by some bwrap options).
- `sethostname`, `setdomainname`: `--hostname` inside the UTS namespace.

The container still has no `CAP_SYS_ADMIN`. These calls work only inside a
user namespace that the process made, so they do not give power over the host
or the container's own mounts.

## What else is needed

- `apparmor:unconfined` (in `compose.yaml`). The default Docker AppArmor
  profile denies `mount`.
- `systempaths=unconfined` (in `compose.yaml`). Docker masks paths under
  `/proc` (for example `/proc/kcore`) with bind mounts. The kernel then
  refuses a new procfs mount in a user namespace, so `bwrap --proc /proc`
  fails with `Can't mount proc on /proc: Operation not permitted`. This
  option removes the masks. The container still has no extra capabilities.
  Without it, bwrap works only with `--ro-bind /proc /proc` or with no
  `/proc`, and the sandbox cannot have a private `/proc`.
- The host kernel must permit unprivileged user namespaces. Some distributions
  turn them off, for example Ubuntu 24.04 and later with
  `kernel.apparmor_restrict_unprivileged_userns=1`.

Check it with `just spikes::bwrap-check` after `just docker::image-build`.

## Regenerate

Download the source file again and apply the same four changes. Then update
the commit and the SHA-256 above.
