---
name: terrarium-tests
description: Push test result files (JUnit XML, TRX, CTRF, libtest JSON, Vitest, Jest, TAP) with `terrarium test push`, and link them to the card or the version. Do it after each test run that proves work.
metadata:
  terrarium-default: "true"
---

# Test results in Terrarium

Terrarium keeps the results of test runs. You upload a standard test
result file with `terrarium test push`. The daemon reads the file and
keeps each test case: its status, its duration, its failure message, its
trace, and its output. The operator sees the run on the board (Board →
Tests), on the card, and in the Version modal. The operator can compare
a run with the previous run of the same name.

A test run is proof. A pass count in a comment is a claim. Push the file
so that the operator can see each test.

## When to push results

- After each test run that proves the work of a card. Link the run to the
  card with `--card <id>`.
- Before you move a card to review. The reviewer reads the run on the
  card.
- Before you submit a version. Link the run with `--version <name>`.
  You can link a run only to a version in `draft` or `rejected`.
- When tests fail and you report the failure. A failing run shows the
  messages and the traces better than a comment.

Do not hide failures. Push the run also when tests fail, and say so in
the card comment. Terrarium does not move a card because of a failing
run. You decide the next step.

## Make a standard file

Run the tests on a target (`terrarium exec <target> -- ...`). Write the
result file into the workspace, so that you can read it in the sandbox.

### Rust: cargo-nextest (JUnit XML)

Add a `ci` profile to `.config/nextest.toml`:

```toml
[profile.ci.junit]
path = "junit.xml"
```

```sh
cargo nextest run --profile ci
# The file: target/nextest/ci/junit.xml
```

### Rust: cargo test (libtest JSON)

The JSON format of libtest is unstable. Use a nightly toolchain, or set
`RUSTC_BOOTSTRAP=1`:

```sh
RUSTC_BOOTSTRAP=1 cargo test -- -Z unstable-options --format json --report-time > results.json
```

cargo-nextest can write the same format:

```sh
NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 cargo nextest run --message-format libtest-json > results.json
```

### TypeScript and JavaScript

```sh
# Vitest: JUnit XML, or the JSON reporter
npx vitest run --reporter=junit --outputFile=junit.xml
npx vitest run --reporter=json --outputFile=vitest.json

# Jest: the JSON output, or jest-junit
npx jest --json --outputFile=jest.json
npx jest --reporters=default --reporters=jest-junit   # writes junit.xml

# Node test runner: TAP
node --test --test-reporter=tap > results.tap

# Playwright: CTRF (the playwright-ctrf-json-reporter package)
npx playwright test --reporter=playwright-ctrf-json-reporter   # writes ctrf/ctrf-report.json
```

### Python

```sh
pytest --junitxml=junit.xml
```

### .NET

```sh
dotnet test --logger "trx;LogFileName=results.trx"      # TestResults/results.trx
dotnet test --logger "junit;LogFilePath=junit.xml"      # the JunitXml.TestLogger package
```

### Go

```sh
go test -v ./... 2>&1 | go-junit-report > junit.xml
```

### Other tools

Most test tools write JUnit XML (Maven Surefire, Gradle, PHPUnit,
RSpec with a formatter). Many write CTRF JSON or TAP. Use one of these
formats.

## Push the file

```sh
terrarium test push target/nextest/ci/junit.xml --name unit --card TER-12 \
  --commit "$(git rev-parse HEAD)" --branch "$(git branch --show-current)"
```

- `--name` names the run. Use the same name for the same suite each
  time (`unit`, `e2e`, `api`), so that the compare finds the previous run.
- `--card <id>` and `--version <name>` link the run. Give each flag more
  than once for more links.
- `--format` is `auto` by default. Terrarium finds the format from the
  content. Give `junit`, `trx`, `ctrf`, `libtest-json`, `vitest-json`,
  `jest-json`, or `tap` when the detection fails.
- `--commit` and `--branch` tell the reviewer which code ran.

The command prints the counts and the dashboard link. It exits with 0
also when tests fail: it uploads, it does not judge. Add
`--fail-on-failures` to exit with 1 when a test failed.

## Read and compare runs

```sh
terrarium test ls --card TER-12          # the runs of a card, newest first
terrarium test ls --name unit --failed   # failing runs of one name
terrarium test show <id> --failed        # the failing cases with messages
terrarium test compare <id>              # against the previous run of the same name
terrarium test compare <id> --with <other-id>
```

The compare lists the tests that fail now and passed before (newly
failing), the tests that pass now (fixed), the tests that still fail,
the new tests, and the removed tests.

## Links and deletes

```sh
terrarium test link <id> --card TER-12 --version v1.4.0
terrarium test unlink <id> --card TER-9
terrarium test rm <id>
```

You link a run to a card that you can change: your card, or a card that
you made. A project admin links to any card.

## Rules

- Push the file that the tool wrote. Do not edit it, and do not write a
  result file by hand.
- Each file is at most 20 MiB and 100 000 test cases. Terrarium cuts each
  message, trace, and output to 64 KiB.
- In the card comment, give the run ID or the dashboard link, and the
  counts.
- Do not put secrets in test output. Terrarium replaces a known secret
  value with `[secret:<name>]`.
