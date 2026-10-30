import { describe, expect, it } from "vitest";
import type { AgentEvent, Question } from "../../api/types";
import { cardHref } from "../inbox/questions";
import { cardQuestionBadges, isCardQuestionEvent, linkable, priorityChip, splitCardQuestions } from "./questions";

const q = (id: string, over: Partial<Question> = {}): Question => ({
  id,
  kind: "question",
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
  card: "P-1",
  ...over,
});

const ev = (data: Record<string, unknown>, over: Partial<AgentEvent> = {}): AgentEvent => ({ seq: 1, id: "E1", at: "2026-10-28T10:00:00.000Z", kind: "question", project: "p", summary: "", data, ...over });

describe("card question badges", () => {
  it("counts the open questions of each card; a bundle counts once; closed and unlinked ones do not count", () => {
    const b = cardQuestionBadges([
      q("a"),
      q("b", { kind: "bundle", items: [{ text: "x", options: [] }, { text: "y", options: [] }] }),
      q("c", { kind: "notice" }),
      q("d", { status: "answered" }),
      q("e", { status: "retracted" }),
      q("f", { card: null }),
      q("g", { card: "P-2" }),
    ]);
    expect([...b.keys()].sort()).toEqual(["P-1", "P-2"]);
    expect(b.get("P-1")!.count).toBe(3);
    expect(b.get("P-1")!.label).toBe("? 3");
    expect(b.get("P-2")!.count).toBe(1);
    expect(b.get("P-1")!.title).toContain("bundle (2 items): question b");
    expect(b.get("P-1")!.title.split("\n")).toHaveLength(4);
  });

  it("takes the colour of the highest priority, as the Inbox", () => {
    expect(cardQuestionBadges([q("a")]).get("P-1")).toMatchObject({ priority: "normal", cls: "chip info" });
    const urgent = cardQuestionBadges([q("a"), q("b", { priority: "urgent" })]).get("P-1")!;
    expect(urgent).toMatchObject({ priority: "urgent", cls: "chip bad" });
    expect(urgent.title).toMatch(/^2 open questions on P-1 \(urgent\):/);
    expect(cardQuestionBadges([q("a", { priority: "critical" }), q("b", { priority: "urgent" })]).get("P-1")).toMatchObject({ priority: "critical", cls: "chip bad strong" });
    // A closed critical question does not colour the badge.
    expect(cardQuestionBadges([q("a"), q("b", { priority: "critical", status: "answered" })]).get("P-1")!.priority).toBe("normal");
    expect(priorityChip("urgent")).toBe("chip bad");
  });

  it("offers only the open questions of the project with no card for a link", () => {
    const list = [q("a", { card: null }), q("b"), q("c", { card: null, status: "answered" }), q("d", { card: null, project: "other" })];
    expect(linkable(list, "p").map((x) => x.id)).toEqual(["a"]);
  });

  it("splits the questions of a card into open and closed", () => {
    const s = splitCardQuestions([q("a"), q("b", { status: "answered" }), q("c", { status: "retracted" })]);
    expect(s.open.map((x) => x.id)).toEqual(["a"]);
    expect(s.closed.map((x) => x.id)).toEqual(["b", "c"]);
  });

  it("matches the question events of a project, and of a card by its new or its old card", () => {
    expect(isCardQuestionEvent(ev({ card_id: "P-1" }), { project: "p" })).toBe(true);
    expect(isCardQuestionEvent(ev({}), { project: "other" })).toBe(false);
    expect(isCardQuestionEvent(ev({}, { kind: "card" }), { project: "p" })).toBe(false);
    expect(isCardQuestionEvent(ev({ card_id: "P-1" }), { card: "P-1" })).toBe(true);
    expect(isCardQuestionEvent(ev({ previous_card_id: "P-1" }), { card: "P-1" })).toBe(true);
    expect(isCardQuestionEvent(ev({ card_id: "P-2" }), { card: "P-1" })).toBe(false);
  });

  it("links a question to the board with the card drawer open", () => {
    expect(cardHref("my proj", "P-1")).toBe("/p/my%20proj/board?card=P-1");
  });
});
