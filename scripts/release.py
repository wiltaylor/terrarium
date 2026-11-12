"""The local release build: test, build, sign, and publish a release (TER-613).

It replaces the version, image and release jobs of the GitHub CI
(docs/release.md). The release job runs it on the release client (a small
VM, as the non-root user `release`) on a clean checkout of the commit to
release. Everything that runs the code of the release (the gate, the
image build and the --version checks) runs on the release VM's Docker daemon (TERRARIUM_DOCKER_HOST,
the daemon's relay), which gets no secret. The client only pushes, signs,
and creates the release:

    just release::dry-run          # all but the push, the signatures and the release
    just release::publish          # the release

Steps (each one writes its part of <out>/summary.json):

1. version  scripts/release_version.py: the trailers of HEAD. No trailer:
            no release, exit 0.
2. check    a clean tree; publish: the secrets in the env, the tools on
            PATH, docker/release/cosign.pub, the scopes and the expiry of
            the GitHub token (the headers only), and no tag or release
            v<version> yet.
3. gate     `just ci::validate` (or --gate) as uid 1001 in a container
            on the release VM: the image of docker/release-gate.Dockerfile
            (the toolchain of the old CI), and a clone of the commit copied
            in. The container gets no secret. Its RESULTS_DIR comes back to
            <out>/results. Red: stop, nothing is published.
4. stamp    the version into Cargo.toml and Cargo.lock (the workspace
            crates), as .github/stamp-version.sh did, without cargo.
5. image    `docker buildx build` of the runtime stage of docker/Dockerfile
            for linux/amd64, with the OCI labels, an SBOM and provenance
            (mode=max), on the Docker daemon of TERRARIUM_DOCKER_HOST (the
            release VM, through the daemon's relay). The export is an OCI
            layout on this side, <out>/image: the VM gets no secret.
6. extract  the host agent and the host helper out of that layout, with
            the two checks of the old CI: the SHA-256 is the .sha256 of the
            image, and `--version` is the release version. The --version
            check runs on the release VM (no network, uid 65534, no
            capabilities): the client never runs a binary of the release.
            Writes SHA256SUMS.
7. push     publish: crane pushes the layout as <image>:<version> (the
            same bytes, so the same digest) and tags :pre or :latest.
8. sign     publish: cosign signs the image digest, both binaries and
            SHA256SUMS with the release key (env://COSIGN_PRIVATE_KEY, no
            key file), then verifies each signature with
            docker/release/cosign.pub of the checkout.
9. release  publish: `gh release create v<version> --target <commit>`
            (the tag comes with the release) with the assets and bundles.

Secrets, only in the env of this process on the client: GHCR_TOKEN
(GHCR_USER, default wiltaylor) and GH_TOKEN (both the Terrarium secret
release-github, one classic token), COSIGN_PRIVATE_KEY
(release-cosign.private_key: the PEM, or the PEM base64-encoded in one
line) and COSIGN_PASSWORD. Each command gets only the secrets that it
needs: docker (the gate and the build) gets none. The registry login
(DOCKER_CONFIG) and the state of cosign and gh live in a mode-0700
directory in RAM that is removed at the end, also on SIGTERM. No secret is
on a command line or in the output. cosign signs with the public
transparency log (Rekor), and every verification requires its entry.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import io
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_version

DEFAULT_IMAGE = "ghcr.io/wiltaylor/terrarium"
DEFAULT_REPO = "wiltaylor/terrarium"
DEFAULT_GATE = "just ci::validate"
GATE_UID = 1001
REGISTRY = "ghcr.io"
KEY_PATH = "docker/release/cosign.pub"
GATE_DOCKERFILE = "docker/release-gate.Dockerfile"
# The image of the --version check of the host binaries on the release VM (the
# client base of docker/release.Dockerfile; the binaries are static musl).
VERSION_IMAGE = "debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2"
# The uid of the --version check: nobody.
VERSION_UID = 65534
PLATFORM = "linux/amd64"
BINARIES = ("terrarium-host", "terrarium-host-helper")
DIST = "/opt/terrarium/dist"

SECRETS = ("GHCR_TOKEN", "GH_TOKEN", "GITHUB_TOKEN", "COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD")
# Only these reach a command, plus what the step adds.
PASS_ENV = (
    "PATH", "LANG", "LC_ALL", "TZ", "HOME", "TMPDIR", "USER",
    "RUSTUP_HOME", "CARGO_HOME", "RUSTUP_TOOLCHAIN", "CARGO_BUILD_JOBS",
    "XDG_RUNTIME_DIR",
)
GITHUB_API = "https://api.github.com"
# The scopes of a classic token that the release needs (GHCR and the release).
TOKEN_SCOPES = ("repo", "write:packages")


class ReleaseError(Exception):
    pass


@dataclass
class Ctx:
    repo_dir: Path
    out: Path
    publish: bool
    image: str
    gh_repo: str
    gate: str
    run_gate: bool
    signing_config: str | None
    environ: dict[str, str]
    github_api: str = GITHUB_API
    summary: dict = field(default_factory=dict)
    secret_dir: Path | None = None

    def base_env(self, **extra: str) -> dict[str, str]:
        env = {k: v for k, v in self.environ.items() if k in PASS_ENV}
        env.update(extra)
        return env

    def save(self) -> None:
        self.out.mkdir(parents=True, exist_ok=True)
        (self.out / "summary.json").write_text(json.dumps(self.summary, indent=2, sort_keys=True) + "\n")


def say(msg: str) -> None:
    print(f"release: {msg}", flush=True)


def run(argv: list[str], env: dict[str, str], cwd: Path, *, stdout=None, stdin=None,
        text: bool = True) -> subprocess.CompletedProcess:
    """Run argv; a failure names the command (argv holds no secret) and its exit code."""
    say("$ " + shlex.join(argv))
    try:
        proc = subprocess.run(argv, env=env, cwd=cwd, stdout=stdout, stdin=stdin, text=text, check=False)
    except FileNotFoundError as e:
        raise ReleaseError(f"{argv[0]} is not installed: {e}") from e
    if proc.returncode != 0:
        raise ReleaseError(f"{argv[0]} exited {proc.returncode}")
    return proc


def git(ctx: Ctx, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ctx.repo_dir, env=ctx.base_env(), capture_output=True, text=True, check=True
    ).stdout


def ensure_tags(ctx: Ctx) -> None:
    """The release job clones its commit from a bundle with no tags and no
    remote (TER-754), so the version step would find no last tag and number
    the release from 0. Fetch the v* tags from the repo's public URL (or
    TERRARIUM_RELEASE_TAG_REMOTE, for tests) when the clone has none, and
    refuse to go on when there is still no v* tag."""
    if git(ctx, "tag", "--list", "v*").strip():
        return
    remote = ctx.environ.get("TERRARIUM_RELEASE_TAG_REMOTE") or f"https://github.com/{ctx.gh_repo}.git"
    proc = subprocess.run(
        ["git", "fetch", "-q", "--no-recurse-submodules", remote, "+refs/tags/v*:refs/tags/v*"],
        cwd=ctx.repo_dir, env=ctx.base_env(), capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        raise ReleaseError(f"the clone has no v* tag and the fetch of the tags from {remote} failed: "
                           + proc.stderr.strip()[-300:])
    if not git(ctx, "tag", "--list", "v*").strip():
        raise ReleaseError(f"no v* tag in the clone or on {remote}: refusing to number the release from 0")


# 1 ---------------------------------------------------------------------------
def step_version(ctx: Ctx) -> dict[str, str]:
    ensure_tags(ctx)
    values = release_version.calc(str(ctx.repo_dir))
    ctx.summary["version"] = values
    ctx.save()
    return values


# 2 ---------------------------------------------------------------------------
def step_check(ctx: Ctx, version: str) -> None:
    out_rel = None
    try:
        out_rel = ctx.out.resolve().relative_to(ctx.repo_dir.resolve()).as_posix()
    except ValueError:
        pass
    dirty = [
        line for line in git(ctx, "status", "--porcelain").splitlines()
        if not (out_rel and line[3:].rstrip("/").startswith(out_rel))
    ]
    if dirty:
        raise ReleaseError("the checkout is not clean: " + "; ".join(dirty[:5]))
    if git(ctx, "tag", "--list", f"v{version}").strip():
        raise ReleaseError(f"the tag v{version} exists")
    if not ctx.publish:
        return
    for tool in ("git", "docker", "crane", "cosign", "gh"):
        if not shutil.which(tool, path=ctx.environ.get("PATH")):
            raise ReleaseError(f"{tool} is not on PATH")
    if not (ctx.repo_dir / KEY_PATH).is_file():
        raise ReleaseError(f"{KEY_PATH} is missing: the updaters could not verify this release")
    cosign_key(ctx)
    token_info(ctx)
    view = subprocess.run(
        ["gh", "release", "view", f"v{version}", "--repo", ctx.gh_repo, "--json", "tagName"],
        cwd=ctx.repo_dir, env=gh_env(ctx), capture_output=True, text=True, check=False,
    )
    if view.returncode == 0:
        raise ReleaseError(f"the release v{version} exists on {ctx.gh_repo}")


def check_secrets(ctx: Ctx) -> None:
    missing = [k for k in ("GHCR_TOKEN", "GH_TOKEN", "COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD")
               if not ctx.environ.get(k)]
    if missing:
        raise ReleaseError("publish needs these secrets in the env: " + ", ".join(missing))


def cosign_key(ctx: Ctx) -> str:
    """The PEM of COSIGN_PRIVATE_KEY: as is, or decoded from one line of base64."""
    raw = ctx.environ.get("COSIGN_PRIVATE_KEY", "").strip()
    if raw.startswith("-----BEGIN "):
        return raw + "\n"
    try:
        pem = base64.b64decode("".join(raw.split()), validate=True).decode("ascii")
    except (ValueError, UnicodeDecodeError):
        raise ReleaseError("COSIGN_PRIVATE_KEY is neither a PEM nor base64 of a PEM") from None
    if not pem.lstrip().startswith("-----BEGIN "):
        raise ReleaseError("COSIGN_PRIVATE_KEY is base64, but not of a PEM") from None
    return pem


def token_info(ctx: Ctx) -> None:
    """Log the scopes and the expiry of the GitHub token (headers only, never the token)."""
    import urllib.error
    import urllib.request

    req = urllib.request.Request(
        f"{ctx.github_api}/user",
        headers={"Authorization": f"token {ctx.environ['GH_TOKEN']}", "User-Agent": "terrarium-release",
                 "Accept": "application/vnd.github+json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            headers = r.headers
    except urllib.error.HTTPError as e:
        raise ReleaseError(f"GET {ctx.github_api}/user with the GitHub token: HTTP {e.code}") from None
    except OSError as e:
        raise ReleaseError(f"GET {ctx.github_api}/user: {e.__class__.__name__}") from None
    scopes = [x.strip() for x in (headers.get("X-OAuth-Scopes") or "").split(",") if x.strip()]
    expiry = headers.get("github-authentication-token-expiration") or "none"
    ctx.summary["token"] = {"scopes": scopes, "expires": expiry}
    ctx.save()
    say(f"GitHub token: scopes {','.join(scopes) or '-'}, expires {expiry}")
    missing = [x for x in TOKEN_SCOPES if x not in scopes]
    if missing:
        raise ReleaseError("the GitHub token lacks the scopes " + ", ".join(missing))


# 3 ---------------------------------------------------------------------------
def _src_tar(src: Path, dest: Path) -> None:
    """A tar of the gate's clone as /src, owned by the gate user (docker cp -a keeps it)."""

    def owner(info: tarfile.TarInfo) -> tarfile.TarInfo:
        info.uid = info.gid = GATE_UID
        info.uname = info.gname = ""
        return info

    with tarfile.open(dest, "w") as tar:
        tar.add(src, arcname="src", filter=owner)


def step_gate(ctx: Ctx, commit: str) -> None:
    """The gate in a container on the release VM, as uid 1001, with no secret.

    The gate runs the code and the build scripts of the release: it runs
    where the image build runs, never next to the secrets of the client.
    """
    results = ctx.out / "results"
    src = ctx.out / "gate-src"
    for path in (src, results):
        shutil.rmtree(path, ignore_errors=True)
    run(["git", "clone", "--quiet", "--local", "--no-hardlinks", str(ctx.repo_dir), str(src)],
        ctx.base_env(), ctx.out)
    _src_tar(src, ctx.out / "gate-src.tar")
    image = f"terrarium-release-gate:{commit[:12]}"
    name = f"terrarium-release-gate-{commit[:12]}-{os.getpid()}"
    denv = docker_env(ctx)
    ctx.summary["gate"] = {"command": ctx.gate, "image": image, "uid": GATE_UID, "results": str(results)}
    ctx.save()
    run(["docker", "buildx", "build", "--load", "--file", GATE_DOCKERFILE, "--tag", image, "docker"],
        denv, ctx.repo_dir)
    created = False
    try:
        # Only these two variables: the container gets nothing of this env.
        run(["docker", "create", "--name", name, "--user", f"{GATE_UID}:{GATE_UID}", "--workdir", "/src",
             "--env", "RESULTS_DIR=/results", "--env", "HOME=/home/release", image, "sh", "-c", ctx.gate],
            denv, ctx.out, stdout=subprocess.DEVNULL)
        created = True
        with (ctx.out / "gate-src.tar").open("rb") as f:
            run(["docker", "cp", "--archive", "-", f"{name}:/"], denv, ctx.out, stdin=f, text=False)
        try:
            run(["docker", "start", "--attach", name], denv, ctx.out)
            passed = True
        except ReleaseError:
            passed = False
        # The results also of a red gate: the lead reads them (terrarium test push).
        with (ctx.out / "results.tar").open("wb") as f:
            run(["docker", "cp", f"{name}:/results", "-"], denv, ctx.out, stdout=f, text=False)
        extract_tar(ctx.out / "results.tar", ctx.out / "results-tmp", "the gate results")
        (ctx.out / "results-tmp" / "results").rename(results)
    finally:
        if created:
            subprocess.run(["docker", "rm", "--force", name], env=denv, cwd=ctx.out,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        for path in (ctx.out / "gate-src.tar", ctx.out / "results.tar"):
            path.unlink(missing_ok=True)
        shutil.rmtree(ctx.out / "results-tmp", ignore_errors=True)
    ctx.summary["gate"]["passed"] = passed
    ctx.save()
    if not passed:
        raise ReleaseError(f"the gate is red ({ctx.gate} in {image}): nothing is published")


# 4 ---------------------------------------------------------------------------
def step_stamp(ctx: Ctx, version: str) -> None:
    """[workspace.package] version and the lock entries of the workspace crates (no source)."""
    toml = ctx.repo_dir / "Cargo.toml"
    lines = toml.read_text().splitlines(keepends=True)
    section, done = "", False
    for i, line in enumerate(lines):
        if line.startswith("["):
            section = line.strip()
        elif section == "[workspace.package]" and line.startswith("version = "):
            lines[i] = f'version = "{version}"\n'
            done = True
    if not done:
        raise ReleaseError("stamp: Cargo.toml has no [workspace.package] version")
    toml.write_text("".join(lines))
    lock = ctx.repo_dir / "Cargo.lock"
    if lock.is_file():
        blocks = lock.read_text().split("\n[[package]]\n")
        for i, block in enumerate(blocks[1:], start=1):
            if "\nsource = " not in block:
                blocks[i] = "\n".join(
                    f'version = "{version}"' if ln.startswith("version = ") else ln
                    for ln in block.split("\n"))
        lock.write_text("\n[[package]]\n".join(blocks))
    ctx.summary["stamp"] = version
    ctx.save()


# 5 ---------------------------------------------------------------------------
def docker_env(ctx: Ctx) -> dict[str, str]:
    """The env of docker: the daemon of TERRARIUM_DOCKER_HOST, and no registry login."""
    env = ctx.base_env()
    host = ctx.environ.get("TERRARIUM_DOCKER_HOST") or ctx.environ.get("DOCKER_HOST")
    if host:
        env["DOCKER_HOST"] = host
    for k in ("BUILDX_BUILDER", "DOCKER_CONTEXT"):
        if ctx.environ.get(k):
            env[k] = ctx.environ[k]
    return env


def build_argv(ctx: Ctx, version: str, commit: str) -> list[str]:
    source = f"https://github.com/{ctx.gh_repo}"
    return [
        "docker", "buildx", "build",
        "--file", "docker/Dockerfile",
        "--target", "runtime",
        "--platform", PLATFORM,
        # `--version` of every binary: <version>+<commit> (crates/terrarium-version).
        # The commit: the stamp leaves a dirty tree.
        "--build-arg", f"TERRARIUM_GIT_REF={commit}",
        "--build-arg", f"TERRARIUM_VERSION={version}",
        "--build-arg", f"TERRARIUM_RELEASE_PUBKEY={(ctx.repo_dir / KEY_PATH).read_text().strip()}",
        "--label", f"org.opencontainers.image.version={version}",
        "--label", f"org.opencontainers.image.revision={commit}",
        "--label", f"org.opencontainers.image.source={source}",
        "--provenance=mode=max",
        "--sbom=true",
        "--metadata-file", str(ctx.out / "build-metadata.json"),
        # The export comes back to this side: the VM pushes nothing and gets no login.
        "--output", f"type=oci,dest={ctx.out / 'image.tar'}",
        ".",
    ]


def extract_layout(archive: Path, dest: Path) -> None:
    """Unpack the OCI layout tar of the build: plain files and dirs inside `dest` only."""
    extract_tar(archive, dest, "the image export")


def extract_tar(archive: Path, dest: Path, what: str) -> None:
    """Unpack plain files and dirs inside `dest` only (Python 3.11 has no tar filter)."""
    dest.mkdir(parents=True)
    with tarfile.open(archive) as tar:
        for m in tar:
            rel = PurePosixPath(m.name)
            if rel.is_absolute() or ".." in rel.parts:
                raise ReleaseError(f"{what} has the path {m.name!r}")
            target = dest.joinpath(*rel.parts) if rel.parts not in ((), (".",)) else dest
            if m.isdir():
                target.mkdir(parents=True, exist_ok=True)
            elif m.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                src = tar.extractfile(m)
                assert src is not None
                with src, target.open("wb") as f:
                    shutil.copyfileobj(src, f)
            else:
                raise ReleaseError(f"{what} has a link or device {m.name!r}")


def step_image(ctx: Ctx, version: str, commit: str) -> str:
    run(build_argv(ctx, version, commit), docker_env(ctx), ctx.repo_dir)
    meta = json.loads((ctx.out / "build-metadata.json").read_text())
    digest = meta.get("containerimage.digest", "")
    if not digest.startswith("sha256:"):
        raise ReleaseError(f"the build metadata has no image digest: {sorted(meta)}")
    layout = ctx.out / "image"
    shutil.rmtree(layout, ignore_errors=True)
    extract_layout(ctx.out / "image.tar", layout)
    top = json.loads((layout / "index.json").read_text()).get("manifests", [])
    if [m.get("digest") for m in top] != [digest]:
        raise ReleaseError(f"the OCI layout holds {[m.get('digest') for m in top]}, the build says {digest}")
    (ctx.out / "image.tar").unlink()
    ctx.summary["image"] = {"name": ctx.image, "digest": digest, "pushed": False}
    ctx.save()
    return digest


# 6 ---------------------------------------------------------------------------
def _blob(layout: Path, digest: str) -> Path:
    algo, _, hexd = digest.partition(":")
    p = layout / "blobs" / algo / hexd
    if not p.is_file():
        raise ReleaseError(f"the image has no blob {digest}")
    return p


def _json_blob(layout: Path, digest: str) -> dict:
    data = _blob(layout, digest).read_bytes()
    if "sha256:" + hashlib.sha256(data).hexdigest() != digest:
        raise ReleaseError(f"the blob {digest} does not match its digest")
    return json.loads(data)


def image_layers(layout: Path, platform: str = PLATFORM) -> list[str]:
    """The layer digests, in order, of the `platform` image in an OCI layout."""
    os_, arch = platform.split("/")
    index = json.loads((layout / "index.json").read_text())
    todo = list(index.get("manifests", []))
    while todo:
        desc = todo.pop(0)
        mt = desc.get("mediaType", "")
        if mt.endswith(("image.index.v1+json", "manifest.list.v2+json")):
            todo[:0] = _json_blob(layout, desc["digest"]).get("manifests", [])
            continue
        plat = desc.get("platform")
        if plat and (plat.get("os"), plat.get("architecture")) != (os_, arch):
            continue
        manifest = _json_blob(layout, desc["digest"])
        if "layers" not in manifest:
            continue
        if not plat:
            config = _json_blob(layout, manifest["config"]["digest"])
            if (config.get("os"), config.get("architecture")) != (os_, arch):
                continue
        return [layer["digest"] for layer in manifest["layers"]]
    raise ReleaseError(f"the image has no {platform} manifest")


def _open_layer(path: Path) -> tarfile.TarFile:
    with path.open("rb") as f:
        magic = f.read(4)
    if magic[:2] == b"\x1f\x8b":
        return tarfile.open(fileobj=gzip.open(path), mode="r|")
    if magic == b"\x28\xb5\x2f\xfd":
        raise ReleaseError("a zstd layer: build with gzip layers (the BuildKit default)")
    return tarfile.open(path, mode="r|")


def _norm(name: str) -> str:
    return PurePosixPath("/" + name.lstrip("./")).as_posix()


def image_files(layout: Path, wanted: list[str], platform: str = PLATFORM) -> dict[str, bytes]:
    """The content of the files `wanted` (absolute paths, symlinks followed) in an image."""
    layers = image_layers(layout, platform)
    # Pass 1: the type and link target of each path after all layers.
    meta: dict[str, tuple[str, str]] = {}
    for digest in layers:
        with _open_layer(_blob(layout, digest)) as tar:
            for m in tar:
                name = _norm(m.name)
                base = PurePosixPath(name)
                if base.name == ".wh..wh..opq":
                    parent = str(base.parent).rstrip("/") + "/"
                    for k in [k for k in meta if k.startswith(parent)]:
                        del meta[k]
                    continue
                if base.name.startswith(".wh."):
                    gone = str(base.parent / base.name[4:])
                    for k in [k for k in meta if k == gone or k.startswith(gone + "/")]:
                        del meta[k]
                    continue
                if m.issym():
                    meta[name] = ("link", m.linkname)
                elif m.isfile():
                    meta[name] = ("file", digest)
                elif m.isdir():
                    meta[name] = ("dir", "")
                else:
                    meta[name] = ("other", "")

    def resolve(path: str, depth: int = 0) -> str:
        if depth > 40:
            raise ReleaseError(f"too many symlinks at {path}")
        parts = PurePosixPath(path).parts[1:]
        cur = PurePosixPath("/")
        for i, part in enumerate(parts):
            nxt = cur / part
            kind = meta.get(str(nxt))
            if kind and kind[0] == "link":
                target = PurePosixPath(kind[1])
                target = target if target.is_absolute() else cur / target
                rest = PurePosixPath(*parts[i + 1:]) if i + 1 < len(parts) else None
                full = str(target / rest) if rest else str(target)
                return resolve(os.path.normpath(full), depth + 1)
            cur = nxt
        return str(cur)

    real = {w: resolve(w) for w in wanted}
    for w, r in real.items():
        if meta.get(r, ("", ""))[0] != "file":
            raise ReleaseError(f"the image has no file {w}")
    # Pass 2: the content from the layer that wrote the path last.
    out: dict[str, bytes] = {}
    for digest in {meta[r][1] for r in real.values()}:
        names = {r for r in real.values() if meta[r][1] == digest}
        with _open_layer(_blob(layout, digest)) as tar:
            for m in tar:
                name = _norm(m.name)
                if name in names and m.isfile():
                    f = tar.extractfile(m)
                    assert f is not None
                    out[name] = f.read()
    return {w: out[r] for w, r in real.items()}


def version_on_vm(ctx: Ctx, binary: str, data: bytes) -> str:
    """`<binary> --version` in a container on the release VM: the client runs no code of the release."""
    name = f"terrarium-release-version-{binary}-{os.getpid()}"
    denv = docker_env(ctx)
    archive = ctx.out / f"{binary}.version.tar"
    with tarfile.open(archive, "w") as tar:
        d = tarfile.TarInfo("opt/check")
        d.type, d.mode = tarfile.DIRTYPE, 0o755
        tar.addfile(d)
        f = tarfile.TarInfo(f"opt/check/{binary}")
        f.size, f.mode = len(data), 0o755
        tar.addfile(f, io.BytesIO(data))
    created = False
    try:
        run(["docker", "create", "--name", name, "--network", "none", "--user", f"{VERSION_UID}:{VERSION_UID}",
             "--cap-drop", "ALL", "--security-opt", "no-new-privileges", VERSION_IMAGE,
             f"/opt/check/{binary}", "--version"], denv, ctx.out, stdout=subprocess.DEVNULL)
        created = True
        with archive.open("rb") as fh:
            run(["docker", "cp", "-", f"{name}:/"], denv, ctx.out, stdin=fh, text=False)
        return run(["docker", "start", "--attach", name], denv, ctx.out, stdout=subprocess.PIPE).stdout
    finally:
        if created:
            subprocess.run(["docker", "rm", "--force", name], env=denv, cwd=ctx.out,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        archive.unlink(missing_ok=True)


def step_extract(ctx: Ctx, version: str) -> list[Path]:
    layout = ctx.out / "image"
    wanted = []
    for b in BINARIES:
        wanted += [f"{DIST}/{b}-x86_64-linux", f"{DIST}/{b}-x86_64-linux.sha256"]
    files = image_files(layout, wanted)
    dist = ctx.out / "dist"
    dist.mkdir(parents=True, exist_ok=True)
    sums = []
    assets = []
    for b in BINARIES:
        data = files[f"{DIST}/{b}-x86_64-linux"]
        want = files[f"{DIST}/{b}-x86_64-linux.sha256"].decode().split()[0]
        got = hashlib.sha256(data).hexdigest()
        # The daemon serves the image's .sha256 (self-update): it must be the asset's hash.
        if want != got:
            raise ReleaseError(f"the image's {b} is {got}, its .sha256 says {want}")
        asset = dist / f"{b}-{version}-linux-x86_64"
        asset.write_bytes(data)
        asset.chmod(0o755)
        said = version_on_vm(ctx, b, data)
        print(said, end="")
        if not said.startswith(f"{b} {version}+"):
            raise ReleaseError(f"{asset.name} --version says {said.strip()!r}, not {b} {version}+")
        sums.append(f"{got}  {asset.name}\n")
        assets.append(asset)
    (dist / "SHA256SUMS").write_text("".join(sums))
    ctx.summary["assets"] = {a.name: hashlib.sha256(a.read_bytes()).hexdigest() for a in assets}
    ctx.save()
    return [*assets, dist / "SHA256SUMS"]


# 7 ---------------------------------------------------------------------------
def tool_env(ctx: Ctx, *, registry: bool) -> dict[str, str]:
    """The env of crane, cosign and gh: their state in the RAM directory, and the
    registry login (DOCKER_CONFIG) only for the tools that talk to the registry."""
    env = ctx.base_env()
    if ctx.secret_dir:
        d = ctx.secret_dir
        env.update(GH_CONFIG_DIR=str(d / "gh"), TUF_ROOT=str(d / "sigstore"),
                   XDG_CONFIG_HOME=str(d / "config"), XDG_CACHE_HOME=str(d / "cache"),
                   XDG_DATA_HOME=str(d / "data"))
        if registry:
            env["DOCKER_CONFIG"] = str(d)
    return env


def crane_env(ctx: Ctx) -> dict[str, str]:
    return tool_env(ctx, registry=True)


def step_push(ctx: Ctx, version: str, prerelease: bool, digest: str) -> None:
    """Push the layout that the checks read: the registry gets those bytes."""
    channel = "pre" if prerelease else "latest"
    ref = f"{ctx.image}:{version}"
    proc = run(["crane", "push", str(ctx.out / "image"), ref], crane_env(ctx), ctx.repo_dir,
               stdout=subprocess.PIPE)
    pushed = proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else ""
    if pushed != f"{ctx.image}@{digest}":
        raise ReleaseError(f"crane pushed {pushed!r}, not {ctx.image}@{digest}")
    run(["crane", "tag", f"{ctx.image}@{digest}", channel], crane_env(ctx), ctx.repo_dir)
    ctx.summary["image"].update(pushed=True, tags=[ref, f"{ctx.image}:{channel}"])
    ctx.save()


# 8 ---------------------------------------------------------------------------
def cosign_env(ctx: Ctx, *, key: bool, registry: bool) -> dict[str, str]:
    """registry: the image commands (sign, verify); the blob commands get no login."""
    env = tool_env(ctx, registry=registry)
    if key:
        env["COSIGN_PRIVATE_KEY"] = cosign_key(ctx)
        env["COSIGN_PASSWORD"] = ctx.environ["COSIGN_PASSWORD"]
    return env


def _sign_args(ctx: Ctx) -> tuple[list[str], list[str]]:
    extra = ["--signing-config", ctx.signing_config] if ctx.signing_config else []
    # Only with a test signing config, which has no transparency log.
    no_tlog = ["--insecure-ignore-tlog"] if ctx.signing_config else []
    return extra, no_tlog


def sign_blobs(ctx: Ctx, files: list[Path]) -> list[Path]:
    """Sign each file into <file>.sigstore.json, then verify it with the checkout's key."""
    extra, no_tlog = _sign_args(ctx)
    pub = str(ctx.repo_dir / KEY_PATH)
    bundles = []
    for f in files:
        bundle = f.with_name(f.name + ".sigstore.json")
        run(["cosign", "sign-blob", "--yes", "--key", "env://COSIGN_PRIVATE_KEY", *extra,
             "--bundle", str(bundle), str(f)], cosign_env(ctx, key=True, registry=False), ctx.repo_dir)
        bundles.append(bundle)
    for f, bundle in zip(files, bundles):
        run(["cosign", "verify-blob", "--key", pub, *no_tlog, "--bundle", str(bundle), str(f)],
            cosign_env(ctx, key=False, registry=False), ctx.repo_dir)
    return bundles


def step_sign(ctx: Ctx, digest: str, files: list[Path]) -> list[Path]:
    ref = f"{ctx.image}@{digest}"
    extra, no_tlog = _sign_args(ctx)
    pub = str(ctx.repo_dir / KEY_PATH)
    run(["cosign", "sign", "--yes", "--key", "env://COSIGN_PRIVATE_KEY", *extra, ref],
        cosign_env(ctx, key=True, registry=True), ctx.repo_dir)
    # The key of the checkout (the one that the image ships) must accept the signature.
    run(["cosign", "verify", "--key", pub, *no_tlog, ref], cosign_env(ctx, key=False, registry=True), ctx.repo_dir,
        stdout=subprocess.DEVNULL)
    bundles = sign_blobs(ctx, files)
    ctx.summary["signed"] = {"image": ref, "blobs": [b.name for b in bundles]}
    ctx.save()
    return bundles


# 9 ---------------------------------------------------------------------------
def gh_env(ctx: Ctx) -> dict[str, str]:
    env = tool_env(ctx, registry=False)
    env.update(GH_TOKEN=ctx.environ.get("GH_TOKEN", ""), GH_PROMPT_DISABLED="1", GH_NO_UPDATE_NOTIFIER="1")
    return env


def notes(ctx: Ctx, version: str, digest: str) -> str:
    image = ctx.image
    gate = ctx.summary.get("gate", {})
    cut = (f"Tested at cut: {gate['runs']}\n\n"
           if gate.get("reason") == "tested-at-cut" else "")
    return cut + f"""## Image

`{image}:{version}` (digest `{digest}`)

Verify it with the release public key (docs/release.md, `docker/release/cosign.pub`):

```sh
cosign verify --key cosign.pub {image}@{digest}
cosign verify-blob --key cosign.pub --bundle terrarium-host-{version}-linux-x86_64.sigstore.json terrarium-host-{version}-linux-x86_64
cosign verify-blob --key cosign.pub --bundle terrarium-host-helper-{version}-linux-x86_64.sigstore.json terrarium-host-helper-{version}-linux-x86_64
cosign verify-blob --key cosign.pub --bundle SHA256SUMS.sigstore.json SHA256SUMS
sha256sum -c SHA256SUMS
```
"""


def step_release(ctx: Ctx, version: str, prerelease: bool, commit: str, digest: str,
                 files: list[Path]) -> None:
    note_file = ctx.out / "notes.md"
    note_file.write_text(notes(ctx, version, digest))
    tag = f"v{version}"
    argv = ["gh", "release", "create", tag, "--repo", ctx.gh_repo, "--target", commit,
            "--title", tag, "--notes-file", str(note_file), "--generate-notes"]
    argv.append("--prerelease" if prerelease else "--latest")
    argv += [str(f) for f in files]
    run(argv, gh_env(ctx), ctx.repo_dir)
    ctx.summary["release"] = {"tag": tag, "url": f"https://github.com/{ctx.gh_repo}/releases/tag/{tag}"}
    ctx.save()


# ---------------------------------------------------------------------------
def ram_dir(environ: dict[str, str]) -> str:
    """A writable tmpfs for the registry login: XDG_RUNTIME_DIR or /dev/shm, never a disk."""
    for d in (environ.get("XDG_RUNTIME_DIR"), "/dev/shm"):
        if d and os.path.isdir(d) and os.access(d, os.W_OK) and _is_tmpfs(d):
            return d
    raise ReleaseError("no RAM directory for the registry login (XDG_RUNTIME_DIR or /dev/shm on tmpfs)")


def _is_tmpfs(path: str) -> bool:
    try:
        mounts = Path("/proc/self/mounts").read_text().splitlines()
    except OSError:
        return False
    real = os.path.realpath(path)
    best, fstype = "", ""
    for line in mounts:
        parts = line.split()
        if len(parts) > 2:
            mnt = parts[1].replace("\\040", " ")
            if (real == mnt or real.startswith(mnt.rstrip("/") + "/")) and len(mnt) > len(best):
                best, fstype = mnt, parts[2]
    return fstype in ("tmpfs", "ramfs")


def registry_login(ctx: Ctx) -> None:
    """The RAM directory of the registry login (DOCKER_CONFIG) and of the state of cosign and gh."""
    ctx.secret_dir = Path(tempfile.mkdtemp(prefix="terrarium-release-", dir=ram_dir(ctx.environ)))
    ctx.secret_dir.chmod(0o700)
    for sub in ("gh", "sigstore", "config", "cache", "data"):
        (ctx.secret_dir / sub).mkdir(mode=0o700)
    user = ctx.environ.get("GHCR_USER", "wiltaylor")
    auth = base64.b64encode(f"{user}:{ctx.environ['GHCR_TOKEN']}".encode()).decode()
    cfg = ctx.secret_dir / "config.json"
    fd = os.open(cfg, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump({"auths": {REGISTRY: {"auth": auth}}}, f)


def release(ctx: Ctx) -> int:
    values = step_version(ctx)
    if values.get("do_release") != "true":
        say("no release: HEAD has no release: or pre-release: trailer")
        return 0
    version, prerelease = values["version"], values["prerelease"] == "true"
    commit = git(ctx, "rev-parse", "HEAD").strip()
    ctx.summary.update(commit=commit, mode="publish" if ctx.publish else "dry-run")
    say(f"{ctx.summary['mode']} {version} (prerelease={prerelease}) of {commit}")
    try:
        if ctx.publish:
            check_secrets(ctx)
            registry_login(ctx)
        step_check(ctx, version)
        if values.get("tested_at_cut") == "true":
            ctx.summary["gate"] = {
                "skipped": True, "reason": "tested-at-cut",
                "runs": release_version.trailer(str(ctx.repo_dir), "tested-at-cut"),
            }
            ctx.save()
        elif ctx.run_gate:
            step_gate(ctx, commit)
        else:
            ctx.summary["gate"] = {"skipped": True}
        step_stamp(ctx, version)
        digest = step_image(ctx, version, commit)
        files = step_extract(ctx, version)
        if ctx.publish:
            step_push(ctx, version, prerelease, digest)
            bundles = step_sign(ctx, digest, files)
            step_release(ctx, version, prerelease, commit, digest, files + bundles)
    finally:
        if ctx.secret_dir:
            shutil.rmtree(ctx.secret_dir, ignore_errors=True)
            ctx.secret_dir = None
    ctx.summary["done"] = True
    ctx.save()
    say(f"done: {version} {ctx.image}@{digest}")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("mode", choices=("publish", "dry-run"))
    ap.add_argument("--out", default="release-out", help="the output directory (default ./release-out)")
    ap.add_argument("--image", default=DEFAULT_IMAGE)
    ap.add_argument("--repo", default=DEFAULT_REPO, help="the GitHub repo of the release")
    ap.add_argument("--gate", default=DEFAULT_GATE, help="the release gate command")
    ap.add_argument("--no-gate", action="store_true", help="dry run only: skip the gate")
    ap.add_argument("--signing-config", help="tests only: a cosign signing config (no public log)")
    ap.add_argument("--github-api", default=GITHUB_API, help="tests only: the GitHub API URL")
    a = ap.parse_args(argv)
    publish = a.mode == "publish"
    if publish and a.no_gate:
        ap.error("--no-gate is for the dry run only: a release passes the gate")
    testing = os.environ.get("TERRARIUM_RELEASE_TEST") == "1"
    if publish and a.gate != DEFAULT_GATE and not testing:
        ap.error(f"--gate is for the tests only (TERRARIUM_RELEASE_TEST=1): a release passes {DEFAULT_GATE}")
    if publish and a.signing_config and not testing:
        ap.error("--signing-config is for the tests only (TERRARIUM_RELEASE_TEST=1)")
    if a.github_api != GITHUB_API and not testing:
        ap.error("--github-api is for the tests only (TERRARIUM_RELEASE_TEST=1)")
    repo_dir = Path(git_toplevel())
    out = Path(a.out)
    out = out if out.is_absolute() else Path.cwd() / out
    ctx = Ctx(
        repo_dir=repo_dir, out=out, publish=publish, image=a.image, gh_repo=a.repo,
        gate=a.gate, run_gate=not a.no_gate, signing_config=a.signing_config,
        environ=dict(os.environ), github_api=a.github_api,
    )
    # A killed run (SIGTERM, SIGHUP, SIGINT) still removes the registry login:
    # the signal becomes an exception, and the `finally` of release() runs.
    def stop(signum, _frame):
        raise ReleaseError(f"stopped by signal {signum}")

    for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
        signal.signal(sig, stop)
    try:
        return release(ctx)
    except (ReleaseError, release_version.VersionError) as e:
        ctx.summary["error"] = str(e)
        ctx.save()
        print(f"release: failed: {e}", file=sys.stderr)
        return 1


def git_toplevel() -> str:
    proc = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise SystemExit("release: run it in a git checkout")
    return proc.stdout.strip()


if __name__ == "__main__":
    sys.exit(main())
