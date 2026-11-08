#!/usr/bin/env python3
"""Disposable acceptance fixtures. Run on terrarium-docker-scratch only.

TER261_BIN_DIR holds built terrarium{d,-runner,} and hot-swap-docker-test.
The latter is cargo test -p terrarium-updater --test hot_swap_docker --no-run.
Run with pytest --junitxml=<file>. No live daemon or existing image is changed.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[1]


def docker(*args, check=True):
    return subprocess.run(["docker", *args], check=check, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)


class ContainerHotSwap(unittest.TestCase):
    def test_real_engine_transaction(self):
        binaries = Path(os.environ["TER261_BIN_DIR"])
        self.assertTrue((binaries / "hot-swap-docker-test").is_file())
        name = "ter-261-acceptance"
        volume = "ter-261-acceptance-state"
        # Refuse collisions, rather than touching a previous run's resources.
        self.assertNotEqual(docker("inspect", name, check=False).returncode, 0)
        self.assertNotEqual(docker("volume", "inspect", volume, check=False).returncode, 0)
        for tag in ("base", "old", "good", "bad", "changed-base"):
            self.assertNotEqual(docker("image", "inspect", "ter-261-" + tag, check=False).returncode, 0)
        created_images = []
        with tempfile.TemporaryDirectory(prefix="ter-261-context-") as temp:
            context = Path(temp)
            for src in ("entrypoint.sh", "hot-swap.py"):
                shutil.copy2(REPO / "docker" / src, context / src)
            (context / "Dockerfile.base").write_text('''FROM node:22.23.4-trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 git tini util-linux procps && rm -rf /var/lib/apt/lists/*
RUN userdel node && useradd -u 1000 -m terrarium && mkdir -p /state /work /config/terrarium /opt/terrarium/tools /run/terrarium /auth/claude /auth/codex /auth/copilot /auth/pi && chown -R terrarium:terrarium /state /work /config /opt/terrarium /run/terrarium /auth
COPY entrypoint.sh /usr/local/bin/terrarium-entrypoint
COPY hot-swap.py /usr/local/lib/terrarium-hot-swap.py
RUN chmod +x /usr/local/bin/terrarium-entrypoint && ln -s /state/releases/current/bin/terrariumd /usr/local/bin/terrariumd && ln -s /state/releases/current/bin/terrarium /usr/local/bin/terrarium && ln -s /state/releases/current/bin/terrarium-runner /usr/local/bin/terrarium-runner
ENV TERRARIUM_STATE_DIR=/state TERRARIUM_CONFIG_REPO=/config/terrarium TERRARIUM_CONFIG_DIR=/config/terrarium TERRARIUM_API_SOCKET=/run/terrarium/api.sock TERRARIUM_RUNTIME_DIR=/run/terrarium TERRARIUM_RUNNER=/usr/local/bin/terrarium-runner TERRARIUM_RUNNER_MODE=process TERRARIUM_HARNESS_INSTALL=0 TERRARIUM_NO_HARNESS_INSTALL=true TERRARIUM_HOST_LISTEN="" TERRARIUM_DEFAULTS_DIR=/state/releases/current/defaults
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/terrarium-entrypoint"]
CMD ["--daemon-service", "terrariumd", "--sandbox", "none"]
USER terrarium
HEALTHCHECK --interval=1s --timeout=2s --start-period=10s --retries=3 CMD terrarium daemon status
LABEL io.terrarium.hot-swap-layout="1" io.terrarium.runner-compat-epoch="1"
''')
            try:
                result = docker("build", "-f", str(context / "Dockerfile.base"), "-t", "ter-261-base", str(context), check=False)
                self.assertEqual(result.returncode, 0, result.stderr)
                created_images.append("ter-261-base")
                payload = context / "payload"
                (payload / "bin").mkdir(parents=True)
                for binary in ("terrariumd", "terrarium", "terrarium-runner"):
                    shutil.copy2(binaries / binary, payload / "bin" / binary)
                shutil.copytree(REPO / "defaults", payload / "defaults")
                for tag, key, changed in [("old", "a", False), ("good", "b", False), ("changed-base", "d", True), ("bad", "c", False)]:
                    (payload / "release-id").write_text(key * 64)
                    if tag == "bad":
                        (payload / "bin/terrariumd").write_text('''#!/usr/bin/env python3
import sqlite3
with sqlite3.connect('/state/terrarium.db') as c:
    c.execute('CREATE TABLE IF NOT EXISTS failed_migration(value TEXT)')
raise SystemExit(42)
''')
                        (payload / "bin/terrariumd").chmod(0o755)
                    dockerfile = "FROM ter-261-base\n"
                    if changed:
                        dockerfile += "RUN touch /home/terrarium/different-runtime-base\n"
                    dockerfile += "COPY payload/ /opt/terrarium/bundled/\n"
                    (context / "Dockerfile").write_text(dockerfile)
                    image = "ter-261-" + tag
                    result = docker("build", "-t", image, str(context), check=False)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    created_images.append(image)
                docker("volume", "create", volume)
                docker("run", "-d", "--name", name, "--mount", f"type=volume,src={volume},dst=/state", "ter-261-old")
                deadline = time.monotonic() + 60
                while docker("exec", name, "terrarium", "daemon", "status", check=False).returncode:
                    if time.monotonic() > deadline:
                        self.fail(docker("logs", name).stdout)
                    time.sleep(.2)
                # A documented compose-run-style backup from the bundled OLD
                # image must not touch an active different release or probation.
                docker("run", "--rm", "--volumes-from", name, "--entrypoint", "python3",
                       "ter-261-good", "/usr/local/lib/terrarium-hot-swap.py", "stage", "{}")
                switch = "from pathlib import Path; r=Path('/state/releases'); (r/'current').unlink(); (r/'current').symlink_to('b'*64); (r/'probation').touch()"
                docker("exec", name, "python3", "-c", switch)
                docker("run", "--rm", "--volumes-from", name, "ter-261-old", "terrariumd", "backup", "list", "--json")
                check = "from pathlib import Path; r=Path('/state/releases'); assert (r/'probation').exists(); assert (r/'current').resolve().name=='b'*64"
                docker("exec", name, "python3", "-c", check)
                # Even an accidental second default service command cannot
                # acquire the state-volume lock while the live service owns it.
                second = docker("run", "--rm", "--volumes-from", name, "ter-261-old", check=False)
                self.assertNotEqual(second.returncode, 0)
                docker("exec", name, "python3", "-c", check)
                docker("exec", name, "python3", "-c", "from pathlib import Path; r=Path('/state/releases'); (r/'current').unlink(); (r/'current').symlink_to('a'*64); (r/'probation').unlink()")
                docker("exec", "-d", name, "sh", "-c", "echo $$ > /run/terrarium/survivor.pid; exec sleep 600")
                result = subprocess.run([str(binaries / "hot-swap-docker-test"), "--ignored", "--nocapture"],
                                        env={**os.environ, "TER261_CONTAINER": name},
                                        text=True, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr + docker("logs", name).stdout)
            finally:
                # Exact names created by this test only. Never prune or force-rmi.
                docker("rm", "-f", name, check=False)
                docker("volume", "rm", volume, check=False)
                for image in reversed(created_images):
                    docker("rmi", image, check=False)


if __name__ == "__main__":
    unittest.main()
