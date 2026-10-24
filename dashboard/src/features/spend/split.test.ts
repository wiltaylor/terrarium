import { describe, expect, it } from "vitest";
import type { UsagePoint } from "../../api/types";
import { bucketStarts, tokenSeries, toPercent, totalsByKey } from "./split";

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

  it("fills a token series with zeros, per key or for all", () => {
    const buckets = bucketStarts(Date.parse("2026-10-24T04:30:00Z"), Date.parse("2026-10-24T06:10:00Z"), "hour");
    expect(buckets).toEqual(["2026-10-24T04:00:00Z", "2026-10-24T05:00:00Z", "2026-10-24T06:00:00Z"]);
    expect(tokenSeries(rows, buckets, "lead")).toEqual([0, 110, 55]);
    expect(tokenSeries(rows, buckets)).toEqual([0, 110, 385]);
    expect(toPercent([0, 50, 100, 25])).toEqual([0, 50, 100, 25]);
    expect(toPercent([0, 0])).toEqual([0, 0]);
  });
});
