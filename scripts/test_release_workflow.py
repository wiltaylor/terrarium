"""Tests for the release part of .github/workflows/ci.yml (docs/release.md).

They read the workflow as text (and as YAML when PyYAML is there) and check
the rules of the release: the image goes to GHCR with an attestation, the
host agent and host helper assets come out of the image with SHA256SUMS
(a dry run of the extract step with a fake docker), each action is
pinned to a full commit SHA, and each job has the least permissions.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TEXT = (REPO / ".github/workflows/ci.yml").read_text()
DOCKERFILE = (REPO / "docker/Dockerfile").read_text()


def run_script(marker: str) -> str:
    """The run script of the step whose first line is `marker`."""
    lines = TEXT.splitlines()
    start = lines.index(marker)
    run = lines.index("        run: |", start)
    body = []
    for line in lines[run + 1 :]:
        if line.strip() and not line.startswith("          "):
            break
        body.append(line)
    return textwrap.dedent("\n".join(body))


def calc_script() -> str:
    """The run script of the `calc` step of the version job."""
    return run_script("      - id: calc")


EXTRACT_STEP = "      - name: Take the host agent and the host helper out of the image"

# A fake docker for the extract step: `cp <cid>:<path> <dest>` copies from
# $FAKE_IMAGE/<path>; pull, create, and rm only log.
FAKE_DOCKER = """#!/bin/sh
echo "docker $*" >> "$FAKE_LOG"
case "$1" in
  create) echo fakecid ;;
  cp) cp "$FAKE_IMAGE/${2#*:}" "$3" ;;
esac
"""


def git(cwd, *args):
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    )


def make_history(path, steps):
    """steps: ("tag", name) or (subject, [trailer, ...]) in order, from an init commit."""
    git(path, "init", "-q", "-b", "main")
    git(path, "commit", "-q", "--allow-empty", "-m", "chore: init")
    for step in steps:
        if step[0] == "tag":
            git(path, "tag", step[1])
            continue
        subject, trailers = step
        args = ["commit", "-q", "--allow-empty", "-m", subject]
        if trailers:
            # One paragraph: git reads trailers from the last paragraph only.
            args += ["-m", "\n".join(trailers)]
        git(path, *args)


def yaml_doc():
    try:
        import yaml  # type: ignore
    except ImportError:
        raise unittest.SkipTest("PyYAML is not installed (uv run --with pyyaml ...)")
    return yaml.safe_load(TEXT)


class Pins(unittest.TestCase):
    def test_every_action_is_pinned_to_a_commit_with_a_version_comment(self):
        uses = re.findall(r"(?m)^\s*-?\s*uses:\s*(\S+)(.*)$", TEXT)
        self.assertTrue(uses)
        for ref, rest in uses:
            with self.subTest(ref=ref):
                self.assertRegex(ref, r"^[\w.-]+/[\w.-]+(/[\w./-]+)?@[0-9a-f]{40}$")
                self.assertRegex(rest, r"^\s+#\s+\S+", "a comment names the version")

    def test_the_default_permissions_are_read_only(self):
        self.assertRegex(TEXT, r"(?m)^permissions:\n  contents: read$")


class Release(unittest.TestCase):
    def test_the_image_job(self):
        self.assertIn('image="ghcr.io/${GITHUB_REPOSITORY,,}"', TEXT)
        self.assertIn('echo "$image:$VERSION"', TEXT)
        self.assertIn('if [[ "$PRERELEASE" == "true" ]]; then channel=pre; else channel=latest; fi', TEXT)
        self.assertIn("TERRARIUM_GIT_REF=${{ github.sha }}", TEXT)
        for label in ("version", "revision", "source"):
            self.assertIn(f"org.opencontainers.image.{label}=", TEXT)
        self.assertIn("provenance: mode=max", TEXT)
        self.assertIn("sbom: true", TEXT)
        self.assertIn("push-to-registry: true", TEXT)
        self.assertIn("subject-path: terrarium-host-*-linux-x86_64", TEXT)

    def test_the_asset_comes_out_of_the_image(self):
        self.assertIn("/opt/terrarium/dist/$bin-x86_64-linux", TEXT)
        self.assertIn("for bin in terrarium-host terrarium-host-helper; do", TEXT)
        self.assertIn("sha256sum \"$asset\" >> SHA256SUMS", TEXT)
        # The Dockerfile puts the binary and its .sha256 there.
        self.assertIn("COPY --from=host-build /dist/ /payload/dist/", DOCKERFILE)
        # docker create/cp in release CI does not run the entrypoint, so this
        # path must resolve to bundled files without an initialized volume.
        self.assertIn("ln -s /opt/terrarium/bundled/dist /opt/terrarium/dist", DOCKERFILE)
        self.assertIn("TERRARIUM_HOST_DIST_DIR=/state/releases/current/dist", DOCKERFILE)
        self.assertIn('sha256sum "$dist" > "$dist.sha256"', DOCKERFILE)

    def test_the_dockerfile_builds_the_helper_with_the_host_agent(self):
        self.assertIn("--bin terrarium-host --bin terrarium-host-helper", DOCKERFILE)
        self.assertRegex(DOCKERFILE, r"cargo build --release --locked --target \"\$target\" -p terrarium-host")
        self.assertIn("for bin in terrarium-host terrarium-host-helper; do", DOCKERFILE)

    def test_the_notes_verify_the_helper(self):
        self.assertIn(
            "gh attestation verify terrarium-host-helper-$VERSION-linux-x86_64 --repo $GITHUB_REPOSITORY", TEXT
        )

    def test_the_dockerfile_labels_the_image(self):
        for label in ("source", "licenses", "revision", "version"):
            self.assertIn(f"org.opencontainers.image.{label}=", DOCKERFILE)

    def test_jobs_needs_and_permissions(self):
        jobs = yaml_doc()["jobs"]
        self.assertEqual(set(jobs), {"check", "version", "image", "release"})
        for name, job in jobs.items():
            for need in job.get("needs", []):
                self.assertIn(need, jobs, f"{name} needs {need}")
        self.assertEqual(jobs["image"]["needs"], ["version", "check"])
        self.assertEqual(jobs["release"]["needs"], ["version", "image"])
        self.assertEqual(
            jobs["image"]["permissions"],
            {"contents": "read", "packages": "write", "id-token": "write", "attestations": "write"},
        )
        self.assertEqual(jobs["release"]["permissions"], {"contents": "write"})
        for name in ("check", "version"):
            self.assertNotIn("permissions", jobs[name], f"{name} keeps the read-only default")
        for name in ("image", "release"):
            self.assertEqual(jobs[name]["if"], "needs.version.outputs.do_release == 'true'")

    def test_the_release_has_the_asset_and_the_sums(self):
        steps = yaml_doc()["jobs"]["release"]["steps"]
        gh = next(s for s in steps if "action-gh-release" in s.get("uses", ""))
        self.assertEqual(gh["with"]["files"], "dist/*")
        self.assertTrue(gh["with"]["generate_release_notes"])
        self.assertEqual(gh["with"]["body_path"], "notes.md")
        dl = next(s for s in steps if "download-artifact" in s.get("uses", ""))
        self.assertTrue(dl["with"]["merge-multiple"])

    def test_no_expression_in_a_run_script_of_the_release_jobs(self):
        """Values reach the scripts through env, not ${{ }} in the script text."""
        jobs = yaml_doc()["jobs"]
        for name in ("image", "release"):
            for step in jobs[name]["steps"]:
                if "run" in step:
                    with self.subTest(job=name, step=step.get("name")):
                        self.assertNotIn("${{", step["run"])


class ExtractDryRun(unittest.TestCase):
    """Runs the extract step of the image job with a fake docker and fake binaries."""

    VERSION = "0.36.0-alpha"
    BINS = ("terrarium-host", "terrarium-host-helper")

    def extract(self, corrupt=None):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        dist = root / "image/opt/terrarium/dist"
        dist.mkdir(parents=True)
        for name in self.BINS:
            asset = f"{name}-x86_64-linux"
            (dist / asset).write_text(f"#!/bin/sh\necho '{name} {self.VERSION}+abc123'\n")
            digest = hashlib.sha256((dist / asset).read_bytes()).hexdigest()
            if name == corrupt:
                digest = "0" * 64
            (dist / f"{asset}.sha256").write_text(f"{digest}  {asset}\n")
        bindir = root / "bin"
        bindir.mkdir()
        (bindir / "docker").write_text(FAKE_DOCKER)
        (bindir / "docker").chmod(0o755)
        work = root / "work"
        work.mkdir()
        env = {
            "PATH": f"{bindir}:{os.environ['PATH']}",
            "HOME": tmp.name,
            "VERSION": self.VERSION,
            "IMAGE": "ghcr.io/o/terrarium",
            "DIGEST": "sha256:" + "a" * 64,
            "FAKE_IMAGE": str(root / "image"),
            "FAKE_LOG": str(root / "docker.log"),
        }
        proc = subprocess.run(
            ["bash", "-e", "-o", "pipefail", "-c", run_script(EXTRACT_STEP)],
            cwd=work,
            env=env,
            capture_output=True,
            text=True,
        )
        return proc, work

    def test_both_assets_and_their_sums(self):
        proc, work = self.extract()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        assets = [f"{name}-{self.VERSION}-linux-x86_64" for name in self.BINS]
        self.assertEqual(sorted(p.name for p in work.iterdir()), sorted(assets + ["SHA256SUMS"]))
        sums = (work / "SHA256SUMS").read_text().splitlines()
        self.assertEqual([line.split("  ", 1)[1] for line in sums], assets)
        check = subprocess.run(["sha256sum", "-c", "SHA256SUMS"], cwd=work, capture_output=True, text=True)
        self.assertEqual(check.returncode, 0, check.stdout + check.stderr)
        for asset in assets:
            self.assertTrue(os.access(work / asset, os.X_OK), asset)
            # The attest and upload globs pick up each asset.
            self.assertTrue(Path(asset).match("terrarium-host-*-linux-x86_64"), asset)

    def test_a_helper_hash_mismatch_fails(self):
        proc, _ = self.extract(corrupt="terrarium-host-helper")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("the image's terrarium-host-helper is", proc.stdout + proc.stderr)


class VersionCalc(unittest.TestCase):
    """Runs the `calc` step of the version job on a throwaway repo."""

    def calc(self, steps):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp, "repo")
            repo.mkdir()
            make_history(repo, steps)
            out = Path(tmp, "output")
            summary = Path(tmp, "summary")
            env = {
                "PATH": os.environ["PATH"],
                "HOME": tmp,
                "GITHUB_OUTPUT": str(out),
                "GITHUB_STEP_SUMMARY": str(summary),
            }
            proc = subprocess.run(
                ["bash", "-e", "-o", "pipefail", "-c", calc_script()],
                cwd=repo,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            values = dict(line.split("=", 1) for line in out.read_text().splitlines())
            return values

    def test_release_after_alpha_promotes_it(self):
        values = self.calc(
            [("tag", "v0.5.0-alpha"), ("fix: a", []), ("feat: b", ["release: true"])]
        )
        self.assertEqual(values["version"], "0.5.0")
        self.assertEqual(values["prerelease"], "false")
        self.assertEqual(values["do_release"], "true")

    def test_release_after_full_release_bumps(self):
        values = self.calc([("tag", "v0.5.0"), ("fix: a", []), ("fix: b", ["release: true"])])
        self.assertEqual(values["version"], "0.5.1")
        self.assertEqual(values["prerelease"], "false")

    def test_prerelease_after_alpha_bumps_as_before(self):
        values = self.calc([("tag", "v0.5.0-alpha"), ("fix: a", ["pre-release: true"])])
        self.assertEqual(values["version"], "0.5.1-alpha")
        self.assertEqual(values["prerelease"], "true")

    def test_bump_trailer_after_alpha_still_bumps(self):
        values = self.calc(
            [("tag", "v0.5.0-alpha"), ("fix: a", ["release: true", "bump: minor"])]
        )
        self.assertEqual(values["version"], "0.6.0")
        self.assertEqual(values["prerelease"], "false")

    def test_existing_stable_tag_blocks_promotion(self):
        # The nearest tag is the alpha, but v0.5.0 exists: no promotion, so the
        # release bumps from 0.5.0 instead of tagging an existing version.
        values = self.calc(
            [("tag", "v0.5.0"), ("fix: a", []), ("tag", "v0.5.0-alpha"), ("fix: b", ["release: true"])]
        )
        self.assertEqual(values["version"], "0.5.1")

    def test_release_and_prerelease_trailers_keep_the_prerelease(self):
        values = self.calc(
            [("tag", "v0.5.0-alpha"), ("fix: a", ["release: true", "pre-release: true"])]
        )
        self.assertEqual(values["version"], "0.5.1-alpha")
        self.assertEqual(values["prerelease"], "true")


if __name__ == "__main__":
    unittest.main()
