// Usage totals split by a key (PLAN §16.2). Pure.

import type { UsageBucket } from "../../api/types";

export interface SpendRow {
  name: string;
  input: number;
  output: number;
  cache: number;
  turns: number;
  /** input + output. */
  tokens: number;
}

export function splitUsage(buckets: UsageBucket[], key: "agent" | "model" | "subscription"): SpendRow[] {
  const m = new Map<string, SpendRow>();
  for (const b of buckets) {
    const name = b[key] || "unknown";
    const r = m.get(name) ?? { name, input: 0, output: 0, cache: 0, turns: 0, tokens: 0 };
    r.input += b.input_tokens;
    r.output += b.output_tokens;
    r.cache += b.cache_tokens;
    r.turns += b.turns;
    r.tokens += b.input_tokens + b.output_tokens;
    m.set(name, r);
  }
  return [...m.values()].sort((a, b) => b.tokens - a.tokens || a.name.localeCompare(b.name));
}

/** Rows of agent x model: who spends on which model. */
export function splitAgentModel(buckets: UsageBucket[]): (SpendRow & { agent: string; model: string })[] {
  const m = new Map<string, SpendRow & { agent: string; model: string }>();
  for (const b of buckets) {
    const k = `${b.agent}\u0000${b.model}`;
    const r = m.get(k) ?? { name: `${b.agent} · ${b.model}`, agent: b.agent, model: b.model, input: 0, output: 0, cache: 0, turns: 0, tokens: 0 };
    r.input += b.input_tokens;
    r.output += b.output_tokens;
    r.cache += b.cache_tokens;
    r.turns += b.turns;
    r.tokens += b.input_tokens + b.output_tokens;
    m.set(k, r);
  }
  return [...m.values()].sort((a, b) => b.tokens - a.tokens);
}
