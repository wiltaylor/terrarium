// The pure helpers of the Inbox threads (docs/api.md §6.13).
import { describe, expect, it } from "vitest";
import type { Question, Thread, ThreadMessage } from "../../api/types";
import { closeBody, newThreadBody, pendingProposal, safeUrl, sortThreads, threadChip, threadHref, threadText, THREAD_MESSAGE_MAX, THREAD_SUMMARY_MAX, timeline, waitsOnYou, whoName } from "./threads";

const thread = (over: Partial<Thread> = {}): Thread => ({
  id: "T1",
  topic: "Shard?",
  scope: "global",
  template: null,
  opened_by: "operator",
  opened_by_name: "operator",
  agent: null,
  agent_name: null,
  report_to: "L",
  report_to_name: "meta/lead",
  status: "waiting_agent",
  response: null,
  handoff: null,
  created_at: "2026-11-06T09:00:00.000Z",
  updated_at: "2026-11-06T09:00:00.000Z",
  closed_at: null,
  closed_by: null,
  ...over,
});
const msg = (id: string, at: string): ThreadMessage => ({ id, author: "operator", author_name: "operator", kind: "message", text: id, at });
const question = (id: string, at: string, status: Question["status"] = "open") => ({ id, created_at: at, status }) as Question;

describe("Inbox threads", () => {
  it("marks a thread that waits on the operator: waiting_operator, or an open question in an open thread", () => {
    expect(waitsOnYou(thread({ status: "waiting_operator" }))).toBe(true);
    expect(waitsOnYou(thread({ open_questions: 1 }))).toBe(true);
    expect(waitsOnYou(thread({ questions: [question("Q", "x", "answered")] }))).toBe(false);
    expect(waitsOnYou(thread({ status: "closed", open_questions: 1 }))).toBe(false);
    expect(threadChip(thread({ status: "waiting_operator" }))).toEqual({ label: "waiting on you", chip: "warn" });
    expect(threadChip(thread())).toEqual({ label: "waiting on the agent", chip: "info" });
    expect(threadChip(thread({ status: "closed" })).label).toBe("closed");
    expect(threadHref("01M7/x")).toBe("/inbox/threads/01M7%2Fx");
  });

  it("shows the agent's close proposal only on an open thread, with its own chip (TER-95)", () => {
    const p = { summary: "Done.", links: [], by: "A1", at: "2026-11-06T09:00:00.000Z" };
    expect(pendingProposal(thread({ status: "waiting_operator", close_proposal: p }))).toEqual(p);
    expect(pendingProposal(thread({ status: "closed", close_proposal: p }))).toBeNull();
    expect(pendingProposal(thread())).toBeNull();
    expect(threadChip(thread({ status: "waiting_operator", close_proposal: p }))).toEqual({ label: "close proposed", chip: "warn" });
    const t = thread({ agent: "A1", agent_name: "meta/architect" });
    expect([whoName(t, "A1"), whoName(t, "operator"), whoName(t, "system"), whoName(t, null)]).toEqual(["meta/architect", "you", "system", "unknown"]);
    expect(safeUrl({ kind: "url", ref: "https://x.example" })).toBe("https://x.example");
    expect(safeUrl({ kind: "url", ref: "javascript:alert(1)" })).toBeNull();
    expect(safeUrl({ kind: "other", ref: "https://x.example" })).toBeNull();
  });

  it("sorts the ones that wait on the operator first, then the newest activity", () => {
    const a = thread({ id: "A", last_at: "2026-11-06T10:00:00.000Z" });
    const b = thread({ id: "B", last_at: "2026-11-06T08:00:00.000Z", status: "waiting_operator" });
    const c = thread({ id: "C", last_at: "2026-11-06T11:00:00.000Z" });
    expect(sortThreads([a, b, c]).map((t) => t.id)).toEqual(["B", "C", "A"]);
  });

  it("puts the questions between the messages by time, after the messages of the same time", () => {
    const t = thread({
      messages: [msg("m1", "2026-11-06T09:00:00.000Z"), msg("m2", "2026-11-06T09:05:00.000Z"), msg("m3", "2026-11-06T09:05:00.000Z")],
      questions: [question("q2", "2026-11-06T09:10:00.000Z"), question("q1", "2026-11-06T09:01:00.000Z"), question("q0", "2026-11-06T09:00:00.000Z")],
    });
    expect(timeline(t).map((e) => e.id)).toEqual(["m1", "q0", "q1", "m2", "m3", "q2"]);
    expect(timeline(thread())).toEqual([]);
  });

  it("checks a message by bytes", () => {
    expect(threadText("  hi  ", 10)).toEqual({ text: "hi" });
    expect(threadText("   ", 10, "response")).toEqual({ error: "Write the response." });
    expect(threadText("é".repeat(6), 10)).toEqual({ error: "The message has 12 bytes; at most 10." });
    expect("text" in threadText("x".repeat(THREAD_MESSAGE_MAX), THREAD_MESSAGE_MAX)).toBe(true);
  });

  it("builds the body of a new thread, and leaves out the empty fields", () => {
    expect(newThreadBody({ template: "", scope: "", topic: "  Shard?  ", text: "", agent: "" })).toEqual({ body: { topic: "Shard?", scope: "global" } });
    expect(newThreadBody({ template: "architect", scope: "terrarium", topic: "Shard?", text: " How? ", agent: "terrarium/lead" })).toEqual({
      body: { topic: "Shard?", scope: "terrarium", template: "architect", agent: "terrarium/lead", text: "How?" },
    });
    expect(newThreadBody({ template: "", scope: "", topic: " ", text: "", agent: "" })).toEqual({ error: "Write the topic." });
    expect(newThreadBody({ template: "", scope: "", topic: "a\nb", text: "", agent: "" })).toEqual({ error: "The topic is one line." });
    expect(newThreadBody({ template: "", scope: "", topic: "x".repeat(201), text: "", agent: "" })).toEqual({ error: "The topic has 201 characters; at most 200." });
  });

  it("builds the handoff of a close from a summary and link lines", () => {
    expect(closeBody("", "")).toEqual({ body: {} });
    expect(closeBody(" Done. ", "card TER-12\n\nwiki plans/x\nhttps://example.com/a\nsomething else")).toEqual({
      body: {
        summary: "Done.",
        links: [
          { kind: "card", ref: "TER-12" },
          { kind: "wiki", ref: "plans/x" },
          { kind: "url", ref: "https://example.com/a" },
          { kind: "other", ref: "something else" },
        ],
      },
    });
    expect(closeBody("x".repeat(THREAD_SUMMARY_MAX + 1), "")).toEqual({ error: `The summary has ${THREAD_SUMMARY_MAX + 1} bytes; at most ${THREAD_SUMMARY_MAX}.` });
    expect(closeBody("", Array.from({ length: 51 }, (_, i) => `card T-${i}`).join("\n"))).toEqual({ error: "A handoff has at most 50 links; you gave 51." });
    expect(closeBody("", `card ${"x".repeat(501)}`)).toEqual({ error: "Link 1 is longer than 500 characters." });
  });
});
