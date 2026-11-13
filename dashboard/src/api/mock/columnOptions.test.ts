import { describe, expect, it } from "vitest";
import { MOCK_CATALOG } from "./columnOptions";

const args = (verb: string) => MOCK_CATALOG.actions.find((a) => a.verb === verb)?.args.map((f) => `${f.name}:${f.type}${f.required ? "!" : ""}`);

// The names, types and required flags of `rules_file::VERBS` (crates/terrarium-core/src/rules_file.rs).
describe("the mock rule catalog (TER-1242)", () => {
  it("has the daemon's arguments for each verb it lists", () => {
    expect(args("card.move")).toEqual(["to:column!", "from:columns", "comment:text", "position:one_of"]);
    expect(args("card.comment")).toEqual(["text:text!"]);
    expect(args("card.assign")).toEqual(["who:text", "template:text", "templates:list", "owner:text", "from:column", "limit:count"]);
    expect(args("mail")).toEqual(["to:list!", "subject:text!", "body:text"]);
    expect(args("agent.end")).toEqual(["outcome:one_of!", "result:text"]);
  });

  it("has the daemon's if_card fields and if_job values", () => {
    expect(MOCK_CATALOG.conditions.if_card.fields.map((f) => f.name)).toContain("any_labels");
    const job = Object.fromEntries(MOCK_CATALOG.conditions.if_job.fields.map((f) => [f.name, f.values]));
    expect(job.state).toEqual(["queued", "running", "passed", "failed", "cancelled"]);
    expect(job.outcome).toEqual(["red", "conflict", "error", "none"]);
  });
});
