"""Credential-free relay benchmark with a simulated 5 ms executor RTT.

Set TERRARIUM_BENCH_RELAY to compare an older relay with the same workload.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest


class RelayThroughputTests(unittest.TestCase):
    def test_stream_digest_and_throughput(self):
        relay = os.environ.get("TERRARIUM_BENCH_RELAY", str(
            Path(__file__).resolve().parents[1] / "crates/terrariumd/src/docker_relay.py"))
        payload = bytes(range(256)) * (128 * 1024)
        with tempfile.TemporaryDirectory(prefix="ter-781-") as root:
            directory = Path(root) / "relay"
            child = subprocess.Popen(
                [sys.executable, "-u", relay, str(directory), "60"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            worker = None
            try:
                self.assertEqual(json.loads(child.stdout.readline()), {"ready": True})
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(60)
                    client.connect(str(directory / "docker.sock"))
                    ident = json.loads(child.stdout.readline())["id"]
                    worker = threading.Thread(target=client.sendall, args=(payload,))
                    digest = hashlib.sha256()
                    total = 0
                    frames = 0
                    started = time.monotonic()
                    worker.start()
                    while total < len(payload):
                        event = json.loads(child.stdout.readline())
                        data = base64.b64decode(event["data"], validate=True)
                        digest.update(data)
                        total += len(data)
                        frames += 1
                        time.sleep(0.005)
                        child.stdin.write((json.dumps({"id": ident, "ack": True}) + "\n").encode())
                        child.stdin.flush()
                    elapsed = time.monotonic() - started
                    worker.join(timeout=5)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(total, len(payload))
                    self.assertEqual(digest.digest(), hashlib.sha256(payload).digest())
                    print(f"relay={relay} bytes={total} frames={frames} seconds={elapsed:.3f} "
                          f"MiB/s={total / elapsed / 1048576:.3f} sha256={digest.hexdigest()}", flush=True)
            finally:
                if child.poll() is None:
                    child.stdin.write(b'{"stop":true}\n')
                    child.stdin.flush()
                child.wait(timeout=5)
                if worker is not None:
                    worker.join(timeout=5)
                for stream in (child.stdin, child.stdout, child.stderr):
                    stream.close()


if __name__ == "__main__":
    unittest.main()
