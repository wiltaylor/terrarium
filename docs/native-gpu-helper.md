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
- A native session cannot reach the helper, the host agent's user bus, or
  the Docker socket: it runs as a separate workload user.

Not covered: processes that Terrarium did not start (an operator login, a
non-Terrarium service). The daemon never calls a native reservation
host-wide exclusive. Hardware validation on a dedicated GPU host is a
follow-up card (Q4 is the operator's decision).

## The design (lead decision Q3: O4 + O1)

```
daemon --exec{limits.native_gpu}--> host agent (agent user)
           host agent --spawn--> terrarium-host native (shim, agent user)
                  shim --Unix socket, framed--> terrarium-host-helper (root)
                         helper --> systemd-run --system --uid=<workload> \
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
  - the working directory: resolved by root, under `workspace_root`, no
    `%`, no control characters.
- **The session** is a transient system service, not a scope: systemd sets
  the user, the groups, and the device policy before `exec`, and
  `KillMode=control-group` ends every descendant with the main process.
  Properties: `--uid`/`--gid` of the workload user, `NoNewPrivileges=yes`,
  `CapabilityBoundingSet=`, `AmbientCapabilities=`, `RestrictSUIDSGID=yes`,
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
- `resource.apply` and `resource.stop` of a native holder go to the
  helper's `apply` and `stop`, so CPU isolation and the cleanup at the end
  of a reservation reach the system slices. `stop` waits for the units to
  stop: the daemon keeps the reservation `ending` until then.

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
| helper | root | start a unit as the workload user, set and stop Terrarium slices, read GPU inventory |
| host agent | agent user | call the helper with the rights in its config |
| session | workload user | use its granted GPUs, its workspace, nothing of the helper |

What a caller with the agent UID can do through the helper: start any
command, but only as the workload user (never root, never the agent user),
only in a closed device policy, only with GPUs from the live inventory,
only in a working directory under `workspace_root`, and set or stop slices
whose names the helper derives. It cannot pass properties, units, paths,
devices, or users. A compromised host agent can therefore misuse GPU
grants between its own holders, but it cannot gain root or reach devices
other than the GPUs and their support nodes.

Negative tests (`crates/terrarium-host/src/native/tests.rs`,
`crates/terrarium-host/tests/native_helper.rs`): a caller with another UID,
a missing right, an old protocol version, unknown fields (a smuggled
property, unit, or slice), a forged holder on a live lease, a second
reservation on an exclusive GPU, a session without a grant on an exclusive
GPU, a stale lease of the same holder, a working directory outside the root
(directly or by a symlink), `%` in a path, control characters, NUL in argv,
bad environment names and newlines in values, unknown GPUs, device nodes
that do not match the inventory, missing support nodes, and `stop` of a
shared parent slice.

Known limits:

- One workload UID for all holders (lead decision Q2). Two holders'
  sessions can signal or read each other's processes. One UID per holder
  is a follow-up if the security review asks for it.
- File ownership: sessions write the workspace as the workload user. The
  workspace root needs a shared group with the setgid bit, or ACLs, so the
  host agent and the sessions can both write (install step 4).
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
2. **The workload user**: a system user that is not the agent user, has
   no login, no linger, and none of the groups `root wheel sudo admin adm
   docker libvirt lxd incus-admin disk kvm systemd-journal`. The helper
   refuses to start otherwise.

   ```sh
   useradd --system --no-create-home --shell /usr/sbin/nologin terrarium-work
   loginctl disable-linger terrarium-work
   ```

3. **The binary**, root-owned:

   ```sh
   install -o root -g root -m 0755 terrarium-host-helper /usr/local/libexec/terrarium-host-helper
   ```

4. **The workspace**: a group that both users share, with the setgid bit,
   so files stay writable for the host agent and the sessions:

   ```sh
   groupadd terrarium-ws
   usermod -aG terrarium-ws <agent user>
   usermod -aG terrarium-ws terrarium-work
   chgrp -R terrarium-ws /srv/terrarium/work
   chmod -R g+rwX /srv/terrarium/work
   find /srv/terrarium/work -type d -exec chmod g+s {} +
   ```

5. **The config** `/etc/terrarium-host-helper/config.toml`, owned by root,
   mode 0644 or stricter (the helper refuses a file that group or others
   can write):

   ```toml
   agent_user = "terrarium"              # the host agent's user
   workload_user = "terrarium-work"
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

**Turning it off**: set `[native_gpu] enabled = false` and restart the
host agent. The daemon then refuses new native exclusive GPU claims on the
host. Running units end with their sessions. `systemctl disable --now
terrarium-host-helper` removes the helper; on its next start it kills the
units that an earlier run left (`terrarium-native-*` only).
