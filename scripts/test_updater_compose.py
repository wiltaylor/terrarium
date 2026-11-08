#!/usr/bin/env python3
"""Opt-in TER-298 acceptance on a disposable Docker host.

TER_298_COMPOSE_TEST_BINARY must name the compiled compose_environment libtest
binary. All containers, networks and built images belong to this test project.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


@unittest.skipUnless(os.environ.get("TER_298_COMPOSE_TEST_BINARY"), "scratch Docker acceptance only")
class UpdaterComposeTest(unittest.TestCase):
    def test_edited_dotenv_reaches_new_container(self):
        binary = Path(os.environ["TER_298_COMPOSE_TEST_BINARY"]).resolve()
        project = f"ter-298-scratch-{os.getpid()}"
        with tempfile.TemporaryDirectory(prefix="ter-298-") as directory:
            root = Path(directory)
            source = root / "src"
            source.mkdir()
            (root / "test").write_bytes(binary.read_bytes())
            (root / "test").chmod(0o755)
            (root / "Dockerfile").write_text(
                "FROM docker:29.0.3-cli AS cli\n"
                "FROM debian:trixie-slim\n"
                "COPY --from=cli /usr/local/bin/docker /usr/local/bin/docker\n"
                "COPY --from=cli /usr/local/libexec/docker/cli-plugins/docker-compose "
                "/usr/local/libexec/docker/cli-plugins/docker-compose\n"
                "COPY test /test\n"
            )
            config = source / "compose.yaml"
            config.write_text(
                "services:\n  terrarium:\n    image: busybox:1.37.0\n"
                "    command: [sleep, '600']\n"
                "    environment:\n"
                "      TERRARIUM_MAIL_LOOP_LIMIT: ${TERRARIUM_MAIL_LOOP_LIMIT:-}\n"
            )
            env = root / ".env"
            env.write_text("# initial environment\n")
            compose = ["docker", "compose", "--project-name", project,
                       "--file", str(config), "--env-file", str(env)]
            image = f"{project}:test"
            try:
                subprocess.run(["docker", "build", "-t", image, str(root)], check=True)
                subprocess.run([*compose, "up", "-d"], check=True)
                target = subprocess.check_output([*compose, "ps", "-q", "terrarium"], text=True).strip()
                # The daemon remains unchanged. Only the deployment .env changes.
                env.write_text("TERRARIUM_MAIL_LOOP_LIMIT=30\n")
                command = [
                    "docker", "run", "--rm", "--name", f"{project}-runner",
                    "-v", "/var/run/docker.sock:/var/run/docker.sock",
                    "-e", f"TER_298_SCRATCH_TARGET={project}-terrarium-1",
                    "-e", "RUSTC_BOOTSTRAP=1",
                ]
                mounts = ["-v", f"{source}:/compose:ro", "-v", f"{root}:/deployment:ro"]
                def run_case(name, volumes):
                    result = subprocess.run([
                        *command, *volumes, image, "/test", name, "--ignored",
                        "--format", "json", "--report-time", "-Z", "unstable-options",
                    ], text=True, capture_output=True)
                    # Preserve libtest output unmodified for test push.
                    output = os.environ.get("TER_298_COMPOSE_RESULTS")
                    if output:
                        Path(f"{output}.{name}.json").write_text(result.stdout)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                run_case("update_reads_edited_dotenv_and_recovery_keeps_original_environment", mounts)
                run_case("missing_mounts_stop_resolution", [])
                original = config.read_text()
                config.write_text(original + "      REQUIRED: ${REQUIRED:?diagnostic-sentinel}\n")
                try:
                    run_case("interpolation_failure_is_redacted_and_leaves_original_container", mounts)
                finally:
                    config.write_text(original)
                self.assertTrue(target)
            finally:
                subprocess.run([*compose, "down"], check=False)
                subprocess.run(["docker", "image", "rm", image], check=False)


if __name__ == "__main__":
    unittest.main()
