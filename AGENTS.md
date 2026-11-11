# AGENTS.md

Instructions for agents that work in this repo.

## Validation

The worker validates its card before review. The merge queue runs the full
suite once per batch. There is no `validating` column and no `terrarium card
validate` for card work (operator, 2026-11-07).

### The card check: the affected tests

Run the checks that your changed files can reach, on a clean checkout of your
pushed commit:

```sh
just ci::affected            # print the plan for HEAD against origin/next
TERRARIUM_CHANGED_FILES=<file with the changed paths> just ci::validate-affected
```

`ci::validate-affected` runs `scripts/affected.py`: fmt, clippy on the changed
crates, the test targets that the changed files can reach (with their reverse
dependencies), the dashboard checks only when `dashboard/` changed, and the
scripts tests only when `scripts/` changed. A change to `Cargo.toml`,
`Cargo.lock`, a `build.rs`, the justfile or the CI files runs the full suite.
The rules are in the header of `scripts/affected.py`.

Write the changed paths with:

```sh
git diff --name-only --no-renames "$(git merge-base origin/next HEAD)" HEAD > /tmp/changed.txt
```

### The full suite

```sh
just ci::validate
```

It runs, in this order, the same parts as the GitHub CI `check` job
(`.github/workflows/ci.yml`), and stops at the first failure:

1. `ci::fmt-check`: `cargo fmt --all --check`
2. `ci::lint`: clippy over the workspace, warnings fail
3. `ci::test`: `cargo test --workspace --no-fail-fast`
   In a release validation (`TERRARIUM_VALIDATE_CONTEXT=release`, the full
   suite at the version cut) or a merge job (`TERRARIUM_JOB_KIND=merge`) it
   is followed by `ci::test-unprivileged`: the built terrarium-host test binaries and the
   terrariumd lib and `it` test binaries run again as uid 1001, the user of
   the release CI, each with a private HOME and the shared /tmp (TER-579,
   TER-611). The terrariumd lib and `it` binaries run alone, the others at
   most 2 at a time.
   The merge host needs a passwd entry for uid 1001 (OpenSSH tests need one):
   `useradd -u 1001 -M -s /usr/sbin/nologin terrarium-ci`. Without it the
   step fails before it runs a test.
   In the same two contexts a failed test step (the root pass or the uid
   1001 pass) runs only its failed tests once more (TER-683,
   `scripts/rerun_tests.py`). All pass: the step passes. One fails again:
   the step fails. No rerun unless every failure is a failed test: a build
   or other cargo error, a crash or signal, a test binary without a result,
   a failed doctest, or more than 5 failed tests keeps it red. With
   `RESULTS_DIR` the step writes `$RESULTS_DIR/reruns.json`, and
   `just ci::flaky-cards "$RESULTS_DIR/reruns.json" "<the run>"` prints the
   flaky-test card commands for the lead. A card check never reruns: there
   each failure shows as it is (docs/api.md 35.3, docs/release.md).
4. `ci::scripts-test`: the Python tests under `scripts/`
5. `ci::dashboard-check`: `npm run check`, then the dashboard unit tests (`npm run test -- --run`) in `dashboard/`

The merge queue (a batch merge job) runs it on the merged result. Run it
yourself only when your change touches the build (see above).

Merge jobs enter through `ci::validate-affected` and `ci::merge`. The merge
gate is opt-in with `TERRARIUM_DOCKER_CI=1` in the delivery recipe. Off by
default, it runs only `ci::validate` and needs neither a key nor Docker.
The switch controls only the recipe gate. Whenever merge secrets are configured,
the daemon resets the VM and opens the tunnel, even if the recipe gate is off.
The daemon consumes the configured `DOCKER_CI_KEY` and
`DOCKER_CI_RESET_KEY` references
from a `delivery:terrarium` run grant. The recipe receives only
`TERRARIUM_DOCKER_HOST` for a job-scoped Unix socket, never a key or SSH
agent; `scripts/docker_ci.py` sets `DOCKER_HOST` from it only when the gate
is on, and removes both when it is off. It runs
the full suite with the pinned remote Docker daemon, then explicitly runs
the ignored lifecycle/workspace and reservation/release tests. A missing
Docker backend, skipped test, or empty selection fails the gate. Workers
must not fetch this key or connect to the Docker CI VM. Card checks remain
`ci::validate-affected` without `TERRARIUM_JOB_KIND=merge`. Device-IO live
tests still require a colocated host agent and remain manual.

When `RESULTS_DIR` is set to an absolute path, `ci::test` also writes libtest
JSON to `$RESULTS_DIR/rust.json`, and `ci::dashboard-check` writes JUnit XML to
`$RESULTS_DIR/dashboard.xml`. Without `RESULTS_DIR` the output is the plain console output.

`just ci::test-nextest` is opt-in: it installs pinned cargo-nextest 0.9.103,
runs the bounded `ci` profile, then runs doctests separately. With `RESULTS_DIR`,
it writes nextest JUnit to `rust.xml` and doctest libtest JSON to `doctests.json`.
Cargo test remains the default because the TER-239 matched-load comparison
was 642.2s for cargo test versus 791.1s for this nextest profile (2495 passed each).

The recipes set `RUSTUP_TOOLCHAIN=1.97.1` (the toolchain CI pins) and the
CI cargo env: `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`. They do not run the release, image, or version
jobs, and they do not run `ci::compat`.

### Target

- Target: `terrarium-helios-build`
- At most one heavy run of yours at a time. Run builds under
  `ionice -c3 nice -n19` with your own `CARGO_TARGET_DIR`.

### Run it on a clean checkout of a pushed commit

Validate the pushed commit, not your working tree:

1. Commit and push the branch. Note the commit hash.
2. Check out a clean copy on the target:

   ```sh
   terrarium repo checkout terrarium --target terrarium-helios-build --branch <branch> --path /src/<agent>-<card>
   ```

3. Check that the checkout HEAD is the pushed commit.
4. Run the check there, as a named run:

   ```sh
   terrarium run terrarium-helios-build --name <name> -- sh -c 'cd /src/<agent>-<card> && git diff --name-only --no-renames "$(git merge-base origin/next HEAD)" HEAD > /tmp/<card>-changed.txt && TERRARIUM_CHANGED_FILES=/tmp/<card>-changed.txt ionice -c3 nice -n19 just ci::validate-affected'
   ```

5. Push the results with `terrarium test push <file> --card <card-id>`.

Do not run `git fetch`, `git pull` or `git ls-remote` against `origin` on the
target. A plain exec has no repo key and no repo host keys there (no
`~/.ssh`), so ssh to `git@github.com` fails with `Host key verification
failed`. Only `terrarium repo checkout` reaches the remote: the daemon gives
that one run the host keys (`TERRARIUM_GIT_KNOWN_HOSTS`) and the credential.
A new clone has `origin/next` as of the clone, which is what the
`merge-base` above needs. An update of an existing path fetches only the
branch, so `origin/next` there stays old. For a newer `next`, check out to a
new path.

### Load and reruns

A failure counts as load, not as a fail, when it is in tests that the card does
not touch and the rerun either passes or fails in a different set of tests. Say
so in the move comment with both runs. Do not edit unrelated tests to make a
run pass.

### Packaged defaults

A change to a file under `defaults/` needs `just defaults::manifest`, and the
updated `defaults/manifest.json` committed with it.

## Branches

New card branches start from `next`, not from `main`:

```sh
git fetch origin
git checkout -b <card-id-lowercase> origin/next
```

## Card flow

1. Claim the card: `terrarium card claim <id> --eta <duration>`. This moves it to in progress.
2. Branch from `next` (see above). Commit with a conventional subject, and push the branch.
3. Run the card check (see above) on a clean checkout of the pushed commit.
4. Move the card to `review` with the command, the commit hash and the pass
   count in the comment, and clear your name.
5. The lead (or a review agent) reviews; an accepted card goes to `merge_queue`.
6. The merge queue merges the cards of the column as one batch and runs the
   full suite; a green batch moves its cards to `accepted`.

Never push to `main` or `next` directly, and never force-push a branch.
Never add a Co-Authored-By or other co-author trailer to a commit.
