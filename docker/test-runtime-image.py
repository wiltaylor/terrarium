"""Real runtime Dockerfile acceptance with precompiled binary build contexts.

Run on the disposable Docker target with TER261_BIN_DIR. The directory must
also contain terrarium-host. This checks packaging, not release compilation.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[1]


def docker(*args, check=True):
    result = subprocess.run(["docker", *args], text=True, capture_output=True)
    if check and result.returncode:
        raise AssertionError(f"docker {args}: {result.stdout}\n{result.stderr}")
    return result


class RuntimeImage(unittest.TestCase):
    def test_real_dockerfile_runtime(self):
        image = "ter-261-real-runtime"
        name = "ter-261-real-runtime-check"
        binaries = Path(os.environ["TER261_BIN_DIR"])
        self.assertNotEqual(docker("image", "inspect", image, check=False).returncode, 0)
        self.assertNotEqual(docker("inspect", name, check=False).returncode, 0)
        with tempfile.TemporaryDirectory(prefix="ter-261-runtime-") as temp:
            root = Path(temp)
            rust = root / "rust"
            host = root / "host"
            (rust / "out").mkdir(parents=True)
            (host / "dist").mkdir(parents=True)
            for binary in ("terrariumd", "terrarium", "terrarium-runner", "terrarium-updater"):
                shutil.copy2(binaries / binary, rust / "out" / binary)
            asset = "terrarium-host-x86_64-linux"
            shutil.copy2(binaries / "terrarium-host", host / "dist" / asset)
            digest = hashlib.sha256((host / "dist" / asset).read_bytes()).hexdigest()
            (host / "dist" / (asset + ".sha256")).write_text(f"{digest}  {asset}\n")
            try:
                docker("buildx", "build", "--load", "-f", str(REPO / "docker/Dockerfile"),
                       "--build-context", f"rust={rust}", "--build-context", f"host-build={host}",
                       "-t", image, str(REPO))
                history = json.loads(docker("image", "inspect", image).stdout)[0]
                self.assertEqual(history["Config"]["Labels"]["io.terrarium.hot-swap-layout"], "1")
                layers = docker("history", "--no-trunc", "--format", "{{.Size}} {{.CreatedBy}}", image).stdout.splitlines()
                last_filesystem = next(line for line in layers if not line.startswith("0B "))
                self.assertIn("/payload/ /opt/terrarium/bundled/", last_filesystem)
                # Release CI extracts from an unstarted container.
                docker("create", "--name", name, image)
                extracted = root / "extracted"
                extracted.mkdir()
                docker("cp", name + ":/opt/terrarium/dist/.", str(extracted))
                self.assertEqual(hashlib.sha256((extracted / asset).read_bytes()).hexdigest(), digest)
                docker("rm", name)
                docker("run", "-d", "--name", name,
                       "-e", "TERRARIUM_HARNESS_INSTALL=0", "-e", "TERRARIUM_NO_HARNESS_INSTALL=true",
                       "-e", "TERRARIUM_HOST_LISTEN=", image,
                       "--daemon-service", "terrariumd", "--sandbox", "none")
                deadline = time.monotonic() + 60
                while docker("exec", name, "terrarium", "daemon", "status", check=False).returncode:
                    if time.monotonic() > deadline:
                        self.fail(docker("logs", name).stdout + docker("logs", name).stderr)
                    time.sleep(.2)
                selected = docker("exec", name, "readlink", "/state/releases/current").stdout.strip()
                self.assertEqual(len(selected), 64)
                docker("exec", name, "test", "-x", "/state/releases/current/bin/terrariumd")
                self.check_legacy_boundary(binaries)
            finally:
                docker("rm", "-f", name, check=False)
                docker("rmi", image, check=False)

    def check_legacy_boundary(self, binaries):
        legacy = "ghcr.io/wiltaylor/terrarium:0.17.0-alpha"
        name = "ter-261-legacy-check"
        volume = "ter-261-legacy-check-state"
        self.assertNotEqual(docker("inspect", name, check=False).returncode, 0)
        self.assertNotEqual(docker("volume", "inspect", volume, check=False).returncode, 0)
        docker("pull", legacy)
        spec = json.loads(docker("image", "inspect", legacy).stdout)[0]
        self.assertEqual(spec["Config"]["Cmd"], ["terrariumd"])
        self.assertNotIn("io.terrarium.hot-swap-layout", spec["Config"].get("Labels") or {})
        try:
            docker("volume", "create", volume)
            docker("run", "-d", "--name", name, "--mount", f"type=volume,src={volume},dst=/state",
                   "-e", "TERRARIUM_SANDBOX=none", "-e", "TERRARIUM_HARNESS_INSTALL=0",
                   "-e", "TERRARIUM_NO_HARNESS_INSTALL=true", "-e", "TERRARIUM_HOST_LISTEN=",
                   "--health-cmd", "terrarium daemon status", "--health-interval", "1s",
                   "--health-start-period", "10s", legacy)
            deadline = time.monotonic() + 60
            while docker("exec", name, "terrarium", "daemon", "status", check=False).returncode:
                if time.monotonic() > deadline:
                    self.fail(docker("logs", name).stdout + docker("logs", name).stderr)
                time.sleep(.2)
            result = subprocess.run([str(binaries / "hot-swap-docker-test"), "--ignored",
                                     "--nocapture", "legacy_image_swap_and_rollback"],
                                    env={**os.environ, "TER261_LEGACY_CONTAINER": name},
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        finally:
            docker("rm", "-f", name, check=False)
            docker("volume", "rm", volume, check=False)
