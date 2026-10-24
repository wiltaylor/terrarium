import { describe, expect, it } from "vitest";
import type { UsageBucket } from "../../api/types";
import { splitAgentModel, splitUsage } from "./split";

const b = (agent: string, model: string, input: number, output: number): UsageBucket => ({ at: "", subscription: "s", project: "p", agent, model, input_tokens: input, output_tokens: output, cache_tokens: 1, turns: 1 });

describe("spend split", () => {
  const rows = [b("lead", "opus", 100, 10), b("lead", "sonnet", 50, 5), b("dev", "opus", 300, 30)];
  it("sums by agent and by model, biggest first", () => {
    expect(splitUsage(rows, "agent").map((r) => [r.name, r.tokens, r.turns])).toEqual([["dev", 330, 1], ["lead", 165, 2]]);
    expect(splitUsage(rows, "model").map((r) => [r.name, r.tokens])).toEqual([["opus", 440], ["sonnet", 55]]);
  });
  it("splits by agent and model together", () => {
    expect(splitAgentModel(rows).map((r) => r.name)).toEqual(["dev · opus", "lead · opus", "lead · sonnet"]);
  });
});
