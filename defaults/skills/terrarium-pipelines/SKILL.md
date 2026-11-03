---
name: terrarium-pipelines
description: Show the CI/CD runs of the project in Terrarium (`terrarium pipeline new`): GitHub Actions, or a script for any other system (Octopus Deploy, Jenkins, GitLab). Read the status of the newest build or deploy with `terrarium pipeline ls`, and the steps of a run with `terrarium pipeline run`. Use it before you report that a change is built or deployed, and when you set up CI for a project.
metadata:
  terrarium-default: "true"
---

# Pipelines in Terrarium

A pipeline shows the runs of a CI/CD system in a project: the builds of
GitHub Actions, or the deploys of Octopus Deploy. Terrarium polls each
pipeline on a schedule (every 5 minutes by default) and keeps the recent
runs. The operator sees them on the Pipelines page of the project.

A pipeline is proof that you did not make. When you say "the build is
green", give the run from `terrarium pipeline ls`.

## Who can do what

- Every agent of the project reads the pipelines and their runs.
- A project admin (a lead), a sysadmin, and the operator make, change,
  refresh, and delete pipelines.
- A script pipeline runs on a target. The target must be in your
  `policy.targets`, or you must hold a lease on it.
- A secret that a pipeline names (the GitHub token, or a key for a script)
  must be a secret of your project, or a global secret with a `use` grant
  to your project.
- Each poll uses your rights again. When you lose the target or the
  secret, the pipeline shows `Poll error` with the reason.

## Read the pipelines

```
terrarium pipeline ls                      # each pipeline, the status of its newest run
terrarium pipeline runs ci                 # the recent runs, newest first
terrarium pipeline run ci 9123456789       # one run with its steps (the GitHub jobs)
terrarium pipeline refresh ci              # poll now (a project admin)
```

The status of a run is one of `queued`, `running`, `waiting` (a person
must approve), `success`, `failed`, `cancelled`, `skipped`, or `unknown`.

## GitHub Actions

```
terrarium pipeline new ci --github owner/repo --workflow ci.yml --branch main \
  --token-secret github-token --every 2m
```

- `--workflow` is the file name of the workflow. Without it, the pipeline
  shows the runs of all workflows.
- `--token-secret` names a secret that holds a token with read access to
  Actions. A private repo needs it. Without a token, GitHub allows only 60
  requests an hour for each address.
- GitHub Enterprise Server: add `--api-url https://<host>/api/v3`.

## Any other system: a script

A script pipeline runs a script with `sh -c` on a target. The script
asks the CI/CD system for its recent runs, and prints them as JSON on
stdout, newest run first:

```json
{"runs": [
  {"id": "Deployments-123", "name": "1.4.2 to Production", "status": "Executing",
   "environment": "Production", "url": "https://octopus.example/app#/...",
   "started_at": "2026-11-03T10:00:00+10:00",
   "steps": [{"name": "Deploy web", "status": "Success"},
             {"name": "Smoke test", "status": "Executing"}]}
]}
```

- `id` and `status` are necessary. The other keys are optional: `name`,
  `url`, `branch`, `commit`, `actor`, `event`, `environment`,
  `started_at`, `finished_at` (RFC 3339), `summary`, `number`, `steps`.
- The status words of common tools are fine: `Executing`, `Canceled`,
  `TimedOut`, `Succeeded`, `in_progress`.
- Exit with 0. Another exit code is a poll error, with the last line of
  stderr as the reason.
- Put a key in the environment from a secret with `--env-secret
  VAR=secret`. Do not write a key in the script.

```
terrarium pipeline test --target wsl --script 'python3 ~/bin/octopus-runs.py' \
  --env-secret OCTOPUS_API_KEY=octopus-key          # try it, nothing is kept
terrarium pipeline new deploys --target wsl --script 'python3 ~/bin/octopus-runs.py' \
  --env-secret OCTOPUS_API_KEY=octopus-key --every 5m
```

The script also gets `TERRARIUM_PROJECT` and `TERRARIUM_PIPELINE`.

## Alerts

Each new run, and each change of the status of a run, is a `pipeline.run`
event. A change of the status of the newest run is a `pipeline.status`
event. A poll that starts to fail is a `pipeline.error` event. A trigger
turns them into mail, a webhook, or an exec:

```
terrarium schedule new deploy-failed --on pipeline.run --match 'data.to=failed' \
  --mail lead -s 'A deploy failed' --body '{{event.summary}}'
```
