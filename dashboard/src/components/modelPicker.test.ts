import { describe, expect, it } from "vitest";
import type { ModelInfo } from "../api/types";
import { afterInput, CLOSED, contextText, filterModels, MAX_SHOWN, matches, moreText, notInList, pickerKey, tokens } from "./modelPicker";

const models: ModelInfo[] = [
  { id: "claude-sonnet-4.6", name: "Claude Sonnet 4.6", context: 264_000 },
  { id: "gpt-5.3-codex", name: "GPT-5.3-Codex", context: 400_000 },
  { id: "mai-code-1.1-flash", name: "MAI-Code-1.1-Flash" },
  { id: "claude-haiku-4.5" },
];

describe("filter", () => {
  it("matches a case-insensitive substring of the id or the name", () => {
    expect(matches(models[0]!, "SONNET")).toBe(true);
    expect(matches(models[1]!, "5.3-cod")).toBe(true);
    expect(matches(models[2]!, "mai-code")).toBe(true);
    expect(matches(models[3]!, "Haiku")).toBe(true);
    expect(matches(models[3]!, "opus")).toBe(false);
    expect(matches(models[3]!, "  ")).toBe(true);
  });

  it("keeps the list order", () => {
    expect(filterModels(models, "claude").shown.map((m) => m.id)).toEqual(["claude-sonnet-4.6", "claude-haiku-4.5"]);
    expect(filterModels(models, "").shown).toHaveLength(4);
    expect(filterModels(models, "zzz")).toEqual({ shown: [], more: 0 });
  });

  it("shows at most the limit and counts the rest", () => {
    const many: ModelInfo[] = Array.from({ length: 320 }, (_, i) => ({ id: `openai/model-${i}` }));
    const f = filterModels(many, "model");
    expect(f.shown).toHaveLength(MAX_SHOWN);
    expect(f.more).toBe(320 - MAX_SHOWN);
    expect(moreText(f.more)).toBe("270 more — keep typing to narrow the list.");
    expect(moreText(0)).toBeNull();
    const narrow = filterModels(many, "model-31");
    expect(narrow.shown.map((m) => m.id)).toEqual(["openai/model-31", ...Array.from({ length: 10 }, (_, i) => `openai/model-31${i}`)]);
    expect(narrow.more).toBe(0);
    expect(filterModels(many, "", 3)).toMatchObject({ more: 317 });
  });

  it("formats the context and finds ids that are not in the list", () => {
    expect(contextText(models[0]!)).toBe("264K context");
    expect(contextText(models[3]!)).toBe("");
    expect(tokens(1_000_000)).toBe("1M");
    expect(tokens(204_800)).toBe("204.8K");
    expect(notInList("my-model", models)).toBe(true);
    expect(notInList("gpt-5.3-codex", models)).toBe(false);
    expect(notInList("", models)).toBe(false);
    expect(notInList("glm-5.3", models, "glm-5.3")).toBe(false);
    expect(notInList("x", undefined)).toBe(false);
  });
});

describe("keyboard", () => {
  const open = (active: number) => ({ open: true, active });

  it("opens with the arrows and moves with a wrap", () => {
    expect(pickerKey(CLOSED, "ArrowDown", 3)).toEqual({ state: open(0), handled: true });
    expect(pickerKey(CLOSED, "ArrowUp", 3)).toEqual({ state: open(2), handled: true });
    expect(pickerKey(open(0), "ArrowDown", 3).state).toEqual(open(1));
    expect(pickerKey(open(2), "ArrowDown", 3).state).toEqual(open(0));
    expect(pickerKey(open(0), "ArrowUp", 3).state).toEqual(open(2));
    expect(pickerKey(open(-1), "ArrowDown", 3).state).toEqual(open(0));
    expect(pickerKey(open(-1), "ArrowDown", 0).state).toEqual(open(-1));
    expect(pickerKey(open(1), "Home", 3).state).toEqual(open(0));
    expect(pickerKey(open(1), "End", 3).state).toEqual(open(2));
    expect(pickerKey(CLOSED, "Home", 3).handled).toBe(false);
  });

  it("picks with Enter, and keeps the typed text with no highlight", () => {
    expect(pickerKey(open(1), "Enter", 3)).toEqual({ state: CLOSED, pick: 1, handled: true });
    expect(pickerKey(afterInput(), "Enter", 3)).toEqual({ state: CLOSED, handled: true });
    // A closed list leaves Enter to the form.
    expect(pickerKey(CLOSED, "Enter", 3).handled).toBe(false);
    expect(pickerKey(open(5), "Enter", 3).pick).toBeUndefined();
  });

  it("closes with Escape and Tab", () => {
    expect(pickerKey(open(1), "Escape", 3)).toEqual({ state: CLOSED, handled: true });
    // A closed list leaves Escape to the modal.
    expect(pickerKey(CLOSED, "Escape", 3).handled).toBe(false);
    expect(pickerKey(open(1), "Tab", 3)).toEqual({ state: CLOSED, handled: false });
    expect(pickerKey(open(1), "a", 3)).toEqual({ state: open(1), handled: false });
  });

  it("opens after a keystroke with no highlight", () => {
    expect(afterInput()).toEqual(open(-1));
  });
});
