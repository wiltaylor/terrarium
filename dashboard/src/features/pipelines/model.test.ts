import { describe, expect, it } from "vitest";
import type { Pipeline } from "../../api/types";
import { defOf, formOf, parseEnvSecrets, pipelineState, runDuration, sourceText, stateCounts } from "./model";

const base = { enabled: true, error: null, status: null, polled_at: null } as const;

describe("pipeline model", () => {
  it("gives the state of a pipeline", () => {
    expect(pipelineState({ ...base })).toBe("pending");
    expect(pipelineState({ ...base, polled_at: "2026-11-03T00:00:00Z" })).toBe("unknown");
    expect(pipelineState({ ...base, status: "failed" })).toBe("failed");
    expect(pipelineState({ ...base, status: "success", error: "rate limit" })).toBe("error");
    expect(pipelineState({ ...base, enabled: false, error: "x" })).toBe("off");
    expect(stateCounts([{ ...base, status: "failed" }, { ...base, status: "failed" }, { ...base }])).toEqual({ failed: 2, pending: 1 });
  });

  it("writes a source and a duration", () => {
    expect(sourceText({ kind: "github", repo: "o/r", workflow: "ci.yml", branch: "main" })).toBe("github o/r · ci.yml · @ main");
    expect(sourceText({ kind: "script", target: "wsl", script: "x" })).toBe("script on wsl");
    const now = Date.parse("2026-11-03T01:00:00Z");
    expect(runDuration({ started_at: "2026-11-03T00:58:55Z", finished_at: null, status: "running" }, now)).toBe("1m 05s");
    expect(runDuration({ started_at: "2026-11-03T00:00:00Z", finished_at: "2026-11-03T00:00:42Z", status: "success" }, now)).toBe("42s");
    expect(runDuration({ started_at: "2026-10-03T00:00:00Z", finished_at: null, status: "failed" }, now)).toBe("–");
    expect(runDuration({ started_at: null, finished_at: null, status: "queued" }, now)).toBe("–");
  });

  it("reads the env secrets of a script", () => {
    expect(parseEnvSecrets("OCTOPUS_API_KEY=octo-key\n\n A=b ")).toEqual({ env: { OCTOPUS_API_KEY: "octo-key", A: "b" } });
    expect(parseEnvSecrets("lower=x")).toHaveProperty("error");
    expect(parseEnvSecrets("NOVALUE=")).toHaveProperty("error");
  });

  it("makes the body from the form and back", () => {
    const f = { ...formOf(), name: "ci", repo: "o/r", workflow: "ci.yml" };
    expect(defOf(f)).toEqual({ name: "ci", description: null, source: { kind: "github", repo: "o/r", workflow: "ci.yml" }, schedule: { kind: "interval", every: "5m" }, enabled: true, max_runs: 20 });
    const s = defOf({ ...formOf(), kind: "script", name: "octo", target: "wsl", script: "python3 x.py", envSecrets: "KEY=octo" });
    expect(s.source).toEqual({ kind: "script", target: "wsl", script: "python3 x.py", timeout_secs: 60, env_secrets: { KEY: "octo" } });
    const p = { name: "octo", description: null, source: s.source, schedule: { kind: "interval", every: "1m" }, enabled: false, max_runs: 5 } as unknown as Pipeline;
    const back = formOf(p);
    expect(back).toMatchObject({ kind: "script", target: "wsl", envSecrets: "KEY=octo", every: "1m", maxRuns: "5", enabled: false });
  });
});
