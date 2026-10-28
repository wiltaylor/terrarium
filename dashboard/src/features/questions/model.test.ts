import { describe, expect, it } from "vitest";
import type { AgentEvent, Question } from "../../api/types";
import { answerBody, chosenLabel, isQuestionEvent, matchesFilter, openCounts, putQuestion, shortQuestion, sortQuestions } from "./model";

const q = (id: string, over: Partial<Question> = {}): Question => ({
  id,
  agent: "A1",
  agent_name: "p/lead",
  project: "p",
  text: `question ${id}`,
  options: [],
  priority: "normal",
  status: "open",
  thread_id: `T${id}`,
  created_at: "2026-10-28T10:00:00.000Z",
  updated_at: "2026-10-28T10:00:00.000Z",
  revision: 1,
  ...over,
});

describe("questions model", () => {
  it("sorts open first, urgent first, oldest first; then the others newest first", () => {
    const list = [
      q("done-old", { status: "answered", answered_at: "2026-10-28T08:00:00.000Z" }),
      q("late", { created_at: "2026-10-28T11:00:00.000Z" }),
      q("urgent", { priority: "urgent", created_at: "2026-10-28T12:00:00.000Z" }),
      q("done-new", { status: "answered", answered_at: "2026-10-28T09:00:00.000Z" }),
      q("early", { created_at: "2026-10-28T09:00:00.000Z" }),
    ];
    expect(sortQuestions(list).map((x) => x.id)).toEqual(["urgent", "early", "late", "done-new", "done-old"]);
  });

  it("counts the open and urgent questions for the badge", () => {
    expect(openCounts([q("a"), q("b", { priority: "urgent" }), q("c", { status: "answered", priority: "urgent" })])).toEqual({ open: 2, urgent: 1 });
    expect(openCounts([])).toEqual({ open: 0, urgent: 0 });
  });

  it("puts a changed question in place and keeps the order", () => {
    const list = sortQuestions([q("a"), q("b", { created_at: "2026-10-28T11:00:00.000Z" })]);
    const next = putQuestion(list, q("b", { created_at: "2026-10-28T11:00:00.000Z", priority: "urgent" }));
    expect(next.map((x) => x.id)).toEqual(["b", "a"]);
    expect(putQuestion(list, q("c")).map((x) => x.id)).toContain("c");
  });

  it("builds an answer from an option, a text, or both", () => {
    const withOptions = q("a", { options: ["yes", "no"] });
    expect(answerBody(withOptions, "", 1)).toEqual({ body: { option: 1 } });
    expect(answerBody(withOptions, "  because  ", 0)).toEqual({ body: { option: 0, answer: "because" } });
    expect(answerBody(q("b"), "free text", null)).toEqual({ body: { answer: "free text" } });
    expect(answerBody(withOptions, " ", null)).toEqual({ error: "Pick an option or write an answer." });
    expect(answerBody(q("b"), "", null)).toEqual({ error: "Write an answer." });
    expect(answerBody(withOptions, "", 5)).toEqual({ error: "Pick one of the options." });
    expect(answerBody({ ...withOptions, status: "answered" }, "x", null)).toEqual({ error: "The question is answered." });
    expect("error" in answerBody(q("b"), "x".repeat(8001), null)).toBe(true);
  });

  it("filters, names the chosen option, and matches the events", () => {
    expect(matchesFilter(q("a"), "open")).toBe(true);
    expect(matchesFilter(q("a", { status: "retracted" }), "open")).toBe(false);
    expect(matchesFilter(q("a", { status: "retracted" }), "all")).toBe(true);
    expect(chosenLabel(q("a", { options: ["x", "y"], answer_option: 1 }))).toBe("y");
    expect(chosenLabel(q("a"))).toBeNull();
    const e = { kind: "question", agent_id: "A1", data: { agent_id: "A1" } } as unknown as AgentEvent;
    expect(isQuestionEvent(e)).toBe(true);
    expect(isQuestionEvent(e, "A1")).toBe(true);
    expect(isQuestionEvent(e, "A2")).toBe(false);
    expect(isQuestionEvent({ ...e, kind: "wait" })).toBe(false);
    expect(shortQuestion("first line\nsecond", 40)).toBe("first line…");
  });
});
