"""Tests for scripts/release.py, the local release build (TER-613).

The tools of the release (docker, crane, cosign, gh, cargo) are fakes on
PATH that log their argv and the secrets in their env, and a local HTTP
server stands in for GET /user of the GitHub API. The image is a
synthetic OCI layout with the host agent and the host helper. The tests
check the order of the steps, that a red gate or a failed check publishes
nothing, and where each secret goes. One test runs the real cosign with a
test key when cosign is on PATH (it skips otherwise).

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import base64
import gzip
import hashlib
import http.server
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import typing
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
SCRIPT = HERE / "release.py"
sys.path.insert(0, str(HERE))
import release

# release-github is one classic token for GHCR and gh; release-cosign has the
# PEM base64-encoded in one line.
TOKEN = "gh-secret-token-1"
PEM = ("-----BEGIN ENCRYPTED SIGSTORE PRIVATE KEY-----\ncosign-secret-key-body-3\n"
       "-----END ENCRYPTED SIGSTORE PRIVATE KEY-----\n")
SECRETS = {
    "GHCR_TOKEN": TOKEN,
    "GH_TOKEN": TOKEN,
    "COSIGN_PRIVATE_KEY": base64.b64encode(PEM.encode()).decode(),
    "COSIGN_PASSWORD": "cosign-secret-pass-4",
}
LEAKS = (TOKEN, SECRETS["COSIGN_PRIVATE_KEY"], "cosign-secret-key-body-3", "cosign-secret-pass-4")


class FakeGitHub(http.server.BaseHTTPRequestHandler):
    """GET /user: the scope and expiry headers of a classic token."""

    scopes = "repo, write:packages"
    seen: typing.ClassVar[list[str]] = []

    def do_GET(self):
        FakeGitHub.seen.append(self.headers.get("Authorization", ""))
        self.send_response(200 if self.path == "/user" else 404)
        self.send_header("X-OAuth-Scopes", FakeGitHub.scopes)
        self.send_header("github-authentication-token-expiration", "2027-02-08 00:00:00 UTC")
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"login": "wiltaylor"}')

    def log_message(self, *args):
        pass

# One fake for every tool: argv[0]'s name says which. It logs a JSON line.
# Its settings are in fake.json next to it: release.py passes no FAKE_* env.
FAKE = r'''#!PYTHON
import json, os, shutil, sys, stat
from pathlib import Path
tool = Path(sys.argv[0]).name
args = sys.argv[1:]
cfg = json.loads((Path(sys.argv[0]).parent / "fake.json").read_text())
log = cfg["FAKE_LOG"]
SECRETS = ("GHCR_TOKEN", "GH_TOKEN", "GITHUB_TOKEN", "COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD")
entry = {"tool": tool, "argv": args, "secrets": {k: os.environ[k] for k in SECRETS if k in os.environ},
         "uid": os.geteuid(), "docker_config": os.environ.get("DOCKER_CONFIG"),
         "docker_host": os.environ.get("DOCKER_HOST")}
dc = os.environ.get("DOCKER_CONFIG")
if dc and Path(dc, "config.json").exists():
    entry["config_mode"] = oct(stat.S_IMODE(os.stat(Path(dc, "config.json")).st_mode))
    entry["config_auth"] = json.loads(Path(dc, "config.json").read_text())["auths"]["ghcr.io"]["auth"]
with open(log, "a") as f:
    f.write(json.dumps(entry) + "\n")
def opt(name):
    return args[args.index(name) + 1]
if tool == "gate":
    sys.exit(int(cfg.get("FAKE_GATE_RC", "0")))
def layout_digest():
    return json.loads(Path(cfg["FAKE_LAYOUT"], "index.json").read_text())["manifests"][0]["digest"]
if tool == "docker":
    Path(opt("--metadata-file")).write_text(json.dumps(
        {"containerimage.digest": cfg.get("FAKE_BUILD_DIGEST") or layout_digest()}))
    dest = [p.split("=", 1)[1] for p in opt("--output").split(",") if p.startswith("dest=")][0]
    import tarfile
    with tarfile.open(dest, "w") as tar:
        for f in sorted(Path(cfg["FAKE_LAYOUT"]).rglob("*")):
            tar.add(f, arcname=f.relative_to(cfg["FAKE_LAYOUT"]).as_posix(), recursive=False)
elif tool == "crane" and args[0] == "push":
    repo = args[2].rsplit(":", 1)[0]
    print(f"{repo}@{cfg.get('FAKE_PUSH_DIGEST') or layout_digest()}")
elif tool == "cosign":
    if args[0] == "sign-blob":
        Path(opt("--bundle")).write_text("{}")
    if args[0] in ("verify", "verify-blob"):
        sys.exit(int(cfg.get("FAKE_VERIFY_RC", "0")))
elif tool == "gh":
    if args[:2] == ["release", "view"]:
        sys.exit(0 if cfg.get("FAKE_RELEASE_EXISTS") else 1)
sys.exit(0)
'''


def tar_gz(entries) -> bytes:
    """entries: (name, bytes | None, linkname | None, mode)."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        for name, data, link, mode in entries:
            info = tarfile.TarInfo(name)
            info.mode = mode
            if link is not None:
                info.type = tarfile.SYMTYPE
                info.linkname = link
                tar.addfile(info)
            elif data is None:
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            else:
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
    return gzip.compress(buf.getvalue())


def put_blob(layout: Path, data: bytes) -> dict:
    d = hashlib.sha256(data).hexdigest()
    p = layout / "blobs" / "sha256"
    p.mkdir(parents=True, exist_ok=True)
    (p / d).write_bytes(data)
    return {"digest": f"sha256:{d}", "size": len(data)}


def host_binary(name: str, version: str) -> bytes:
    return f'#!/bin/sh\necho "{name} {version}+0123abc"\n'.encode()


def make_layout(layout: Path, version: str, *, bad_sha=None, helper=True, report=None) -> None:
    """An OCI layout like the release image: an index with the amd64 image and an attestation."""
    files = [("opt", None, None, 0o755), ("opt/terrarium", None, None, 0o755),
             ("opt/terrarium/bundled", None, None, 0o755), ("opt/terrarium/bundled/dist", None, None, 0o755)]
    for b in release.BINARIES:
        if b == "terrarium-host-helper" and not helper:
            continue
        data = host_binary(b, report or version)
        h = hashlib.sha256(data).hexdigest()
        if bad_sha == b:
            h = "0" * 64
        files.append((f"opt/terrarium/bundled/dist/{b}-x86_64-linux", data, None, 0o755))
        files.append((f"opt/terrarium/bundled/dist/{b}-x86_64-linux.sha256",
                      f"{h}  {b}-x86_64-linux\n".encode(), None, 0o644))
    # A later layer: an old file that a whiteout removes, and the dist symlink.
    first = tar_gz(files + [("opt/terrarium/old", b"x", None, 0o644)])
    second = tar_gz([("opt/terrarium/.wh.old", b"", None, 0o644),
                     ("opt/terrarium/dist", None, "/opt/terrarium/bundled/dist", 0o777)])
    layers = [put_blob(layout, first), put_blob(layout, second)]
    config = put_blob(layout, json.dumps({"os": "linux", "architecture": "amd64"}).encode())
    manifest = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", **config},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", **lay} for lay in layers]}
    m = put_blob(layout, json.dumps(manifest).encode())
    att = put_blob(layout, json.dumps({"schemaVersion": 2, "config": config, "layers": []}).encode())
    index = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
        {"mediaType": "application/vnd.oci.image.manifest.v1+json", **att,
         "platform": {"os": "unknown", "architecture": "unknown"}},
        {"mediaType": "application/vnd.oci.image.manifest.v1+json", **m,
         "platform": {"os": "linux", "architecture": "amd64"}},
    ]}
    i = put_blob(layout, json.dumps(index).encode())
    (layout / "index.json").write_text(json.dumps({"schemaVersion": 2, "manifests": [
        {"mediaType": "application/vnd.oci.image.index.v1+json", **i}]}))
    (layout / "oci-layout").write_text('{"imageLayoutVersion": "1.0.0"}')


def git(cwd, *args):
    subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
                   cwd=cwd, check=True, capture_output=True, text=True)


class Release(unittest.TestCase):
    VERSION = "0.5.1"

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        # Readable by uid 1001: as root the gate runs as 1001 (setpriv).
        self.tmp.chmod(0o755)
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.repo = self.tmp / "repo"
        (self.repo / ".github").mkdir(parents=True)
        (self.repo / "docker/release").mkdir(parents=True)
        shutil.copy(REPO / release.STAMP, self.repo / release.STAMP)
        (self.repo / "Cargo.toml").write_text('[workspace.package]\nversion = "0.0.0"\n\n[workspace]\n')
        (self.repo / release.KEY_PATH).write_text("test public key\n")
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", "chore: init")
        git(self.repo, "tag", "v0.5.0")
        git(self.repo, "commit", "-q", "--allow-empty", "-m", "fix: a", "-m", "release: true")
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        fake = FAKE.replace("PYTHON", sys.executable)
        for tool in ("docker", "crane", "cosign", "gh", "cargo", "gate"):
            p = self.bin / tool
            p.write_text(fake)
            p.chmod(0o755)
        self.logdir = self.tmp / "log"
        self.logdir.mkdir()
        self.logdir.chmod(0o777)
        self.log = self.logdir / "calls.jsonl"
        self.log.touch()
        self.log.chmod(0o666)
        self.layout = self.tmp / "layout"
        make_layout(self.layout, self.VERSION)
        self.out = self.tmp / "out"
        FakeGitHub.scopes = "repo, write:packages"
        FakeGitHub.seen = []
        self.api = http.server.HTTPServer(("127.0.0.1", 0), FakeGitHub)
        threading.Thread(target=self.api.serve_forever, daemon=True).start()
        self.addCleanup(self.api.server_close)
        self.addCleanup(self.api.shutdown)

    def run_release(self, mode, *extra, secrets=True, env=None, fake=None):
        cfg = {"FAKE_LOG": str(self.log), "FAKE_LAYOUT": str(self.layout)}
        cfg.update(fake or {})
        (self.bin / "fake.json").write_text(json.dumps(cfg))
        e = {"PATH": f"{self.bin}:{os.environ['PATH']}", "HOME": str(self.tmp),
             "TERRARIUM_RELEASE_TEST": "1", "TERRARIUM_DOCKER_HOST": "unix:///run/job/docker.sock"}
        if secrets:
            e.update(SECRETS)
        e.update(env or {})
        proc = subprocess.run(
            [sys.executable, "-I", str(SCRIPT), mode, "--out", str(self.out),
             "--gate", str(self.bin / "gate"),
             "--github-api", f"http://127.0.0.1:{self.api.server_address[1]}", *extra],
            cwd=self.repo, env=e, capture_output=True, text=True, check=False,
        )
        self.proc = proc
        return proc

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def tools(self):
        return [(c["tool"], c["argv"][0] if c["argv"] else "") for c in self.calls()]

    def summary(self):
        return json.loads((self.out / "summary.json").read_text())

    def assert_no_secret_leaks(self):
        """No secret on any argv, in the output, or in a file of <out>."""
        for c in self.calls():
            for v in LEAKS:
                self.assertNotIn(v, " ".join(c["argv"]), c["tool"])
        for v in LEAKS:
            self.assertNotIn(v, self.proc.stdout + self.proc.stderr)
            for f in self.out.rglob("*"):
                if f.is_file():
                    self.assertNotIn(v.encode(), f.read_bytes(), str(f))

    def digest(self):
        return json.loads((self.layout / "index.json").read_text())["manifests"][0]["digest"]

    def test_publish_runs_the_steps_in_order_and_signs_everything(self):
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        digest = self.digest()
        image = release.DEFAULT_IMAGE
        self.assertEqual(
            self.tools(),
            [("gh", "release"), ("gate", ""), ("cargo", "update"), ("docker", "buildx"),
             ("crane", "push"), ("crane", "tag"), ("cosign", "sign"), ("cosign", "verify"),
             ("cosign", "sign-blob"), ("cosign", "sign-blob"), ("cosign", "sign-blob"),
             ("cosign", "verify-blob"), ("cosign", "verify-blob"), ("cosign", "verify-blob"),
             ("gh", "release")],
        )
        calls = self.calls()
        build = calls[3]["argv"]
        self.assertEqual(build[:2], ["buildx", "build"])
        self.assertEqual(build[-1], ".")
        # The build exports to this side and pushes nothing.
        self.assertIn(f"type=oci,dest={self.out / 'image.tar'}", build)
        self.assertNotIn("--push", build)
        for want in ("--target", "runtime", "--platform", "linux/amd64", "--provenance=mode=max", "--sbom=true",
                     f"TERRARIUM_VERSION={self.VERSION}", f"org.opencontainers.image.version={self.VERSION}",
                     "org.opencontainers.image.source=https://github.com/wiltaylor/terrarium"):
            self.assertIn(want, build)
        commit = self.summary()["commit"]
        self.assertIn(f"TERRARIUM_GIT_REF={commit}", build)
        self.assertIn(f"org.opencontainers.image.revision={commit}", build)
        # The daemon's relay socket becomes DOCKER_HOST of docker only.
        self.assertEqual(calls[3]["docker_host"], "unix:///run/job/docker.sock")
        for c in calls:
            if c["tool"] != "docker":
                self.assertIsNone(c["docker_host"], c["tool"])
        self.assertEqual(calls[4]["argv"], ["push", str(self.out / "image"), f"{image}:{self.VERSION}"])
        self.assertEqual(calls[5]["argv"], ["tag", f"{image}@{digest}", "latest"])
        self.assertEqual(calls[6]["argv"][-1], f"{image}@{digest}")
        self.assertIn("env://COSIGN_PRIVATE_KEY", calls[6]["argv"])
        self.assertEqual(calls[7]["argv"][:3], ["verify", "--key", str(self.repo / release.KEY_PATH)])
        signed = [Path(c["argv"][-1]).name for c in calls if c["argv"][:1] == ["sign-blob"]]
        self.assertEqual(signed, [f"terrarium-host-{self.VERSION}-linux-x86_64",
                                  f"terrarium-host-helper-{self.VERSION}-linux-x86_64", "SHA256SUMS"])
        create = calls[-1]["argv"]
        self.assertEqual(create[:9], ["release", "create", f"v{self.VERSION}", "--repo",
                                      "wiltaylor/terrarium", "--target", commit, "--title", f"v{self.VERSION}"])
        self.assertIn("--latest", create)
        self.assertNotIn("--prerelease", create)
        names = sorted(Path(a).name for a in create if a.startswith(str(self.out)) and "dist" in a)
        self.assertEqual(len(names), 6, names)
        sums = (self.out / "dist/SHA256SUMS").read_text().splitlines()
        self.assertEqual(len(sums), 2)
        for line in sums:
            h, name = line.split("  ")
            self.assertEqual(hashlib.sha256((self.out / "dist" / name).read_bytes()).hexdigest(), h)
        notes = (self.out / "notes.md").read_text()
        self.assertIn(f"cosign verify --key cosign.pub {image}@{digest}", notes)
        s = self.summary()
        self.assertTrue(s["done"])
        self.assertEqual(s["image"]["digest"], digest)
        self.assertEqual(s["image"]["tags"], [f"{image}:{self.VERSION}", f"{image}:latest"])
        self.assertTrue(s["gate"]["passed"])
        self.assertEqual(s["release"]["tag"], f"v{self.VERSION}")
        self.assertEqual(s["token"], {"scopes": ["repo", "write:packages"], "expires": "2027-02-08 00:00:00 UTC"})
        self.assertIn("GitHub token: scopes repo,write:packages, expires 2027-02-08 00:00:00 UTC", proc.stdout)
        self.assertIn(f'version = "{self.VERSION}"', (self.repo / "Cargo.toml").read_text())

    def test_each_secret_goes_only_where_it_is_needed(self):
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        key = {"COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD"}
        for c in self.calls():
            got = set(c["secrets"])
            if c["tool"] == "gh":
                self.assertEqual(got, {"GH_TOKEN"}, c)
            elif c["tool"] == "cosign" and c["argv"][0] in ("sign", "sign-blob"):
                self.assertEqual(got, key, c)
                # cosign gets the PEM: the secret holds it base64-encoded.
                self.assertEqual(c["secrets"]["COSIGN_PRIVATE_KEY"], PEM)
            else:
                self.assertEqual(got, set(), c)
        # The registry login: a private 0600 file for crane and cosign, gone after. docker has none.
        push = next(c for c in self.calls() if c["tool"] == "crane")
        self.assertEqual(push["config_mode"], "0o600")
        self.assertEqual(base64.b64decode(push["config_auth"]).decode(), f"wiltaylor:{TOKEN}")
        self.assertFalse(Path(push["docker_config"]).exists())
        for tool in ("docker", "gate", "cargo"):
            c = next(c for c in self.calls() if c["tool"] == tool)
            self.assertIsNone(c["docker_config"], tool)
        self.assertNotEqual(next(c for c in self.calls() if c["tool"] == "gate")["uid"], 0)
        # The token goes to the GitHub API, and nowhere else.
        self.assertEqual(FakeGitHub.seen, [f"token {TOKEN}"])
        self.assert_no_secret_leaks()

    def test_a_token_without_the_scopes_stops_before_the_gate(self):
        FakeGitHub.scopes = "repo"
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("the GitHub token lacks the scopes write:packages", proc.stderr)
        self.assertEqual(self.calls(), [])
        self.assert_no_secret_leaks()

    def test_a_key_that_is_neither_pem_nor_base64_stops_it(self):
        proc = self.run_release("publish", env={"COSIGN_PRIVATE_KEY": "not a key!"})
        self.assertEqual(proc.returncode, 1)
        self.assertIn("COSIGN_PRIVATE_KEY is neither a PEM nor base64 of a PEM", proc.stderr)
        self.assertNotIn("not a key!", proc.stdout + proc.stderr)
        proc = self.run_release("publish", env={"COSIGN_PRIVATE_KEY": base64.b64encode(b"plain").decode()})
        self.assertIn("base64, but not of a PEM", proc.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_plain_pem_key_works_too(self):
        proc = self.run_release("publish", env={"COSIGN_PRIVATE_KEY": PEM})
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        sign = next(c for c in self.calls() if c["argv"][:1] == ["sign"])
        self.assertEqual(sign["secrets"]["COSIGN_PRIVATE_KEY"], PEM)

    def test_a_push_of_other_bytes_stops_before_the_signatures(self):
        proc = self.run_release("publish", fake={"FAKE_PUSH_DIGEST": "sha256:" + "00" * 32})
        self.assertEqual(proc.returncode, 1)
        self.assertIn("crane pushed", proc.stderr)
        self.assertEqual(self.tools()[-1], ("crane", "push"))

    def test_a_build_digest_that_is_not_the_layout_stops_it(self):
        proc = self.run_release("dry-run", secrets=False, fake={"FAKE_BUILD_DIGEST": "sha256:" + "11" * 32})
        self.assertEqual(proc.returncode, 1)
        self.assertIn("the OCI layout holds", proc.stderr)

    def test_a_prerelease_gets_the_pre_tag_and_flag(self):
        git(self.repo, "commit", "-q", "--allow-empty", "-m", "feat: b", "-m", "pre-release: true")
        shutil.rmtree(self.layout)
        make_layout(self.layout, "0.6.0-alpha")
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        calls = self.calls()
        self.assertEqual(next(c for c in calls if c["argv"][:1] == ["push"])["argv"][-1],
                         f"{release.DEFAULT_IMAGE}:0.6.0-alpha")
        self.assertEqual(next(c for c in calls if c["argv"][:1] == ["tag"])["argv"][-1], "pre")
        self.assertIn("--prerelease", calls[-1]["argv"])

    def test_dry_run_builds_and_extracts_but_publishes_nothing(self):
        proc = self.run_release("dry-run", secrets=False)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.tools(), [("gate", ""), ("cargo", "update"), ("docker", "buildx")])
        self.assertEqual(len((self.out / "dist/SHA256SUMS").read_text().splitlines()), 2)
        self.assertTrue((self.out / "image/index.json").is_file())
        self.assertFalse((self.out / "image.tar").exists())
        s = self.summary()
        self.assertEqual(s["mode"], "dry-run")
        self.assertFalse(s["image"]["pushed"])
        self.assertEqual(FakeGitHub.seen, [])

    def test_dry_run_can_skip_the_gate_but_publish_cannot(self):
        proc = self.run_release("dry-run", "--no-gate", secrets=False)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertNotIn(("gate", ""), self.tools())
        proc = self.run_release("publish", "--no-gate")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("--no-gate is for the dry run only", proc.stderr)

    def test_a_red_gate_publishes_nothing(self):
        proc = self.run_release("publish", fake={"FAKE_GATE_RC": "1"})
        self.assertEqual(proc.returncode, 1)
        self.assertIn("the gate is red", proc.stderr)
        self.assertEqual(self.tools(), [("gh", "release"), ("gate", "")])
        self.assertFalse(self.summary()["gate"]["passed"])
        self.assert_no_secret_leaks()

    def test_no_trailer_no_release(self):
        git(self.repo, "commit", "-q", "--allow-empty", "-m", "fix: c")
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(self.calls(), [])
        self.assertEqual(self.summary()["version"], {"do_release": "false"})

    def test_publish_needs_each_secret(self):
        proc = self.run_release("publish", secrets=False, env={"GH_TOKEN": "x"})
        self.assertEqual(proc.returncode, 1)
        self.assertIn("GHCR_TOKEN, COSIGN_PRIVATE_KEY, COSIGN_PASSWORD", proc.stderr)
        self.assertEqual(self.calls(), [])

    def test_publish_needs_the_public_key(self):
        git(self.repo, "rm", "-q", release.KEY_PATH)
        git(self.repo, "commit", "-q", "-m", "fix: d", "-m", "release: true")
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("docker/release/cosign.pub is missing", proc.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_dirty_checkout_or_an_existing_release_stops_it(self):
        (self.repo / "stray").write_text("x")
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("not clean", proc.stderr)
        (self.repo / "stray").unlink()
        proc = self.run_release("publish", fake={"FAKE_RELEASE_EXISTS": "1"})
        self.assertEqual(proc.returncode, 1)
        self.assertIn(f"the release v{self.VERSION} exists", proc.stderr)
        self.assertNotIn("gate", [t for t, _ in self.tools()])

    def test_a_hash_mismatch_in_the_image_signs_nothing(self):
        shutil.rmtree(self.layout)
        make_layout(self.layout, self.VERSION, bad_sha="terrarium-host-helper")
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("the image's terrarium-host-helper is", proc.stderr)
        self.assertNotIn("cosign", [t for t, _ in self.tools()])

    def test_an_image_without_the_helper_fails(self):
        shutil.rmtree(self.layout)
        make_layout(self.layout, self.VERSION, helper=False)
        proc = self.run_release("dry-run", secrets=False)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("the image has no file /opt/terrarium/dist/terrarium-host-helper-x86_64-linux", proc.stderr)

    def test_a_binary_of_another_version_fails(self):
        shutil.rmtree(self.layout)
        make_layout(self.layout, self.VERSION, report="0.4.0")
        proc = self.run_release("publish")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("--version says", proc.stderr)
        self.assertNotIn("cosign", [t for t, _ in self.tools()])

    def test_a_signature_that_the_checkout_key_refuses_makes_no_release(self):
        proc = self.run_release("publish", fake={"FAKE_VERIFY_RC": "1"})
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self.tools()[-1], ("cosign", "verify"))
        self.assertNotIn("create", [c["argv"][1] for c in self.calls() if c["tool"] == "gh"])

    def test_the_test_signing_config_is_refused_outside_the_tests(self):
        proc = self.run_release("publish", "--signing-config", "/x.json", env={"TERRARIUM_RELEASE_TEST": "0"})
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("--signing-config is for the tests only", proc.stderr)
        proc = self.run_release("publish", env={"TERRARIUM_RELEASE_TEST": "0"})
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("--github-api is for the tests only", proc.stderr)
        self.assertEqual(self.calls(), [])


class ExtractLayout(unittest.TestCase):
    def test_a_path_out_of_the_layout_or_a_link_is_refused(self):
        for name, kind in (("../evil", tarfile.REGTYPE), ("/abs", tarfile.REGTYPE), ("blobs/l", tarfile.SYMTYPE)):
            with tempfile.TemporaryDirectory() as tmp:
                archive = Path(tmp, "image.tar")
                with tarfile.open(archive, "w") as tar:
                    info = tarfile.TarInfo(name)
                    info.type = kind
                    info.linkname = "/etc/passwd" if kind == tarfile.SYMTYPE else ""
                    tar.addfile(info, io.BytesIO(b"") if kind == tarfile.REGTYPE else None)
                with self.assertRaises(release.ReleaseError, msg=name):
                    release.extract_layout(archive, Path(tmp, "out"))
                self.assertFalse(Path(tmp, "evil").exists())


class ImageFiles(unittest.TestCase):
    def test_symlinks_whiteouts_and_platforms(self):
        with tempfile.TemporaryDirectory() as tmp:
            layout = Path(tmp)
            make_layout(layout, "1.2.3")
            got = release.image_files(layout, ["/opt/terrarium/dist/terrarium-host-x86_64-linux"])
            self.assertEqual(got["/opt/terrarium/dist/terrarium-host-x86_64-linux"],
                             host_binary("terrarium-host", "1.2.3"))
            with self.assertRaisesRegex(release.ReleaseError, "no file /opt/terrarium/old"):
                release.image_files(layout, ["/opt/terrarium/old"])
            with self.assertRaisesRegex(release.ReleaseError, "no linux/arm64 manifest"):
                release.image_layers(layout, "linux/arm64")

    def test_a_blob_that_does_not_match_its_digest_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            layout = Path(tmp)
            make_layout(layout, "1.2.3")
            outer = json.loads((layout / "index.json").read_text())["manifests"][0]["digest"]
            blob = layout / "blobs/sha256" / outer.split(":")[1]
            blob.write_text(blob.read_text().replace("amd64", "arm64"))
            with self.assertRaisesRegex(release.ReleaseError, "does not match its digest"):
                release.image_layers(layout)


class Pins(unittest.TestCase):
    """The cosign of the release container signs what the updater's cosign verifies."""

    def arg(self, text, name):
        import re
        m = re.search(rf"^ARG {name}=(\S+)$", text, re.MULTILINE)
        self.assertIsNotNone(m, name)
        return m.group(1)

    def test_one_cosign_in_both_images(self):
        runtime = (REPO / "docker/Dockerfile").read_text()
        build = (REPO / "docker/release.Dockerfile").read_text()
        self.assertEqual(self.arg(runtime, "COSIGN_VERSION"), self.arg(build, "COSIGN_VERSION"))
        self.assertEqual(self.arg(runtime, "COSIGN_SHA256_AMD64"), self.arg(build, "COSIGN_SHA256"))
        self.assertIn("COPY --from=cosign /out/cosign /usr/local/bin/cosign", runtime)
        self.assertIn("COPY docker/release/ /opt/terrarium/release/", runtime)
        self.assertIn("!docker/release/**", (REPO / "docker/Dockerfile.dockerignore").read_text())

    def test_the_base_images_of_the_release_container_are_pinned_by_digest(self):
        build = (REPO / "docker/release.Dockerfile").read_text()
        for name in ("RUST_IMAGE", "NODE_IMAGE", "DOCKER_CLI_IMAGE"):
            self.assertIn("@sha256:", self.arg(build, name), name)
        self.assertIn("USER 1001", build)

    def test_the_release_public_key_is_in_the_repo(self):
        # The updater's default check and release.py need it (docker/release/README.md).
        pem = (REPO / release.KEY_PATH).read_text()
        self.assertTrue(pem.startswith("-----BEGIN PUBLIC KEY-----\n"), pem[:40])
        self.assertTrue(pem.endswith("-----END PUBLIC KEY-----\n"))
        self.assertNotIn("PRIVATE", pem)

    def test_the_compose_default_is_the_cosign_check(self):
        compose = (REPO / "compose.yaml").read_text()
        self.assertIn("TERRARIUM_UPDATE_VERIFY: ${TERRARIUM_UPDATE_VERIFY:-cosign}", compose)
        self.assertIn("TERRARIUM_UPDATE_COSIGN_KEY: ${TERRARIUM_UPDATE_COSIGN_KEY:-/opt/terrarium/release/cosign.pub}", compose)


@unittest.skipUnless(shutil.which("cosign"), "cosign is not on PATH")
class RealCosign(unittest.TestCase):
    """sign_blobs with the real cosign, a test key, and a signing config with no log."""

    def test_sign_and_verify_blobs_with_the_key_from_the_env(self):
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            env = {"PATH": os.environ["PATH"], "HOME": tmp, "COSIGN_PASSWORD": "test-only"}
            subprocess.run(["cosign", "generate-key-pair"], cwd=t, env=env, check=True, capture_output=True)
            (t / "docker/release").mkdir(parents=True)
            shutil.move(t / "cosign.pub", t / release.KEY_PATH)
            sc = t / "signing-config.json"
            sc.write_text(json.dumps({
                "mediaType": "application/vnd.dev.sigstore.signingconfig.v0.2+json",
                "caUrls": [], "oidcUrls": [], "rekorTlogUrls": [], "tsaUrls": [],
                "rekorTlogConfig": {"selector": "ANY"}, "tsaConfig": {"selector": "ANY"}}))
            blob = t / "SHA256SUMS"
            blob.write_text("abc  x\n")
            private = (t / "cosign.key").read_text()
            (t / "cosign.key").unlink()
            ctx = release.Ctx(
                repo_dir=t, out=t / "out", publish=True, image="x", gh_repo="o/r", gate="true",
                run_gate=False, signing_config=str(sc),
                environ={"PATH": os.environ["PATH"], "HOME": tmp, "COSIGN_PRIVATE_KEY": private,
                         "COSIGN_PASSWORD": "test-only"},
            )
            bundles = release.sign_blobs(ctx, [blob])
            self.assertTrue(bundles[0].is_file())
            # The secret's form: the PEM base64-encoded in one line.
            ctx.environ["COSIGN_PRIVATE_KEY"] = base64.b64encode(private.encode()).decode()
            bundles = release.sign_blobs(ctx, [blob])
            self.assertTrue(bundles[0].is_file())
            self.assertNotIn("PRIVATE", bundles[0].read_text())
            # Another public key refuses the signature.
            subprocess.run(["cosign", "generate-key-pair"], cwd=t, env=env, check=True, capture_output=True)
            shutil.move(t / "cosign.pub", t / release.KEY_PATH)
            with self.assertRaises(release.ReleaseError):
                release.sign_blobs(ctx, [blob])


if __name__ == "__main__":
    unittest.main()
