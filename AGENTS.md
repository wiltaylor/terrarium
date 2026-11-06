# AGENTS.md

Instructions for agents that work in this repo.

## Validation

One command gates a card:

```sh
just ci::validate
```

It runs, in this order, the same parts as the GitHub CI `check` job
(`.github/workflows/ci.yml`), and stops at the first failure:

1. `ci::fmt-check`: `cargo fmt --all --check`
2. `ci::lint`: clippy over the workspace, warnings fail
3. `ci::test`: `cargo test --workspace --no-fail-fast`
4. `ci::scripts-test`: the Python tests under `scripts/`
5. `ci::dashboard-check`: `npm run check`, then the dashboard unit tests (`npm run test -- --run`) in `dashboard/`

When `RESULTS_DIR` is set to an absolute path, `ci::test` also writes libtest
JSON to `$RESULTS_DIR/rust.json`, and `ci::dashboard-check` writes JUnit XML to
`$RESULTS_DIR/dashboard.xml`. Without `RESULTS_DIR` the output is the plain console output.

The recipe sets `RUSTUP_TOOLCHAIN=1.97.1` (the toolchain CI pins) and the
CI cargo env: `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`. It fails wherever GitHub CI fails for these parts.
It does not run the release, image, or version jobs, and it does not run `ci::compat`.

### Target and slots

- Target: `terrarium-helios-build`
- Slot count: N=2. At most two validation runs are active on the target at once.
  Do not start a third run. Keep other builds on the target filtered
  (`-p <crate> --test <name>`) while two validations run.

### Load and reruns

A failure counts as load, not as a fail, when it is in tests that the card does
not touch and the rerun either passes or fails in a different set of tests. The
card then goes to review with both runs attached, and the lead decides. Do not
edit unrelated tests to make a run pass.

### Run it on a clean checkout of a pushed commit

Validate the pushed commit, not your working tree:

1. Commit and push the branch. Note the commit hash.
2. Check out a clean copy on the target:

   ```sh
   terrarium repo checkout terrarium --target terrarium-helios-build --branch <branch> --path /src/<agent>-<card>
   ```

3. Check that the checkout HEAD is the pushed commit.
4. Run the recipe there, as a named run. `RESULTS_DIR` is in the shared
   workspace, so the sandbox CLI can read the files:

   ```sh
   terrarium run terrarium-helios-build --name <name> -- sh -c 'cd /src/<agent>-<card> && RESULTS_DIR=/work/terrarium/<agent>/results/<card-id> just ci::validate'
   ```

5. Push the results from the sandbox:

   ```sh
   terrarium test push /work/terrarium/<agent>/results/<card-id>/rust.json --card <card-id>
   terrarium test push /work/terrarium/<agent>/results/<card-id>/dashboard.xml --card <card-id>
   ```

A run takes 40 to 75 minutes on the build target under load.

## Branches

New card branches start from `next`, not from `main`:

```sh
git fetch origin
git checkout -b <card-id-lowercase> origin/next
```

## Card flow

1. Claim the card: `terrarium card claim <id> --eta <duration>`. This moves it to in progress.
2. Branch from `next` (see above). Commit with a conventional subject, and push the branch.
3. Run the validation (see above) on a clean checkout of the pushed commit.
4. Move the card to `validating` with the commit hash in the comment.
5. Review: the card moves to `review`, and a reviewer checks the evidence.
6. Accepted: the card moves to `accepted`.

Never push to `main` or `next` directly, and never force-push a branch.
