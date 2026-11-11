"""Static checks for the host artifacts and dist layout in docker/Dockerfile.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
DOCKERFILE = (REPO / "docker/Dockerfile").read_text()


class HostArtifacts(unittest.TestCase):
    def test_builds_the_helper_with_the_host_agent(self):
        self.assertIn("--bin terrarium-host --bin terrarium-host-helper", DOCKERFILE)
        self.assertRegex(
            DOCKERFILE,
            r'cargo build --release --locked --target "\$target" -p terrarium-host',
        )
        self.assertIn("for bin in terrarium-host terrarium-host-helper; do", DOCKERFILE)

    def test_bundles_both_artifacts_and_their_checksums(self):
        self.assertIn('dist="$bin-$(uname -m)-linux"', DOCKERFILE)
        self.assertIn('cp "/out/$bin" "/dist/$dist"', DOCKERFILE)
        self.assertIn('sha256sum "$dist" > "$dist.sha256"', DOCKERFILE)
        self.assertIn("COPY --from=host-build /dist/ /payload/dist/", DOCKERFILE)
        self.assertIn(
            "COPY --from=release-payload /payload/ /opt/terrarium/bundled/", DOCKERFILE
        )

    def test_dist_is_available_without_an_initialized_state_volume(self):
        # Image extraction does not run the entrypoint. The public dist link
        # must resolve to the bundled payload, not the active state release.
        self.assertIn("ln -s /opt/terrarium/bundled/dist /opt/terrarium/dist", DOCKERFILE)
        self.assertIn("TERRARIUM_HOST_DIST_DIR=/state/releases/current/dist", DOCKERFILE)

    def test_labels_the_image(self):
        for label in ("source", "licenses", "revision", "version"):
            with self.subTest(label=label):
                self.assertIn(f"org.opencontainers.image.{label}=", DOCKERFILE)


if __name__ == "__main__":
    unittest.main()
