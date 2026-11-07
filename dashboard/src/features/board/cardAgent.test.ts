import { describe, expect, it } from "vitest";
import { cardAgentName } from "./cardAgent";

describe("cardAgentName", () => {
  it("names the bound agent of a waiting card", () => {
    expect(cardAgentName({ agent: "E1", agent_name: "ter-2" })).toBe("ter-2");
  });
  it("leaves it out when the card has none or the agent is the assignee", () => {
    expect(cardAgentName({})).toBeUndefined();
    expect(cardAgentName({ agent: "E1", agent_name: "ter-3", assignee: "E1", assignee_name: "terrarium/ter-3" })).toBeUndefined();
    expect(cardAgentName({ agent: "E1", agent_name: "ter-3", assignee_name: "terrarium/ter-3" })).toBeUndefined();
  });
  it("names it next to another assignee", () => {
    expect(cardAgentName({ agent: "E1", agent_name: "ter-4", assignee: "A1", assignee_name: "terrarium/lead" })).toBe("ter-4");
  });
});
