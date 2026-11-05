import { describe, expect, it } from "vitest";
import type { AuditEntry, Tool } from "../../api/types";
import { approvalText, argsText, commandLine, granteeOptions, runRows, toolsOf } from "./model";

const tool = (over: Partial<Tool> = {}): Tool => ({ name: "t", target: "box", command: ["/x"], scope: "global", source: "api", created_at: "", updated_at: "", grants: [], ...over });
const grant = (grantee: string) => ({ tool: "t", grantee, grantee_label: grantee, created_by: "operator", created_at: "" });

describe("tool proxies in the dashboard", () => {
  it("shows the command, the rules, and the approval in words", () => {
    expect(commandLine(["/home/u/.local/bin/uv", "run", "/s/t.py"])).toBe("/home/u/.local/bin/uv run /s/t.py");
    expect(commandLine(["echo", "a b", "it's"])).toBe(`echo 'a b' 'it'\\''s'`);
    expect(argsText(undefined)).toBe("any");
    expect(argsText({ allow: ["show"], deny_flags: ["--config"], max: 4 })).toBe("first argument one of show; no --config; at most 4 arguments");
    expect(approvalText(undefined)).toBe("none");
    expect(approvalText(true)).toBe("every run");
    expect(approvalText({ first: ["comment"] })).toBe("first argument comment");
  });

  it("offers a project tool only to its project, and not the grantees it has", () => {
    const projects = [{ name: "p" }, { name: "q" }];
    const agents = [
      { id: "A1", project: "p", name: "w1" },
      { id: "A2", project: "q", name: "x1" },
      { id: "E1", project: "p", name: "tmp", ephemeral: true },
    ];
    expect(granteeOptions(tool({ project: "p", grants: [grant("project:p")] }), projects, agents).map((o) => o.value)).toEqual(["agent:A1"]);
    expect(granteeOptions(tool(), projects, agents).map((o) => o.value)).toEqual(["agent:A1", "agent:A2", "project:p", "project:q"]);
  });

  it("lists the tools of an agent: its own grants first, then its project's", () => {
    const tools = [tool({ name: "own", grants: [grant("agent:A1")] }), tool({ name: "proj", grants: [grant("project:p")] }), tool({ name: "other", project: "q", grants: [grant("project:q")] }), tool({ name: "none" })];
    expect(toolsOf(tools, { id: "A1", project: "p" }).map((x) => [x.tool.name, x.via])).toEqual([
      ["own", "agent"],
      ["proj", "project"],
    ]);
  });

  it("reads the recent runs from the tool.run audit rows", () => {
    const rows = [
      { id: "1", at: "2026-11-05T10:00:00Z", actor: "agent:A1", action: "tool.run", target: "t", detail: { agent: "p/w1", argv: ["show", "a b"], exit: 0, duration_ms: 1500, truncated: false } },
      { id: "2", at: "2026-11-05T10:01:00Z", actor: "agent:A1", action: "tool.run", target: "t", detail: { argv: [], exit: null, end: { type: "error", code: "truncated" }, duration_ms: 20, truncated: true } },
      { id: "3", at: "2026-11-05T10:02:00Z", actor: "operator", action: "tool.grant", target: "t" },
    ] as AuditEntry[];
    expect(runRows(rows)).toEqual([
      { at: "2026-11-05T10:00:00Z", tool: "t", agent: "p/w1", argv: "show 'a b'", exit: "0", duration: "1.5 s", truncated: false },
      { at: "2026-11-05T10:01:00Z", tool: "t", agent: "agent:A1", argv: "", exit: "truncated", duration: "20 ms", truncated: true },
    ]);
  });
});
