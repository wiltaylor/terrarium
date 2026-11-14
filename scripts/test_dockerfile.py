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

    def test_the_daemon_build_has_the_compose_file_it_embeds(self):
        # TER-1209: terrariumd embeds compose.yaml (release-env-keys).
        ignore = (REPO / "docker/Dockerfile.dockerignore").read_text()
        self.assertIn("COPY compose.yaml ./compose.yaml", DOCKERFILE)
        self.assertIn("\n!compose.yaml\n", ignore)

    def test_every_cargo_stage_has_the_cargo_config(self):
        # TER-1314: .cargo/config.toml sets LIBSQLITE3_FLAGS. Without it an
        # image stage builds a different SQLite than the tests use.
        ignore = (REPO / "docker/Dockerfile.dockerignore").read_text()
        config = (REPO / ".cargo/config.toml").read_text()
        self.assertIn("\n!.cargo/config.toml\n", ignore)
        self.assertIn('LIBSQLITE3_FLAGS = "-DSQLITE_DEFAULT_MEMSTATUS=0"', config)
        code = "\n".join(
            line for line in DOCKERFILE.splitlines() if not line.lstrip().startswith("#")
        )
        # The text before the first FROM is no stage.
        stages = code.split("\nFROM ")[1:]
        cargo_stages = [s for s in stages if "cargo build" in s]
        self.assertGreaterEqual(len(cargo_stages), 3)
        for stage in cargo_stages:
            name = stage.split("\n", 1)[0]
            with self.subTest(stage=name):
                copy = stage.find("COPY .cargo ./.cargo\n")
                self.assertNotEqual(copy, -1)
                self.assertLess(copy, stage.find("cargo build"))

    def test_labels_the_image(self):
        for label in ("source", "licenses", "revision", "version"):
            with self.subTest(label=label):
                self.assertIn(f"org.opencontainers.image.{label}=", DOCKERFILE)


if __name__ == "__main__":
    unittest.main()
