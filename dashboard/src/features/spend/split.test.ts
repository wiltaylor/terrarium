import { describe, expect, it } from "vitest";
import type { UsagePoint } from "../../api/types";
import { bucketStarts, cardProject, cardRows, tokenSeries, toPercent, totalsByKey } from "./split";

const p = (bucket: string, key: string, input: number, output: number): UsagePoint => ({ bucket, key, input_tokens: input, output_tokens: output, cache_read: 2, cache_write: 1, messages: 1, turns: 1 });

describe("spend split", () => {
  const rows = [p("2026-10-24T05:00:00Z", "lead", 100, 10), p("2026-10-24T06:00:00Z", "lead", 50, 5), p("2026-10-24T06:00:00Z", "dev", 300, 30)];

  it("sums by key, biggest first, and names keys", () => {
    const t = totalsByKey(rows, (k) => k.toUpperCase());
    expect(t.map((r) => [r.key, r.name, r.tokens, r.turns, r.cache])).toEqual([
      ["dev", "DEV", 330, 1, 3],
      ["lead", "LEAD", 165, 2, 6],
    ]);
  });

  it("ranks cards with their titles and drops samples with no card", () => {
    const pts: UsagePoint[] = [
      { ...p("2026-10-24T05:00:00Z", "TER-1", 10, 1), title: "Parser" },
      { ...p("2026-10-24T06:00:00Z", "TER-1", 10, 1), title: "Parser" },
      { ...p("2026-10-24T06:00:00Z", "TER-2", 100, 1), title: "Lexer" },
      { bucket: "2026-10-24T06:00:00Z", input_tokens: 999, output_tokens: 0, cache_read: 0, cache_write: 0, messages: 1, turns: 1 },
    ];
    expect(cardRows(pts).map((r) => [r.key, r.name, r.tokens])).toEqual([
      ["TER-2", "TER-2 Lexer", 101],
      ["TER-1", "TER-1 Parser", 22],
    ]);
    expect(cardRows(pts, 1)).toHaveLength(1);
    expect(cardProject("TER-42", { TER: "terrarium" })).toBe("terrarium");
    expect(cardProject("MY-APP-3", { "MY-APP": "app" })).toBe("app");
    expect(cardProject("NOPE", { TER: "terrarium" })).toBeUndefined();
  });

  it("fills a token series with zeros, per key or for all", () => {
    const buckets = bucketStarts(Date.parse("2026-10-24T04:30:00Z"), Date.parse("2026-10-24T06:10:00Z"), "hour");
    expect(buckets).toEqual(["2026-10-24T04:00:00Z", "2026-10-24T05:00:00Z", "2026-10-24T06:00:00Z"]);
    expect(tokenSeries(rows, buckets, "lead")).toEqual([0, 110, 55]);
    expect(tokenSeries(rows, buckets)).toEqual([0, 110, 385]);
    expect(toPercent([0, 50, 100, 25])).toEqual([0, 50, 100, 25]);
    expect(toPercent([0, 0])).toEqual([0, 0]);
  });
});
