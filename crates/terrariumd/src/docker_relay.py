"""Trusted release code: target Unix socket to daemon executor stream.

No credential is accepted here. The daemon owns the SSH transport.

Each direction ends on its own with an "eof" frame (a half-close). The
Docker CLI half-closes a hijacked exec or attach connection when its stdin
ends and then reads the output until Docker closes. A full close on the
client's half-close lost that output (TER-540). A connection closes when both
directions ended, or at once on an error or a "close" frame.
"""
import base64
import json
import os
import queue
import signal
import socket
import sys
import threading

CHUNK_SIZE = 256 * 1024
MAX_FRAME = 512 * 1024
MAX_CONNECTIONS = 32
# Queued after the last data of a direction: shut down the write side.
EOF = object()


def serve(directory):
    os.makedirs(os.path.dirname(directory), exist_ok=True)
    os.mkdir(directory, 0o700)
    path = os.path.join(directory, "docker.sock")
    listener = socket.socket(socket.AF_UNIX)
    connections = {}
    credits = {}
    writes = {}
    # The directions that ended with a half-close, per connection.
    halves = {}
    output_lock = threading.Lock()
    state_lock = threading.Lock()

    def emit(event):
        with output_lock:
            sys.stdout.write(json.dumps(event) + "\n")
            sys.stdout.flush()

    def close(ident):
        with state_lock:
            connection = connections.pop(ident, None)
            credit = credits.pop(ident, None)
            pending = writes.pop(ident, None)
            halves.pop(ident, None)
        if pending:
            # Wake an idle writer without ever blocking connection teardown.
            # A full queue is consumed by the active writer, which exits on
            # the closed socket rather than returning to pending.get().
            try:
                pending.put_nowait(None)
            except queue.Full:
                pass
        if credit:
            credit.set()
        if connection:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()
        return connection is not None

    def finish(ident, half):
        with state_lock:
            done = halves.get(ident)
            if done is None:
                return
            done.add(half)
            if len(done) < 2:
                return
        if close(ident):
            emit({"id": ident, "close": True})

    def read(ident, connection, credit):
        try:
            while True:
                data = connection.recv(CHUNK_SIZE)
                if not data:
                    # The client half-closed. Keep its read side open for
                    # the rest of the answer.
                    with state_lock:
                        alive = ident in connections
                    if alive:
                        emit({"id": ident, "eof": True})
                        finish(ident, "read")
                    return
                emit({"id": ident, "data": base64.b64encode(data).decode("ascii")})
                credit.wait()
                credit.clear()
                with state_lock:
                    if ident not in connections:
                        break
        except OSError:
            pass
        if close(ident):
            emit({"id": ident, "close": True})

    def write(ident, connection, pending):
        try:
            while True:
                data = pending.get()
                with state_lock:
                    if data is None or ident not in connections:
                        return
                if data is EOF:
                    connection.shutdown(socket.SHUT_WR)
                    finish(ident, "write")
                    return
                connection.sendall(data)
                emit({"id": ident, "ack": True})
        except OSError:
            if close(ident):
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
                credit = threading.Event()
                pending = queue.Queue(maxsize=1)
                credits[ident] = credit
                writes[ident] = pending
                halves[ident] = set()
            emit({"id": ident, "open": True})
            threading.Thread(target=read, args=(ident, connection, credit), daemon=True).start()
            threading.Thread(target=write, args=(ident, connection, pending), daemon=True).start()

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
                credit = credits.get(ident)
                pending = writes.get(ident)
            if not connection:
                continue
            if event.get("ack"):
                credit.set()
            elif event.get("close"):
                close(ident)
            elif event.get("eof"):
                pending.put(EOF)
            elif "data" in event:
                pending.put(base64.b64decode(event["data"], validate=True))
    finally:
        listener.close()
        for ident in list(connections):
            close(ident)
        if os.path.exists(path):
            os.unlink(path)
        os.rmdir(directory)


if __name__ == "__main__":
    serve(sys.argv[1])
