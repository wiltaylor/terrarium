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
5. `ci::dashboard-check`: `npm run check` and `npm run test -- --run` in `dashboard/`
6. `npm --prefix dashboard test`: the dashboard unit tests (vitest), run again on purpose

The recipe sets `RUSTUP_TOOLCHAIN=1.97.1` (the toolchain CI pins) and the
CI cargo env: `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`. It fails wherever GitHub CI fails for these parts.
It does not run the release, image, or version jobs, and it does not run `ci::compat`.

### Target and slots

- Target: `terrarium-helios-build`
- Slot count: N=2. At most two validation runs are active on the target at once.
  Do not start a third run. Do not start other builds on the target during a run.

### Run it on a clean checkout of a pushed commit

Validate the pushed commit, not your working tree:

1. Commit and push the branch. Note the commit hash.
2. Check out a clean copy on the target:

   ```sh
   terrarium repo checkout terrarium --target terrarium-helios-build --branch <branch> --path /src/<agent>-<card>
   ```

3. Check that the checkout HEAD is the pushed commit.
4. Run the recipe there, as a named run:

   ```sh
   terrarium run terrarium-helios-build --name <name> -- sh -c 'cd /src/<agent>-<card> && just ci::validate'
   ```

5. Push the results with `terrarium test push --card <card-id>`.

The run can take about 30 minutes on the build target.

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
