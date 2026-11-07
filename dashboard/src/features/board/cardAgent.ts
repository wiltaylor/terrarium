// The card agent on a card tile (TER-75): the ephemeral agent bound to the
// card, named when it is not the assignee already.
import type { Card } from "../../api/types";

/** The name of the card agent of `c`, or undefined when it has none or it is the assignee. */
export function cardAgentName(c: Pick<Card, "agent" | "agent_name" | "assignee" | "assignee_name">): string | undefined {
  const name = c.agent_name;
  if (!name) return undefined;
  if (c.agent && c.assignee === c.agent) return undefined;
  const assignee = c.assignee_name ?? "";
  if (assignee === name || assignee.split("/").pop() === name) return undefined;
  return name;
}
