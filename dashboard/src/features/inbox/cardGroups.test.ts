import { expect, it } from "vitest";
import type { InboxItem, Question } from "../../api/types";
import { groupCardQuestions } from "./model";

const item = (id: string, card?: string, kind = "question", open = true, project = "p"): InboxItem => ({
  id, kind: kind === "bundle" ? "question" : kind as InboxItem["kind"], open, urgent: false, project, at: "now",
  question: { id, card, kind } as Question,
});

it("groups questions and bundles by card and project without losing item order or ids", () => {
  const rows = [item("urgent", "P-1"), item("mail", undefined, "mail"), item("bundle", "P-1", "bundle"), item("other", "P-2"), item("peer", "P-1", "question", true, "q")];
  const groups = groupCardQuestions(rows);
  expect(groups.map(g => g.items.map(i => i.id))).toEqual([["urgent", "bundle"], ["mail"], ["other"], ["peer"]]);
  expect(groups[0]?.card).toBe("P-1");
});

it("leaves notices, settled questions and cardless questions ungrouped", () => {
  const rows = [item("a", "P-1"), item("notice", "P-1", "notice"), item("done", "P-1", "question", false), item("cardless")];
  expect(groupCardQuestions(rows).map(g => g.items.length)).toEqual([1, 1, 1, 1]);
});
