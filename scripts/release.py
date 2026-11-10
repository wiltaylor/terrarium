"""The local release build: test, build, sign, and publish a release (TER-613).

It replaces the version, image and release jobs of the GitHub CI
(docs/release.md). The lead runs it in the release build container on a
clean checkout of the commit to release, as a named run:

    just release::dry-run          # all but the push, the signatures and the release
    just release::publish          # the release

Steps (each one writes its part of <out>/summary.json):

1. version  scripts/release_version.py: the trailers of HEAD. No trailer:
            no release, exit 0.
2. check    a clean tree; publish: the secrets in the env, the tools on
            PATH, docker/release/cosign.pub, and no tag or release
            v<version> yet.
3. gate     `just ci::validate` (or --gate) as a non-root user: as uid
            1001 with setpriv when the script runs as root. The env of the
            gate has no secret. RESULTS_DIR is <out>/results. Red: stop,
            nothing is published.
4. stamp    the version into Cargo.toml and Cargo.lock.
5. image    BuildKit (buildctl) builds the runtime stage of
            docker/Dockerfile for linux/amd64 with the OCI labels, an SBOM
            and provenance (mode=max). Publish pushes <image>:<version> and
            :pre or :latest. Dry run writes an OCI layout to <out>/image.
6. extract  the host agent and the host helper out of the image (publish:
            the pushed digest, pulled again with crane), with the two
            checks of the old CI: the SHA-256 is the .sha256 of the image,
            and `--version` is the release version. Writes SHA256SUMS.
7. sign     publish: cosign signs the image digest, both binaries and
            SHA256SUMS with the release key (env://COSIGN_PRIVATE_KEY, no
            key file), then verifies each signature with
            docker/release/cosign.pub of the checkout.
8. release  publish: `gh release create v<version> --target <commit>`
            (the tag comes with the release) with the assets and bundles.

Secrets, only in the env of this process: GHCR_TOKEN (GHCR_USER, default
wiltaylor), GH_TOKEN, COSIGN_PRIVATE_KEY and COSIGN_PASSWORD. Each command
gets only the secrets that it needs; the gate and the build get none, the
registry login is a mode-0600 file in a private temporary directory that
is removed at the end, and no secret is on a command line or in the output.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import json
import os
import shlex
import shutil
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
STAMP = ".github/stamp-version.sh"
PLATFORM = "linux/amd64"
BINARIES = ("terrarium-host", "terrarium-host-helper")
DIST = "/opt/terrarium/dist"

SECRETS = ("GHCR_TOKEN", "GH_TOKEN", "GITHUB_TOKEN", "COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD")
# Only these reach a command, plus what the step adds.
PASS_ENV = (
    "PATH", "LANG", "LC_ALL", "TZ", "HOME", "TMPDIR", "USER",
    "RUSTUP_HOME", "CARGO_HOME", "RUSTUP_TOOLCHAIN", "CARGO_BUILD_JOBS",
    "XDG_RUNTIME_DIR", "BUILDKIT_HOST", "BUILDKITD_FLAGS",
)


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
    buildctl: str
    environ: dict[str, str]
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


def run(argv: list[str], env: dict[str, str], cwd: Path, *, stdout=None) -> subprocess.CompletedProcess:
    """Run argv; a failure names the command (argv holds no secret) and its exit code."""
    say("$ " + shlex.join(argv))
    try:
        proc = subprocess.run(argv, env=env, cwd=cwd, stdout=stdout, text=True, check=False)
    except FileNotFoundError as e:
        raise ReleaseError(f"{argv[0]} is not installed: {e}") from e
    if proc.returncode != 0:
        raise ReleaseError(f"{argv[0]} exited {proc.returncode}")
    return proc


def git(ctx: Ctx, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ctx.repo_dir, env=ctx.base_env(), capture_output=True, text=True, check=True
    ).stdout


# 1 ---------------------------------------------------------------------------
def step_version(ctx: Ctx) -> dict[str, str]:
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
    missing = [k for k in ("GHCR_TOKEN", "GH_TOKEN", "COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD")
               if not ctx.environ.get(k)]
    if missing:
        raise ReleaseError("publish needs these secrets in the env: " + ", ".join(missing))
    for tool in (ctx.buildctl, "crane", "cosign", "gh"):
        if not shutil.which(tool, path=ctx.environ.get("PATH")):
            raise ReleaseError(f"{tool} is not on PATH")
    if not (ctx.repo_dir / KEY_PATH).is_file():
        raise ReleaseError(f"{KEY_PATH} is missing: the updaters could not verify this release")
    view = subprocess.run(
        ["gh", "release", "view", f"v{version}", "--repo", ctx.gh_repo, "--json", "tagName"],
        cwd=ctx.repo_dir, env=gh_env(ctx), capture_output=True, text=True, check=False,
    )
    if view.returncode == 0:
        raise ReleaseError(f"the release v{version} exists on {ctx.gh_repo}")


# 3 ---------------------------------------------------------------------------
def step_gate(ctx: Ctx) -> None:
    results = ctx.out / "results"
    home = ctx.out / "gate-home"
    results.mkdir(parents=True, exist_ok=True)
    home.mkdir(parents=True, exist_ok=True)
    argv = shlex.split(ctx.gate)
    uid = os.geteuid()
    if uid == 0:
        # The release CI ran as uid 1001; root hides permission bugs (TER-579).
        for p in (ctx.repo_dir, ctx.out):
            run(["chown", "-R", f"{GATE_UID}:{GATE_UID}", str(p)], ctx.base_env(), ctx.repo_dir)
        argv = ["setpriv", f"--reuid={GATE_UID}", f"--regid={GATE_UID}", "--clear-groups", "--", *argv]
        uid = GATE_UID
    env = ctx.base_env(HOME=str(home), RESULTS_DIR=str(results))
    ctx.summary["gate"] = {"command": ctx.gate, "uid": uid, "results": str(results)}
    ctx.save()
    try:
        run(argv, env, ctx.repo_dir)
    except ReleaseError as e:
        ctx.summary["gate"]["passed"] = False
        ctx.save()
        raise ReleaseError(f"the gate is red ({e}): nothing is published") from e
    ctx.summary["gate"]["passed"] = True
    ctx.save()


# 4 ---------------------------------------------------------------------------
def step_stamp(ctx: Ctx, version: str) -> None:
    run(["bash", str(ctx.repo_dir / STAMP), version], ctx.base_env(), ctx.repo_dir)


# 5 ---------------------------------------------------------------------------
def build_argv(ctx: Ctx, version: str, prerelease: bool, commit: str) -> list[str]:
    channel = "pre" if prerelease else "latest"
    source = f"https://github.com/{ctx.gh_repo}"
    if ctx.publish:
        output = f'type=image,"name={ctx.image}:{version},{ctx.image}:{channel}",push=true'
    else:
        output = f"type=oci,dest={ctx.out / 'image'},tar=false"
    argv = [
        ctx.buildctl, "build",
        "--frontend", "dockerfile.v0",
        "--local", "context=.",
        "--local", "dockerfile=docker",
        "--opt", "filename=Dockerfile",
        "--opt", "target=runtime",
        "--opt", f"platform={PLATFORM}",
        # `--version` of every binary: <version>+<commit> (crates/terrarium-version).
        # The commit: the stamp leaves a dirty tree.
        "--opt", f"build-arg:TERRARIUM_GIT_REF={commit}",
        "--opt", f"build-arg:TERRARIUM_VERSION={version}",
        "--opt", f"label:org.opencontainers.image.version={version}",
        "--opt", f"label:org.opencontainers.image.revision={commit}",
        "--opt", f"label:org.opencontainers.image.source={source}",
        "--opt", "attest:sbom=",
        "--opt", "attest:provenance=mode=max",
        "--metadata-file", str(ctx.out / "build-metadata.json"),
        "--output", output,
    ]
    return argv


def step_image(ctx: Ctx, version: str, prerelease: bool, commit: str) -> str:
    env = ctx.base_env()
    if ctx.secret_dir:
        env["DOCKER_CONFIG"] = str(ctx.secret_dir)
    run(build_argv(ctx, version, prerelease, commit), env, ctx.repo_dir)
    meta = json.loads((ctx.out / "build-metadata.json").read_text())
    digest = meta.get("containerimage.digest", "")
    if not digest.startswith("sha256:"):
        raise ReleaseError(f"the build metadata has no image digest: {sorted(meta)}")
    channel = "pre" if prerelease else "latest"
    ctx.summary["image"] = {
        "name": ctx.image,
        "digest": digest,
        "tags": [f"{ctx.image}:{version}", f"{ctx.image}:{channel}"],
        "pushed": ctx.publish,
    }
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


def step_extract(ctx: Ctx, version: str, digest: str) -> list[Path]:
    if ctx.publish:
        layout = ctx.out / "pulled"
        env = ctx.base_env()
        if ctx.secret_dir:
            env["DOCKER_CONFIG"] = str(ctx.secret_dir)
        run(["crane", "pull", "--format=oci", f"--platform={PLATFORM}",
             f"{ctx.image}@{digest}", str(layout)], env, ctx.repo_dir)
    else:
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
        proc = run([str(asset), "--version"], ctx.base_env(), dist, stdout=subprocess.PIPE)
        print(proc.stdout, end="")
        if not proc.stdout.startswith(f"{b} {version}+"):
            raise ReleaseError(f"{asset.name} --version says {proc.stdout.strip()!r}, not {b} {version}+")
        sums.append(f"{got}  {asset.name}\n")
        assets.append(asset)
    (dist / "SHA256SUMS").write_text("".join(sums))
    ctx.summary["assets"] = {a.name: hashlib.sha256(a.read_bytes()).hexdigest() for a in assets}
    ctx.save()
    return [*assets, dist / "SHA256SUMS"]


# 7 ---------------------------------------------------------------------------
def cosign_env(ctx: Ctx, *, key: bool) -> dict[str, str]:
    env = ctx.base_env()
    if ctx.secret_dir:
        env["DOCKER_CONFIG"] = str(ctx.secret_dir)
    if key:
        env["COSIGN_PRIVATE_KEY"] = ctx.environ["COSIGN_PRIVATE_KEY"]
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
             "--bundle", str(bundle), str(f)], cosign_env(ctx, key=True), ctx.repo_dir)
        bundles.append(bundle)
    for f, bundle in zip(files, bundles):
        run(["cosign", "verify-blob", "--key", pub, *no_tlog, "--bundle", str(bundle), str(f)],
            cosign_env(ctx, key=False), ctx.repo_dir)
    return bundles


def step_sign(ctx: Ctx, digest: str, files: list[Path]) -> list[Path]:
    ref = f"{ctx.image}@{digest}"
    extra, no_tlog = _sign_args(ctx)
    pub = str(ctx.repo_dir / KEY_PATH)
    run(["cosign", "sign", "--yes", "--key", "env://COSIGN_PRIVATE_KEY", *extra, ref],
        cosign_env(ctx, key=True), ctx.repo_dir)
    # The key of the checkout (the one that the image ships) must accept the signature.
    run(["cosign", "verify", "--key", pub, *no_tlog, ref], cosign_env(ctx, key=False), ctx.repo_dir,
        stdout=subprocess.DEVNULL)
    bundles = sign_blobs(ctx, files)
    ctx.summary["signed"] = {"image": ref, "blobs": [b.name for b in bundles]}
    ctx.save()
    return bundles


# 8 ---------------------------------------------------------------------------
def gh_env(ctx: Ctx) -> dict[str, str]:
    return ctx.base_env(
        GH_TOKEN=ctx.environ.get("GH_TOKEN", ""), GH_PROMPT_DISABLED="1", GH_NO_UPDATE_NOTIFIER="1"
    )


def notes(ctx: Ctx, version: str, digest: str) -> str:
    image = ctx.image
    return f"""## Image

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
def registry_login(ctx: Ctx) -> None:
    """A private DOCKER_CONFIG for buildctl and crane; the token is in no argv."""
    base = "/dev/shm" if os.path.isdir("/dev/shm") and os.access("/dev/shm", os.W_OK) else None
    ctx.secret_dir = Path(tempfile.mkdtemp(prefix="terrarium-release-", dir=base))
    ctx.secret_dir.chmod(0o700)
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
    step_check(ctx, version)
    if ctx.run_gate:
        step_gate(ctx)
    else:
        ctx.summary["gate"] = {"skipped": True}
    step_stamp(ctx, version)
    try:
        if ctx.publish:
            registry_login(ctx)
        digest = step_image(ctx, version, prerelease, commit)
        files = step_extract(ctx, version, digest)
        if ctx.publish:
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
    a = ap.parse_args(argv)
    publish = a.mode == "publish"
    if publish and a.no_gate:
        ap.error("--no-gate is for the dry run only: a release passes the gate")
    if publish and a.signing_config and os.environ.get("TERRARIUM_RELEASE_TEST") != "1":
        ap.error("--signing-config is for the tests only (TERRARIUM_RELEASE_TEST=1)")
    repo_dir = Path(git_toplevel())
    out = Path(a.out)
    out = out if out.is_absolute() else Path.cwd() / out
    ctx = Ctx(
        repo_dir=repo_dir, out=out, publish=publish, image=a.image, gh_repo=a.repo,
        gate=a.gate, run_gate=not a.no_gate, signing_config=a.signing_config,
        buildctl=os.environ.get("BUILDCTL", "buildctl-daemonless.sh"), environ=dict(os.environ),
    )
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
