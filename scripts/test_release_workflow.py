"""Tests for the release part of .github/workflows/ci.yml (docs/release.md).

They read the workflow as text (and as YAML when PyYAML is there) and check
the rules of the release: the image goes to GHCR with an attestation, the
host agent asset comes out of the image with SHA256SUMS, each action is
pinned to a full commit SHA, and each job has the least permissions.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TEXT = (REPO / ".github/workflows/ci.yml").read_text()
DOCKERFILE = (REPO / "docker/Dockerfile").read_text()


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


if __name__ == "__main__":
    unittest.main()
