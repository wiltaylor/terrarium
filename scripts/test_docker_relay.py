"""Exercise the trusted relay without Docker, SSH, or a credential."""
import base64
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import unittest

RELAY = Path(__file__).resolve().parents[1] / "crates/terrariumd/src/docker_relay.py"


class DockerRelayTests(unittest.TestCase):
    def test_socket_round_trip_and_teardown_on_stop_and_eof(self):
        for explicit_stop in (True, False):
            with tempfile.TemporaryDirectory(prefix="ter-363-relay-") as root:
                directory = Path(root, "job")
                child = subprocess.Popen([sys.executable, "-u", str(RELAY), str(directory), "10"],
                                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                         stderr=subprocess.PIPE, text=True)
                def event():
                    self.assertTrue(select.select([child.stdout], [], [], 5)[0])
                    return json.loads(child.stdout.readline())
                def send(value):
                    child.stdin.write(json.dumps(value) + "\n")
                    child.stdin.flush()
                try:
                    self.assertEqual(event(), {"ready": True})
                    path = directory / "docker.sock"
                    self.assertEqual(os.stat(directory).st_mode & 0o777, 0o700)
                    self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)
                    with socket.socket(socket.AF_UNIX) as client:
                        client.settimeout(5)
                        client.connect(str(path))
                        opened = event()
                        ident = opened["id"]
                        self.assertTrue(opened["open"])
                        client.sendall(b"docker-request")
                        data = event()
                        self.assertEqual(base64.b64decode(data["data"]), b"docker-request")
                        send({"id": ident, "ack": True})
                        send({"id": ident, "data": base64.b64encode(b"docker-response").decode()})
                        self.assertEqual(client.recv(1024), b"docker-response")
                    if explicit_stop:
                        send({"stop": True})
                    else:
                        child.stdin.close()
                    self.assertEqual(child.wait(timeout=5), 0, child.stderr.read())
                    self.assertFalse(directory.exists())
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait()
                    for stream in (child.stdin, child.stdout, child.stderr):
                        stream.close()


if __name__ == "__main__":
    unittest.main()
