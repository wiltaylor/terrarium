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
import threading
import unittest

RELAY = Path(__file__).resolve().parents[1] / "crates/terrariumd/src/docker_relay.py"


class DockerRelayTests(unittest.TestCase):
    def test_many_closed_connections_release_all_worker_threads(self):
        with tempfile.TemporaryDirectory(prefix="ter-363-threads-") as root:
            directory = Path(root, "job")
            # Run the release module in-process in an isolated interpreter so
            # its actual thread count, not only its socket map, is measured.
            probe = """
import importlib.util, json, pathlib, socket, sys, threading, time
spec = importlib.util.spec_from_file_location('relay', sys.argv[1])
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)
directory = sys.argv[2]
sys.argv = [sys.argv[0], directory, sys.argv[3]]
base = threading.active_count()
def exercise():
    path = pathlib.Path(directory) / 'docker.sock'
    while not path.exists(): time.sleep(0.01)
    for _ in range(200):
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(str(path))
        time.sleep(0.002)
    for _ in range(500):
        # Main + exercise + listener, with no remaining per-client workers.
        if threading.active_count() == base + 2: break
        time.sleep(0.01)
    assert threading.active_count() == base + 2, threading.active_count()
    print(json.dumps({'threads_released': True}), flush=True)
threading.Thread(target=exercise, daemon=True).start()
relay.serve(directory)
"""
            child = subprocess.Popen([sys.executable, "-I", "-u", "-c", probe, str(RELAY), str(directory), "30"],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                while True:
                    line = child.stdout.readline()
                    if not line:
                        self.fail(child.stderr.read())
                    event = json.loads(line)
                    if event.get("threads_released"):
                        break
                    if event.get("eof"):
                        # The daemon's answer to a client close: Docker ends
                        # its side too (TER-540 half-close).
                        child.stdin.write(json.dumps({"id": event["id"], "eof": True}) + "\n")
                        child.stdin.flush()
                child.stdin.write('{"stop":true}\n')
                child.stdin.flush()
                self.assertEqual(child.wait(timeout=5), 0, child.stderr.read())
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                for stream in (child.stdin, child.stdout, child.stderr):
                    stream.close()

    def test_large_bidirectional_stream_with_slow_consumers(self):
        with tempfile.TemporaryDirectory(prefix="ter-363-stream-") as root:
            directory = Path(root, "job")
            child = subprocess.Popen([sys.executable, "-u", str(RELAY), str(directory), "60"],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
            payload = bytes(range(256)) * 32768
            received = bytearray()
            errors = []
            def send(value):
                child.stdin.write(json.dumps(value) + "\n")
                child.stdin.flush()
            try:
                self.assertEqual(json.loads(child.stdout.readline()), {"ready": True})
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(30)
                    client.connect(str(directory / "docker.sock"))
                    ident = json.loads(child.stdout.readline())["id"]
                    def client_io():
                        try:
                            while len(received) < len(payload):
                                data = client.recv(4096)
                                if not data:
                                    break
                                received.extend(data)
                        except Exception as error:
                            errors.append(error)
                    worker = threading.Thread(target=client_io)
                    worker.start()
                    sender = threading.Thread(target=client.sendall, args=(payload,))
                    sender.start()
                    # Echo each request frame only once the previous response
                    # has been consumed. Eight MiB exceeds every pipe/socket
                    # and channel buffer in the transport.
                    total = 0
                    pending = 0
                    while total < len(payload) or pending:
                        event = json.loads(child.stdout.readline())
                        if "data" in event:
                            data = base64.b64decode(event["data"])
                            total += len(data)
                            send({"id": ident, "ack": True})
                            send({"id": ident, "data": event["data"]})
                            pending += 1
                        elif event.get("ack"):
                            pending -= 1
                    worker.join(timeout=30)
                    sender.join(timeout=30)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(errors, [])
                    self.assertEqual(bytes(received), payload)
                send({"stop": True})
                self.assertEqual(child.wait(timeout=5), 0, child.stderr.read())
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                for stream in (child.stdin, child.stdout, child.stderr):
                    stream.close()

    def test_answer_after_client_half_close_reaches_the_client(self):
        # TER-540: docker exec half-closes, then reads the exec output.
        with tempfile.TemporaryDirectory(prefix="ter-540-halfclose-") as root:
            directory = Path(root, "job")
            child = subprocess.Popen([sys.executable, "-I", "-u", str(RELAY), str(directory), "10"],
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
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(5)
                    client.connect(str(directory / "docker.sock"))
                    ident = event()["id"]
                    client.sendall(b"POST /exec/x/start")
                    client.shutdown(socket.SHUT_WR)
                    self.assertEqual(base64.b64decode(event()["data"]), b"POST /exec/x/start")
                    send({"id": ident, "ack": True})
                    self.assertEqual(event(), {"id": ident, "eof": True})
                    for part in (b"HTTP/1.1 101 UPGRADED\r\n\r\n", b"seed"):
                        send({"id": ident, "data": base64.b64encode(part).decode()})
                        self.assertEqual(event(), {"id": ident, "ack": True})
                    send({"id": ident, "eof": True})
                    received = b""
                    while True:
                        data = client.recv(1024)
                        if not data:
                            break
                        received += data
                    self.assertEqual(received, b"HTTP/1.1 101 UPGRADED\r\n\r\nseed")
                    # Both directions ended: the relay closes on its own.
                    self.assertEqual(event(), {"id": ident, "close": True})
                send({"stop": True})
                self.assertEqual(child.wait(timeout=5), 0, child.stderr.read())
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                for stream in (child.stdin, child.stdout, child.stderr):
                    stream.close()

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
