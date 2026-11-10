# Native GPU enforcement: the root helper

TER-345. This page is the design note, the security boundary, and the
install guide of native GPU enforcement. It is off by default. Nothing in
it changes a host until an operator installs the helper.

## The problem

TER-250 enforces GPU reservations for managed ephemeral Docker targets:
the container gets only its reserved GPUs. A native `host` target runs its
commands in systemd **user** scopes of the host agent's user. A user scope
has no device controls, so a native process can open every
`/dev/nvidia<N>`, also a GPU that another holder reserved exclusively.
Device access control in cgroup v2 is a BPF program that only root (the
system manager) installs. The host agent is not root.

## The guarantee

**Managed-session exclusivity** (lead decision Q1). With the helper
installed and `[native_gpu] enabled = true`:

- Each native session on a `host` target starts in a device policy that
  allows only the GPUs that the daemon granted it, before the first
  instruction of user code. A session without a GPU reservation gets no
  GPU.
- No two reservations' native sessions hold one exclusively reserved GPU
  at the same time. The helper refuses a session while another
  reservation's session still runs on that GPU (`busy`), so capacity is
  not handed on while an old process can still hold a device handle.
- Each holder's sessions run under that holder's own workload UID from a
  root-configured pool (lead decision Q2, revised after the security
  review). A session of holder B cannot ptrace, `pidfd_getfd`,
  `process_vm_writev`, read `/proc/<pid>/mem`, or signal a session of
  holder A, so it cannot borrow A's open GPU handles: the kernel checks
  the UID for each of these.
- A native session cannot reach the helper, the host agent's user bus, or
  the Docker socket.
- The claim of a session is freed only after systemd confirms its unit
  stopped. A unit that is not confirmed stopped keeps its GPUs unavailable
  to everyone and its UID with its holder, and the helper tries again.
- When enforcement is lost while a native exclusive GPU reservation is
  managed on the host, the daemon refuses every `host` target command on
  that host (fail closed), instead of running it without device policy.

Not covered: processes that Terrarium did not start (an operator login, a
non-Terrarium service). The daemon never calls a native reservation
host-wide exclusive. Hardware validation on a dedicated GPU host is a
follow-up card (Q4 is the operator's decision).

## The design (lead decision Q3: O4 + O1)

```
daemon --exec{limits.native_gpu}--> host agent (agent user)
           host agent --spawn--> terrarium-host native (shim, agent user)
                  shim --Unix socket, framed--> terrarium-host-helper (root)
                         helper --> systemd-run --system --uid=<holder's pool UID> \
                                    -p DevicePolicy=closed -p DeviceAllow=/dev/nvidia<N> rw ...
```

- **The helper** (`terrarium-host-helper`, `crates/terrarium-host/src/bin/`)
  is a small root systemd service. It listens on one Unix socket, mode
  0600, owned by the agent user, and checks `SO_PEERCRED` on each
  connection: only the agent UID gets an answer.
- **The request is typed** (`crates/terrarium-host/src/native.rs`,
  `Request`): `hello`, `apply`, `stop`, `start`. Every struct refuses
  unknown fields. The caller never sends a unit name, a unit property, a
  device path or number, a slice name, or a user. The helper derives each
  of them:
  - the unit: `terrarium-native-<session>-<nonce>.service` (digits and hex);
  - the slice: `crate::limits::slice_name(target, holder)` (escaped);
  - the devices: the helper runs `nvidia-smi --query-gpu=uuid,minor_number`
    itself, maps each granted UUID to `/dev/nvidia<minor>`, and checks that
    the node is a character device `195:<minor>` (no symlink). Unknown
    UUIDs, a node that does not match, or a missing support node
    (`nvidiactl`, `nvidia-uvm`, `nvidia-uvm-tools`) fail closed;
  - the slice limits: only from typed `RuntimeLimits`
    (`crate::limits::reservation_properties`);
  - the working directory: resolved by root, in the holder's workspace
    scope (see UID reuse below) or the workspace root itself, no `%`, no
    control characters;
  - the user: the holder's UID from `workload_users`. A holder keeps its
    UID while it has a live (or stuck) session; a UID never serves two
    holders at once; with no free UID the session is refused (`busy`, "no
    free workload user"). Before each session the helper looks the user up
    again and checks its groups and linger, so a later `usermod` or
    `loginctl enable-linger` does not reach a new session.
  - UID reuse: a pool UID can pass to another holder once its holder's
    last session is confirmed stopped. Nothing that the UID owns from the
    old holder is reachable by the new one:
    - The workspace: each unit gets `TemporaryFileSystem=<workspace_root>:ro`
      (an empty read-only tmpfs) with only its holder's workspace scope
      bound in (`BindPaths=`, `ReadWritePaths=`). The scope follows the
      holder's trust domain:
      - an agent `<project>/<agent>`: only
        `<workspace_root>/<project>/<agent>`;
      - a project service `<project>/@jobs`, `<project>/@pipelines`,
        `<project>/@monitors`, or `<project>/@schedules` (a project-owned
        schedule; a system-owned one is `global/@schedules`): the
        project's directory `<workspace_root>/<project>` (job slots
        `slot-<n>` and the agent directories of that project), never
        another project's. On such a host, set the delivery `work_dir`
        under `workspace_root`: the default `/tmp/terrarium-jobs` is
        refused (see below);
      - `operator`: the whole workspace root, with no tmpfs over it (the
        operator is trusted). Only a command of the operator (the actor,
        or a `run_as = "operator"` webhook) gets this holder: the daemon
        refuses a command with no holder on such a host, so a path that
        forgets the holder never gets the whole root. The daemon's own fixed
        scripts for a card agent's owned root (the python probe, the home
        fallback, prepare, measure, delete, also the sweep of an ended
        agent) run with this operator scope, like the kill and clear
        scripts of a run: they are daemon-written, take only checked names,
        and run no agent code. The agent's own execs keep the holder
        `<project>/<agent>` and see only `<workspace_root>/<project>/<agent>`.
        So on such a host set the target's `work_root` to `workspace_root`:
        the owned root `<work_root>/<project>/<agent>` is then inside the
        agent's own directory scope. The default `/src/terrarium` is
        outside every scope and read-only in the unit. Prepare refuses an
        existing directory without its owner marker, so the owned root
        works only where `<workspace_root>/<project>/<agent>` does not
        exist before the first prepare.

      Directories outside the scope do not exist in the unit, whatever
      their modes, so a file that an earlier holder of the UID made private
      (a credential store, a key, a `mkstemp` file) cannot be opened from
      another trust domain. The working directory must lie in the scope.
      Any other holder, or a scope directory that does not exist yet, gets
      the empty root only. A scope that is a symlink is refused. No agent
      can take a service holder: agent names never contain `@`, and the
      bare names `jobs`, `pipelines`, `monitors`, and `schedules` are
      reserved for agents (an agent of that name made before them is the
      holder `<project>/jobs` and gets only its own directory).
    - The rest of the tree: `ProtectSystem=strict` makes it read-only
      except the API trees (`/dev`, `/proc`, `/sys`), so no world-writable
      directory such as `/run/lock` or `/var/crash` keeps a file of the UID.
    - `PrivateTmp=yes` gives the unit its own `/tmp` and `/var/tmp`, removed
      with the unit. `RemoveIPC=yes` makes systemd remove the System V and
      POSIX IPC objects of the UID, also its files in `/dev/shm`, when the
      last unit of that UID stops.
    - The pool users have no home directory (see the install steps).
    - `workspace_root` must not be under `/tmp` or `/var/tmp` (each unit
      gets a private copy of them): the helper refuses to start. A workspace
      root or scope path with whitespace, quotes, `%`, `:`, or a backslash
      is refused, since it goes in systemd path lists. The working
      directory is not in such a list: only `%` and control characters are
      refused there.
- **The session** is a transient system service, not a scope: systemd sets
  the user, the groups, and the device policy before `exec`, and
  `KillMode=control-group` ends every descendant with the main process.
  Properties: `--uid`/`--gid` of the holder's workload user,
  `NoNewPrivileges=yes`, `CapabilityBoundingSet=`, `AmbientCapabilities=`,
  `RestrictSUIDSGID=yes`, `PrivateTmp=yes`, `RemoveIPC=yes`,
  `ProtectSystem=strict`, `TemporaryFileSystem=<workspace_root>:ro`,
  `BindPaths=` and `ReadWritePaths=` of the holder's workspace scope,
  `DevicePolicy=closed`, `DeviceAllow=char-pts rw`, one `DeviceAllow=` for
  each granted GPU and support node, `--expand-environment=no`. The
  environment goes in a root-only `EnvironmentFile` under `state_dir`, not
  on a command line.
- **The shim** (`terrarium-host native`, hidden subcommand) is what the
  host agent's session runner starts. The request travels in
  `TERRARIUM_NATIVE_REQUEST` (environment, readable only by the same UID).
  The shim relays stdin, stdout, stderr, the forwarded signals (HUP, INT,
  QUIT, USR1, USR2, TERM), and the PTY size (`stty size` after `SIGWINCH`).
  A PTY session gets its own PTY in the helper (`systemd-run --pty`); the
  shim puts the outer one in raw mode. A kill of the session kills the
  shim, the helper sees the end of the connection and kills the unit.
  The request must fit in one environment string: a command and
  environment over 128 KiB of JSON is refused by the host agent with its
  size (Linux `MAX_ARG_STRLEN`). A session that a signal ended exits with
  code 128 + N through the shim (the `exit` frame then has a code, not a
  signal), as a shell reports it.
- **Stop confirmation**: when `systemd-run` ends, the helper sends SIGKILL
  to the unit and waits, at most 30 s, until `systemctl show
  --property=ActiveState` reports it inactive, failed, or unknown, and its
  `ControlGroup` is gone or `populated 0` in `cgroup.events` (a failed unit
  can keep processes after a stop timed out). A cgroup directory without
  `cgroup.events`, or a tree that is not cgroup v2, counts as not stopped.
  Only then is the claim freed. Otherwise it logs a warning, keeps the
  claim marked stuck, and checks again every 10 s.
- **Start**: the helper stops the `terrarium-native-*.service` units of an
  earlier run before it serves, and waits until each is inactive. When the
  list fails or a unit does not stop, it exits (fail closed). It checks
  that the config, the binaries, the socket directory, and the state
  directory, and every parent of each, are owned by root and not writable
  by group or others. A path is resolved one component at a time: each
  directory on the way, also one that a symlink leads to, must pass that
  check, and each symlink (also the middle links of a chain) must be owned
  by root and sit in a directory that passed, at most 40 symlinks. So no
  link on the way can be repointed by another user. The helper then runs
  `systemd_run`, `systemctl`, and `nvidia_smi`, and reads the config,
  through the resolved paths. It refuses to start without cgroup v2
  (`/sys/fs/cgroup/cgroup.controllers`). The state directory must not be a
  symlink, and its existing parents are checked before the helper creates
  it or changes its mode. GPU sessions need `nvidia_smi` verified the same way
  at start; otherwise they are refused with the reason.
- **Rights** (separate per operation): the config lists the rights of the
  agent user. `session` covers `apply`, `stop`, and `start`. With no right,
  only `hello` answers. The disk quota operations of TER-251 get their own
  right in the same helper; `session` does not grant them, and they do not
  grant `session`.
- **Version handshake**: `hello` carries the protocol version (1). A
  helper that is absent, refuses the caller, has another version, or does
  not grant `session` makes the host agent report
  `capabilities.native_gpu.enforced = false` with the reason. The host
  agent asks again before each `hello` to the daemon (each reconnect).

### Daemon side

- `capabilities.native_gpu` (`terrarium_core::protocol::NativeGpuCapability`):
  `{enforced, version, reason}`. Absent on older host agents.
- Admission (`crates/terrariumd/src/reservations.rs`): an exclusive GPU
  claim on a native `host` target needs `native_gpu.enforced`. Otherwise
  it is a `409` with the reason. SSH and vmlab stay refused.
- Every command on a `host` target of such a host carries
  `exec.limits.native_gpu` (`crates/terrariumd/src/executor.rs`,
  `native_gpu_limits`): the holder's reservation ID as `lease`, its
  exclusive UUIDs, and its GPUs in `reservation.gpu_devices`. A command
  with no reservation still gets `native_gpu` with no GPU. On any other
  host the field is dropped.
- Fail closed (`reservations.rs`, `native_gpu_admission`): while a managed
  exclusive GPU reservation of a native `host` target is on the host and
  the host agent does not report `native_gpu.enforced` (the helper is down,
  has another version, `[native_gpu]` is off, or the agent is older),
  every `host` target command on that host is a `409` that names the
  reservation and the reason. Any native process there could open the
  reserved GPU. Docker targets keep their create-time allowlist.
- `resource.apply` goes to the helper's `apply` when the helper answered
  with enforcement at the last `hello`, and to the user slices otherwise.
  The host agent routes it by that answer, not by its config, so a host
  agent whose helper is down keeps managing the user slices where its
  sessions then run.
- `resource.stop` goes through the shim whenever `[native_gpu]` is in the
  config, whatever the helper answered: the shim stops the user slice and
  the helper's slice, and fails when the helper cannot be reached. Native
  units can still run after the helper stopped answering, or after the
  host agent restarted, so a stop never reports success without the
  helper. Without `[native_gpu]`, only the user slice stops. `stop` waits
  for the units to stop: the daemon keeps the reservation `ending` until
  then.

### Attribution

Native units sit in the system slice of their holder, which has the same
name as the user slice. The existing holder attribution of
`metrics.gpu_usage` (a path segment equal to the slice name) works
unchanged. The unit name in the cgroup path carries the session ID, from
the cgroup at sample time, not from a PID that can be reused. Compute VRAM
only; unknown and failed samples stay unknown. Monitoring never enforces.

## The trust boundary

| Party | Identity | Can |
| --- | --- | --- |
| helper | root | start a unit as a pool user, set and stop Terrarium slices, read GPU inventory |
| host agent | agent user | call the helper with the rights in its config |
| session | its holder's pool user | use its granted GPUs, its workspace, nothing of the helper or of another holder's sessions |

What a caller with the agent UID can do through the helper: start any
command, but only as a pool user (never root, never the agent user, never
a UID that another holder's live session uses),
only in a closed device policy, only with GPUs from the live inventory,
only in a working directory under `workspace_root`, and set or stop slices
whose names the helper derives. It cannot pass properties, units, paths,
devices, or users. A compromised host agent can therefore misuse GPU
grants between its own holders, but it cannot gain root or reach devices
other than the GPUs and their support nodes.

Negative tests (`crates/terrarium-host/src/native/tests.rs`,
`crates/terrarium-host/tests/native_helper.rs`): a caller with another UID,
a missing right, an old protocol version, unknown fields (a smuggled
property, unit, or slice), a UID of another holder, a used-up pool, a
second reservation on an exclusive GPU, a session without a grant on an
exclusive GPU, a stale lease of the same holder, the GPU of a unit that is
not confirmed stopped, leftover units that do not stop or cannot be listed,
a workload user that changed since the start, GPU sessions without a
verified `nvidia-smi`, a trusted path with a writable parent, a working
directory outside the root
(directly or by a symlink), `%` in a path, control characters, NUL in argv,
bad environment names and newlines in values, unknown GPUs, device nodes
that do not match the inventory, missing support nodes, and `stop` of a
shared parent slice.

Known limits:

- Holder means the `holder` of the command (the agent, or `operator`).
  Several agents that use one target's reservation share its GPUs, each
  under its own UID. The pool needs as many users as holders that run
  native sessions at one time on the host.
- File ownership: sessions write the workspace as their pool user. The
  workspace root needs a shared group with the setgid bit, or ACLs, so the
  host agent and the sessions can both write (install step 4). A native
  session sees only its holder's workspace scope (see UID reuse above);
  outside native sessions, holders can read and write each other's
  workspaces through that group, as with the host agent today.
- **The agent user must never run content from the workspace** (scripts,
  hooks, binaries, git configuration it does not control). Sessions can
  write there, and the agent UID is the helper's trusted caller. Today
  git's `safe.directory` owner check covers the host agent's own git calls
  under `instances_dir`.
- On an enforced host, every `host` target command runs as a pool user,
  also daemon-internal ones (target root probes, workspace sync, monitors,
  pipelines). Its working directory must be under `workspace_root`: a
  `cwd` that `allowed_cwd_roots` permits outside it, or no `cwd` while
  `work_dir` is outside it, fails with exit code 125 and the reason.
- A helper that goes away between two `hello`s makes each native command
  fail with exit code 125 until the next reconnect reports the loss; the
  daemon then refuses host commands while a native exclusive GPU
  reservation is managed, and the host agent manages the user slices
  again.
- A native session no longer inherits the host agent's environment: it
  gets systemd's service environment plus the command's `env`.
- Removing a device from a policy does not revoke an open handle. The
  helper never changes the policy of a running unit; it only starts new
  units, and ends a unit when its session ends.
- MPS or other GPU proxy services can run work outside the session's
  cgroup. Do not run them on a host with native exclusivity.
- `nvidia-modprobe` is setuid and does not work under `NoNewPrivileges`:
  load `nvidia-uvm` at boot so `/dev/nvidia-uvm` exists.

## Install guide

For an operator. Run it only on a host that the operator chose (Q4). Do
not run it on a shared target or a DGX Spark without that decision.

1. **Requirements**: cgroup v2, systemd 254 or newer
   (`systemctl --version`), the NVIDIA driver with `/dev/nvidiactl`,
   `/dev/nvidia-uvm`, and `/dev/nvidia-uvm-tools` present at boot
   (`nvidia-modprobe -u -c=0` from a boot unit, or `modprobe nvidia-uvm`).
   No MIG.
2. **The workload pool**: one system user for each holder that may run
   native sessions at the same time (for example four). Each has its own
   UID and its own primary group, is not the agent user, has no login, no
   linger, no home directory (a writable home would carry files from one
   holder to the next holder of the UID), and none of the groups `root
   wheel sudo admin adm docker libvirt lxd incus-admin disk kvm
   systemd-journal`. The helper refuses to start
   otherwise, and checks the groups and linger again before each session.

   ```sh
   for i in 1 2 3 4; do
     useradd --system --user-group --no-create-home --shell /usr/sbin/nologin terrarium-w$i
     loginctl disable-linger terrarium-w$i
   done
   ```

3. **The binary**: download it from the GitHub release of the daemon's
   version and verify it; do not build it. Each release has
   `terrarium-host-helper-<version>-linux-x86_64` (static musl, built
   with `--locked` in the release CI, next to `terrarium-host`) and its
   line in `SHA256SUMS` ([release.md](release.md)). Then install it
   root-owned:

   ```sh
   v=<version>   # the version of the daemon and the host agent, without the v
   gh release download "v$v" --repo wiltaylor/terrarium \
     --pattern "terrarium-host-helper-$v-linux-x86_64" --pattern SHA256SUMS
   sha256sum -c --ignore-missing SHA256SUMS
   gh attestation verify "terrarium-host-helper-$v-linux-x86_64" --repo wiltaylor/terrarium \
     --signer-workflow wiltaylor/terrarium/.github/workflows/ci.yml
   install -o root -g root -m 0755 "terrarium-host-helper-$v-linux-x86_64" /usr/local/libexec/terrarium-host-helper
   /usr/local/libexec/terrarium-host-helper --version
   ```

   Without `gh`, take both files from the release page with `curl -fLO`
   and run the `sha256sum` check. A release up to v0.35.0-alpha has no
   such file: build it from the tag with `cargo build
   --release --locked -p terrarium-host --bin terrarium-host-helper`.

4. **The workspace**: a group that both users share, with the setgid bit,
   so files stay writable for the host agent and the sessions:

   ```sh
   groupadd terrarium-ws
   usermod -aG terrarium-ws <agent user>
   for i in 1 2 3 4; do usermod -aG terrarium-ws terrarium-w$i; done
   chgrp -R terrarium-ws /srv/terrarium/work
   chmod -R g+rwX /srv/terrarium/work
   find /srv/terrarium/work -type d -exec chmod g+s {} +
   ```

5. **The config** `/etc/terrarium-host-helper/config.toml`, owned by root,
   mode 0644 or stricter, in directories owned by root and not writable by
   group or others (the helper checks the file and each parent):

   ```toml
   agent_user = "terrarium"              # the host agent's user
   workload_users = ["terrarium-w1", "terrarium-w2", "terrarium-w3", "terrarium-w4"]
   workspace_root = "/srv/terrarium/work"
   rights = ["session"]
   # socket = "/run/terrarium-host-helper/helper.sock"
   # state_dir = "/run/terrarium-host-helper/sessions"
   # systemd_run = "/usr/bin/systemd-run"
   # systemctl = "/usr/bin/systemctl"
   # [gpu]
   # nvidia_smi = "/usr/bin/nvidia-smi"
   # dev_dir = "/dev"
   # support_nodes = ["/dev/nvidiactl", "/dev/nvidia-uvm", "/dev/nvidia-uvm-tools"]
   ```

6. **The service** `/etc/systemd/system/terrarium-host-helper.service`:

   ```ini
   [Unit]
   Description=Terrarium host helper (native GPU enforcement)
   After=systemd-udevd.service

   [Service]
   ExecStart=/usr/local/libexec/terrarium-host-helper --config /etc/terrarium-host-helper/config.toml
   RuntimeDirectory=terrarium-host-helper
   RuntimeDirectoryMode=0755
   Restart=on-failure
   UMask=0077

   [Install]
   WantedBy=multi-user.target
   ```

   Check first, then start:

   ```sh
   /usr/local/libexec/terrarium-host-helper --config /etc/terrarium-host-helper/config.toml --check
   systemctl daemon-reload && systemctl enable --now terrarium-host-helper
   ```

7. **The host agent**: in `/etc/terrarium-host/config.toml`, then restart
   it:

   ```toml
   [native_gpu]
   enabled = true
   # socket = "/run/terrarium-host-helper/helper.sock"
   ```

   `terrarium-host check --json` shows `capabilities.native_gpu`. With
   `enforced: true`, the daemon accepts exclusive GPU claims on native
   `host` targets of this host.

**Turning it off**: end or retract the native exclusive GPU reservations
of the host first. Then set `[native_gpu] enabled = false` and restart the
host agent. The daemon then refuses new native exclusive GPU claims on the
host; while one is still managed, it refuses the host's `host` target
commands (fail closed). Running units end with their sessions. `systemctl disable --now
terrarium-host-helper` removes the helper; on its next start it kills the
units that an earlier run left (`terrarium-native-*` only).
