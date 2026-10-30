import { describe, expect, it } from "vitest";
import type { Question } from "../../api/types";
import {
  andList,
  answeredCount,
  bundleAnswerBody,
  carryDraft,
  chosenItemLabel,
  clearDraft,
  type DraftStorage,
  draftKey,
  emptyDraft,
  isComplete,
  loadDraft,
  missingItems,
  parseDraft,
  progressText,
  saveDraft,
} from "./bundle";

const bundle = (over: Partial<Question> = {}): Question => ({
  id: "B1",
  kind: "bundle",
  agent: "A1",
  agent_name: "p/lead",
  project: "p",
  text: "Release plan",
  options: [],
  items: [
    { text: "Ship on Friday?", options: ["yes", "no"] },
    { text: "Which region?", options: ["eu", "us"] },
    { text: "Notes?", options: [] },
  ],
  priority: "normal",
  status: "open",
  thread_id: "T1",
  created_at: "2026-10-28T10:00:00.000Z",
  updated_at: "2026-10-28T10:00:00.000Z",
  revision: 2,
  ...over,
});

/** A storage in memory; `broken` throws on each call, as a blocked storage does. */
function memory(broken = false): DraftStorage & { data: Map<string, string> } {
  const data = new Map<string, string>();
  const guard = () => {
    if (broken) throw new Error("SecurityError");
  };
  return {
    data,
    getItem: (k) => (guard(), data.get(k) ?? null),
    setItem: (k, v) => (guard(), void data.set(k, v)),
    removeItem: (k) => (guard(), void data.delete(k)),
  };
}

describe("bundle answer form", () => {
  it("counts the answered items and is complete only when each item has an option or a text", () => {
    const q = bundle();
    const d = emptyDraft(q);
    expect(d).toHaveLength(3);
    expect(progressText(q, d)).toBe("0 of 3 answered");
    expect(isComplete(q, d)).toBe(false);
    d[0] = { option: 1, text: "" };
    d[2] = { option: null, text: "   " };
    expect(answeredCount(d)).toBe(1);
    expect(missingItems(q, d)).toEqual([2, 3]);
    expect(progressText(q, d)).toBe("1 of 3 answered");
    d[1] = { option: null, text: "us, for the latency" };
    d[2] = { option: null, text: "None." };
    expect(isComplete(q, d)).toBe(true);
    expect(isComplete(bundle({ status: "answered" }), d)).toBe(false);
    // A question with no items is never a complete bundle.
    expect(isComplete(bundle({ items: [] }), [])).toBe(false);
  });

  it("builds one body for all items, with the revision, or names the missing items", () => {
    const q = bundle();
    const d = emptyDraft(q);
    d[0] = { option: 0, text: "" };
    expect(bundleAnswerBody(q, d)).toEqual({ error: "Answer items 2 and 3 first. The bundle goes in one step." });
    d[1] = { option: 1, text: "  for the latency " };
    expect(bundleAnswerBody(q, d)).toEqual({ error: "Answer item 3 first. The bundle goes in one step." });
    d[2] = { option: null, text: "None." };
    expect(bundleAnswerBody(q, d)).toEqual({ body: { revision: 2, items: [{ option: 0 }, { option: 1, answer: "for the latency" }, { answer: "None." }] } });
    expect(bundleAnswerBody(q, [d[0]!, { option: 5, text: "" }, d[2]!])).toEqual({ error: "Item 2: pick one of the options." });
    expect(bundleAnswerBody(q, [d[0]!, d[1]!, { option: null, text: "x".repeat(8001) }])).toEqual({ error: "Item 3: the answer is longer than 8000 characters." });
    expect(bundleAnswerBody(bundle({ status: "retracted" }), d)).toEqual({ error: "The bundle is retracted." });
  });

  it("keeps the draft in storage for each bundle and revision, and survives a broken storage", () => {
    const q = bundle();
    const s = memory();
    expect(loadDraft(s, q)).toBeNull();
    const d = emptyDraft(q);
    d[1] = { option: 0, text: "eu" };
    saveDraft(s, q, d);
    expect(s.data.has(draftKey(q))).toBe(true);
    expect(draftKey(q)).toBe("terrarium.bundle-draft.B1.2");
    expect(loadDraft(s, q)).toEqual(d);
    // Another revision has its own key.
    expect(loadDraft(s, bundle({ revision: 3 }))).toBeNull();
    // An empty draft removes the key; so does a clear.
    saveDraft(s, q, emptyDraft(q));
    expect(s.data.size).toBe(0);
    saveDraft(s, q, d);
    clearDraft(s, q);
    expect(s.data.size).toBe(0);
    // A blocked storage: no throw, no draft.
    const broken = memory(true);
    expect(() => saveDraft(broken, q, d)).not.toThrow();
    expect(loadDraft(broken, q)).toBeNull();
    expect(() => clearDraft(broken, q)).not.toThrow();
    expect(loadDraft(null, q)).toBeNull();
  });

  it("checks a stored draft: storage is not trusted", () => {
    const q = bundle();
    expect(parseDraft(q, "not json")).toBeNull();
    expect(parseDraft(q, "{}")).toBeNull();
    expect(parseDraft(q, JSON.stringify([{ option: 0, text: "" }]))).toBeNull();
    expect(
      parseDraft(
        q,
        JSON.stringify([
          { option: 7, text: "x" },
          { option: "1", text: 5 },
          { option: 0, text: "" },
        ]),
      ),
    ).toEqual([
      { option: null, text: "x" },
      { option: null, text: "" },
      // Item 3 has no options.
      { option: null, text: "" },
    ]);
    expect(parseDraft(q, JSON.stringify([null, {}, {}]))).toBeNull();
  });

  it("carries the answers of unchanged items to a new revision", () => {
    const before = bundle();
    const d = [
      { option: 1, text: "" },
      { option: 0, text: "eu" },
      { option: null, text: "None." },
    ];
    // Item 2 gets a third option; item 3 moves to the top.
    const after = bundle({
      revision: 3,
      items: [
        { text: "Notes?", options: [] },
        { text: "Ship on Friday?", options: ["yes", "no"] },
        { text: "Which region?", options: ["eu", "us", "ap"] },
        { text: "A new one?", options: [] },
      ],
    });
    const c = carryDraft(before, d, after);
    expect(c.draft).toEqual([
      { option: null, text: "None." },
      { option: 1, text: "" },
      { option: null, text: "" },
      { option: null, text: "" },
    ]);
    expect(c.changed).toEqual([3, 4]);
    // No change: everything carries, nothing changed.
    expect(carryDraft(before, d, bundle({ revision: 3 }))).toEqual({ draft: d, changed: [] });
  });

  it("gives the chosen label of an answered item and joins numbers", () => {
    expect(chosenItemLabel({ text: "a", options: ["x", "y"], answer_option: 1, answer: "y" })).toBe("y");
    expect(chosenItemLabel({ text: "a", options: [], answer: "free" })).toBeNull();
    expect(andList([2])).toBe("2");
    expect(andList([1, 3])).toBe("1 and 3");
    expect(andList([1, 2, 4])).toBe("1, 2, and 4");
  });
});
