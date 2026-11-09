#!/usr/bin/env python3
"""Run the ignored tunnel integration test against a scratch-local SSH daemon.

Use only on an explicitly granted scratch VM with Docker. All keys, pins and
the loopback-only sshd are temporary. Production CI identities are not used.
"""
import argparse
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ter-363-live-") as root:
        root = Path(root)
        identity, host, wrong_host = root / "identity", root / "host", root / "wrong_host"
        for key in (identity, host, wrong_host):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True)
        with socket.socket() as reserve:
            reserve.bind(("127.0.0.1", 0))
            port = reserve.getsockname()[1]
        pin = root / "known_hosts"
        pin.write_text(f"[127.0.0.1]:{port} {host.with_suffix('.pub').read_text()}")
        wrong_pin = root / "wrong_pin"
        wrong_pin.write_text(f"[127.0.0.1]:{port} {wrong_host.with_suffix('.pub').read_text()}")
        authorized = root / "authorized_keys"
        authorized.write_text('command="/bin/true",no-pty,no-agent-forwarding,no-X11-forwarding ' + identity.with_suffix('.pub').read_text())
        server = subprocess.Popen([
            "/usr/sbin/sshd", "-D", "-e", "-f", "/dev/null", "-h", str(host),
            "-p", str(port), "-o", "ListenAddress=127.0.0.1",
            "-o", f"AuthorizedKeysFile={authorized}", "-o", "StrictModes=no",
            "-o", "PermitRootLogin=yes", "-o", "PasswordAuthentication=no",
            "-o", "AuthenticationMethods=publickey", "-o", "UsePAM=no",
            "-o", f"PidFile={root}/sshd.pid",
        ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                if server.poll() is not None:
                    raise RuntimeError("scratch-local sshd failed to start")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                        break
                except OSError:
                    time.sleep(0.05)
            else:
                raise RuntimeError("scratch-local sshd did not become ready")
            env = dict(os.environ, TER363_SCRATCH_SSH="/usr/bin/ssh",
                       TER363_SCRATCH_KEY=str(identity), TER363_SCRATCH_PIN=str(pin),
                       TER363_SCRATCH_WRONG_PIN=str(wrong_pin),
                       TER363_SCRATCH_PORT=str(port), RUSTC_BOOTSTRAP="1")
            return subprocess.run([
                args.binary, "--ignored", "--exact",
                "jobs::live_docker_through_daemon_tunnel_and_release_relay",
                "-Z", "unstable-options", "--format", "json", "--report-time",
            ], env=env, timeout=300).returncode
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


if __name__ == "__main__":
    raise SystemExit(main())
