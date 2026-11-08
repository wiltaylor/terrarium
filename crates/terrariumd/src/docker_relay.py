"""Trusted release code: target Unix socket to daemon executor stream.

No credential is accepted here. The daemon owns the SSH transport.
"""
import base64
import json
import os
import signal
import socket
import sys
import threading

MAX_FRAME = 65536
MAX_CONNECTIONS = 32


def serve(directory):
    os.makedirs(os.path.dirname(directory), exist_ok=True)
    os.mkdir(directory, 0o700)
    path = os.path.join(directory, "docker.sock")
    listener = socket.socket(socket.AF_UNIX)
    connections = {}
    output_lock = threading.Lock()
    state_lock = threading.Lock()

    def emit(event):
        with output_lock:
            sys.stdout.write(json.dumps(event) + "\n")
            sys.stdout.flush()

    def close(ident):
        with state_lock:
            connection = connections.pop(ident, None)
        if connection:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()

    def read(ident, connection):
        try:
            while True:
                data = connection.recv(16384)
                if not data:
                    break
                emit({"id": ident, "data": base64.b64encode(data).decode("ascii")})
        except OSError:
            pass
        finally:
            close(ident)
            emit({"id": ident, "close": True})

    def accept():
        ident = 0
        while True:
            try:
                connection, _ = listener.accept()
            except OSError:
                return
            with state_lock:
                if len(connections) >= MAX_CONNECTIONS:
                    connection.close()
                    continue
                ident += 1
                connections[ident] = connection
            emit({"id": ident, "open": True})
            threading.Thread(target=read, args=(ident, connection), daemon=True).start()

    def stop(*_):
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGALRM, stop)
    signal.alarm(int(sys.argv[2]))
    try:
        listener.bind(path)
        os.chmod(path, 0o600)
        listener.listen(MAX_CONNECTIONS)
        threading.Thread(target=accept, daemon=True).start()
        emit({"ready": True})
        while True:
            line = sys.stdin.buffer.readline(MAX_FRAME + 1)
            if not line or len(line) > MAX_FRAME:
                break
            event = json.loads(line)
            if event.get("stop"):
                break
            ident = event["id"]
            with state_lock:
                connection = connections.get(ident)
            if not connection:
                continue
            if event.get("close"):
                close(ident)
            elif "data" in event:
                try:
                    connection.sendall(base64.b64decode(event["data"], validate=True))
                except OSError:
                    close(ident)
    finally:
        listener.close()
        for ident in list(connections):
            close(ident)
        if os.path.exists(path):
            os.unlink(path)
        os.rmdir(directory)


if __name__ == "__main__":
    serve(sys.argv[1])
