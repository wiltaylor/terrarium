#!/usr/bin/env python3
"""Card-owned scratch fixture. No provider credentials. Never prints keys.

Run `start <scratch-IP>`, then `stop`. Only `serve` runs in the Claude
container. Build the Rust test binary on build2. With the lead's approval,
copy it to scratch and run its single ignored test in a card-owned container
on this network, with TER798_CONNECTION=/fixture/connection.json and the
fixture directory mounted at /fixture. Never install a toolchain on scratch.
The mock is not a provider/billing test. Do not use on Docker CI.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import secrets
import shutil
import ssl
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path("/tmp/ter-798-litellm")
NETWORK = "ter-798-litellm"
CONTAINERS = ["ter-798-claude", "ter-798-proxy", "ter-798-postgres"]
VOLUME = "ter-798-postgres"
IMAGE = "ter-798-claude:proof"
BASES = ["node:22-bookworm", "postgres:16-alpine", "ghcr.io/berriai/litellm:main-stable"]


def docker(*args, **kwargs):
    return subprocess.run(["docker", *args], check=True, capture_output=True, **kwargs)


def request(url, key, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data, {"Authorization": "Bearer " + key,
                                           "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as response:
        return json.load(response)


def certificates(ip):
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                    "-keyout", str(ROOT / "ca.key"), "-out", str(ROOT / "ca.crt"),
                    "-days", "1", "-subj", "/CN=ter-798-ca"],
                   check=True, capture_output=True)
    subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes",
                    "-keyout", str(ROOT / "tls.key"), "-out", str(ROOT / "tls.csr"),
                    "-subj", "/CN=ter-798"], check=True, capture_output=True)
    (ROOT / "tls.ext").write_text(
        f"subjectAltName=IP:{ip},DNS:localhost\nbasicConstraints=critical,CA:FALSE\n"
        "keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    subprocess.run(["openssl", "x509", "-req", "-in", str(ROOT / "tls.csr"),
                    "-CA", str(ROOT / "ca.crt"), "-CAkey", str(ROOT / "ca.key"),
                    "-CAcreateserial", "-out", str(ROOT / "tls.crt"), "-days", "1",
                    "-extfile", str(ROOT / "tls.ext")], check=True, capture_output=True)


def start(ip):
    ROOT.mkdir(mode=0o700)  # Refuse to overwrite a previous run.
    pulled = []
    (ROOT / "pulled.json").write_text("[]")
    for image in BASES:
        if subprocess.run(["docker", "image", "inspect", image], capture_output=True).returncode:
            docker("pull", image)
            pulled.append(image)
            (ROOT / "pulled.json").write_text(json.dumps(pulled))
    master = "sk-" + secrets.token_hex(24)
    control = secrets.token_hex(32)
    (ROOT / "control").write_text(control)
    certificates(ip)
    config = """model_list:
  - model_name: claude-sonnet-4-5
    litellm_params:
      model: anthropic/claude-sonnet-4-5
      api_key: not-a-provider-key
      mock_response: TER798_MOCK_OK
litellm_settings:
  drop_params: true
general_settings:
  master_key: os.environ/LITELLM_MASTER_KEY
"""
    (ROOT / "config.yaml").write_text(config)
    shutil.copyfile(__file__, ROOT / "live.py")
    (ROOT / "Dockerfile").write_text(
        "FROM node:22-bookworm\n"
        "RUN npm install --global @anthropic-ai/claude-code@2.1.39\n"
        "COPY live.py /fixture.py\n"
        "CMD [\"python3\", \"/fixture.py\", \"serve\"]\n")
    docker("build", "-t", IMAGE, str(ROOT))
    docker("network", "create", NETWORK)
    docker("volume", "create", VOLUME)
    docker("run", "-d", "--name", CONTAINERS[2], "--network", NETWORK,
           "-e", "POSTGRES_PASSWORD=throwaway-not-used-elsewhere", "-e", "POSTGRES_DB=litellm",
           "-v", VOLUME + ":/var/lib/postgresql/data", "postgres:16-alpine")
    env = dict(os.environ, LITELLM_MASTER_KEY=master)
    docker("run", "-d", "--name", CONTAINERS[1], "--network", NETWORK,
           "-e", "LITELLM_MASTER_KEY", "-e",
           "DATABASE_URL=postgresql://postgres:throwaway-not-used-elsewhere@ter-798-postgres:5432/litellm",
           "-v", str(ROOT / "config.yaml") + ":/config.yaml:ro",
           "-p", "127.0.0.1:17987:4000", BASES[2],
           "--config", "/config.yaml", "--port", "4000", env=env)
    for _ in range(120):
        try:
            key = request("http://127.0.0.1:17987/key/generate", master,
                          {"models": ["claude-sonnet-4-5"], "max_budget": 100,
                           "budget_duration": "1d"})["key"]
            break
        except (urllib.error.URLError, OSError):
            time.sleep(2)
    else:
        raise RuntimeError("LiteLLM did not become ready")
    # The Rust fixture client trusts this cert. The production probe deliberately
    # does not: that exercises its sanitized failure and the allowed fallback.
    connection = {"base_url": f"https://{ip}:17988", "key": key,
                  "control": control, "certificate": (ROOT / "ca.crt").read_text()}
    (ROOT / "connection.json").write_text(json.dumps(connection))
    docker("run", "-d", "--name", CONTAINERS[0], "--network", NETWORK,
           "-v", str(ROOT) + ":/fixture:ro", "-p", "17988:8443", IMAGE)
    print("Fixture ready. Private connection file: /tmp/ter-798-litellm/connection.json")


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass  # Never log URLs, headers or request bodies.

    def do_POST(self):
        self.handle_request()

    def do_GET(self):
        self.handle_request()

    def handle_request(self):
        data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path == "/_fixture/run":
            if self.headers.get("Authorization") != "Bearer " + Path("/fixture/control").read_text():
                self.send_error(403)
                return
            supplied = json.loads(data)
            assert set(supplied) == {"ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"}
            expected = json.loads(Path("/fixture/connection.json").read_text())
            assert supplied["ANTHROPIC_BASE_URL"] == expected["base_url"]
            assert supplied["ANTHROPIC_AUTH_TOKEN"] == expected["key"]
            with tempfile.TemporaryDirectory() as home:
                env = {"PATH": os.environ["PATH"], "HOME": home,
                       "NODE_EXTRA_CA_CERTS": "/fixture/ca.crt",
                       "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", **supplied}
                proc = subprocess.run(
                    ["claude", "-p", "Say TER798_MOCK_OK. Do not use tools.",
                     "--model", "claude-sonnet-4-5", "--output-format", "json",
                     "--tools", ""], env=env, capture_output=True, timeout=120)
                version = subprocess.run(["claude", "--version"], capture_output=True,
                                         text=True, check=True).stdout.strip()
                # Return structured model output only, never arbitrary stderr.
                result = {"exit_code": proc.returncode, "version": version}
                if proc.returncode == 0:
                    result["result"] = json.loads(proc.stdout)
                    session = result["result"]["session_id"]
                    transcripts = list(Path(home).glob(f".claude/projects/*/{session}.jsonl"))
                    if len(transcripts) == 1:
                        result["transcript"] = transcripts[0].read_text()
                else:
                    result["error"] = "Claude Code failed (output suppressed)"
            body = json.dumps(result).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        req = urllib.request.Request("http://ter-798-proxy:4000" + self.path,
                                     data if self.command == "POST" else None,
                                     {k: v for k, v in self.headers.items()
                                      if k.lower() not in ("host", "content-length")},
                                     method=self.command)
        try:
            response = urllib.request.urlopen(req, timeout=120)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            body = response.read()
            self.send_response(response.code)
            self.send_header("Content-Type", response.headers.get("Content-Type", "application/json"))
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)


def serve():
    server = http.server.ThreadingHTTPServer(("0.0.0.0", 8443), Handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain("/fixture/tls.crt", "/fixture/tls.key")
    server.socket = context.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


def stop():
    # Exact card-owned names only. No prune, patterns or shared-image removal.
    for name in CONTAINERS:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)
    for kind, name in [("volume", VOLUME), ("network", NETWORK), ("image", IMAGE)]:
        subprocess.run(["docker", kind, "rm", name], capture_output=True, check=True)
    for image in json.loads((ROOT / "pulled.json").read_text()):
        docker("image", "rm", image)
    shutil.rmtree(ROOT)
    print("Removed card containers, volume, network, built/pulled images and TLS/key temp files")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["start", "serve", "stop"])
    parser.add_argument("ip", nargs="?")
    args = parser.parse_args()
    if args.action == "start":
        if not args.ip:
            parser.error("start requires the scratch IP")
        start(args.ip)
    elif args.action == "serve":
        serve()
    else:
        stop()
