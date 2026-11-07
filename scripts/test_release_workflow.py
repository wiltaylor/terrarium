"""Tests for the release part of .github/workflows/ci.yml (docs/release.md).

They read the workflow as text (and as YAML when PyYAML is there) and check
the rules of the release: the image goes to GHCR with an attestation, the
host agent asset comes out of the image with SHA256SUMS, each action is
pinned to a full commit SHA, and each job has the least permissions.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

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


def calc_script() -> str:
    """The run script of the `calc` step of the version job."""
    lines = TEXT.splitlines()
    start = lines.index("      - id: calc")
    run = lines.index("        run: |", start)
    body = []
    for line in lines[run + 1 :]:
        if line.strip() and not line.startswith("          "):
            break
        body.append(line)
    return textwrap.dedent("\n".join(body))


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
        self.assertIn("/opt/terrarium/dist/terrarium-host-x86_64-linux", TEXT)
        self.assertIn("sha256sum \"$asset\" > SHA256SUMS", TEXT)
        # The Dockerfile puts the binary and its .sha256 there.
        self.assertIn("COPY --from=host-build /dist/ /opt/terrarium/dist/", DOCKERFILE)
        self.assertIn('sha256sum "$dist" > "$dist.sha256"', DOCKERFILE)

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
