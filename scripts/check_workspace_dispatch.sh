#!/bin/sh
# Inactive dispatcher's bounded card check. Run ONLY after target RESUME and
# disposition of any older run. Never reuse an active checkout/results path.
# Usage: CHECKOUT=/src/<clean-checkout> RESULTS_DIR=/src/<new-results> \
#        EXPECTED_COMMIT=<pushed-full-sha> sh scripts/check_workspace_dispatch.sh
#
# This script's receipts are process results, not manufactured test results.
# SIGKILL/host loss can prevent even the outer receipt. Absence remains UNKNOWN.
# timeout/group cleanup is not proof against escaped descendant groups or
# uninterruptible kernel work. A separate remaining-process receipt is required
# after intervention. Do not cancel a run merely to advance a staged host update.
set -u
: "${CHECKOUT:?set the clean checkout path}"
: "${RESULTS_DIR:?set a fresh absolute results path}"
: "${EXPECTED_COMMIT:?set the pushed commit SHA}"
case "$RESULTS_DIR" in /*) ;; *) exit 64 ;; esac
mkdir "$RESULTS_DIR" || exit 73
export CHECKOUT RESULTS_DIR EXPECTED_COMMIT

finish() {
    rc=$?
    trap - EXIT
    if ! { printf '%s\n' "$rc" > "$RESULTS_DIR/run.rc.tmp" &&
        mv "$RESULTS_DIR/run.rc.tmp" "$RESULTS_DIR/run.rc"; }; then
        rc=74
    fi
    printf 'runner_exit=%s\n' "$rc"
    exit "$rc"
}
trap finish EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# The outer shell survives timeout of this child and records 124/137 as failure.
# The entire child, including lock acquisition, is limited to 20 minutes, with
# KILL escalation after 15 seconds. Slot contention ends with 75 after 60 seconds.
# Never retry a timeout or lock failure automatically, or reinterpret it as pass.
timeout --signal=TERM --kill-after=15s 20m \
    flock -w 60 -E 75 /src/terrarium-cardcheck-4.lock \
    env CARGO_BUILD_JOBS=8 RUST_TEST_THREADS=8 \
    CARGO_TARGET_DIR=/src/terrarium-cardcheck-target-4 \
    RUSTUP_TOOLCHAIN=1.97.1 CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 RUSTC_BOOTSTRAP=1 \
    ionice -c3 nice -n19 sh -c '
        cd "$CHECKOUT" || exit 72
        actual=$(git rev-parse HEAD) || exit 72
        printf "%s\n" "$actual" > "$RESULTS_DIR/commit"
        [ "$actual" = "$EXPECTED_COMMIT" ] || exit 65
        git diff --quiet && git diff --cached --quiet || exit 65
        base=$(git merge-base origin/next HEAD) || exit 65
        git diff --name-only --no-renames "$base" HEAD > "$RESULTS_DIR/changed.txt" || exit 65
        TERRARIUM_CHANGED_FILES="$RESULTS_DIR/changed.txt"
        export TERRARIUM_CHANGED_FILES
        record() {
            printf "%s\n" "$2" > "$RESULTS_DIR/$1.rc.tmp" &&
                mv "$RESULTS_DIR/$1.rc.tmp" "$RESULTS_DIR/$1.rc"
        }
        just ci::affected > "$RESULTS_DIR/affected.log" 2>&1
        rc=$?; record selector "$rc" || exit 74; [ "$rc" -eq 0 ] || exit "$rc"
        cargo test -p terrariumd --lib live_workspace_dispatch:: -- -Z unstable-options --format json --report-time > "$RESULTS_DIR/fakes.json" 2> "$RESULTS_DIR/build.log"
        rc=$?; record fakes "$rc" || exit 74; [ "$rc" -eq 0 ] || exit "$rc"
        just ci::validate-affected > "$RESULTS_DIR/check.log" 2>&1
        rc=$?; record check "$rc" || exit 74; exit "$rc"
    ' > "$RESULTS_DIR/runner.log" 2>&1
exit "$?"
