// The state of the Grant dialog (docs/api.md §17.7): the form, the body it
// sends, and the answer, with the `409 lease_held` refusal kept apart so that
// the dialog shows the holder and its expiry inline. No JSX; the tests drive
// it with the mock API.

import { createSignal } from "solid-js";
import type { Api } from "../../api/client";
import type { Agent, LeaseGrant, LeaseGranted, LeaseHeldDetails, Target } from "../../api/types";
import { leaseHeld, untilValue, type UntilMode } from "../../lib/leases";

export interface GrantForm {
  target: string;
  /** An agent key; the dialog sends the ULID. */
  agent: string;
  untilMode: UntilMode;
  /** For `duration`: `30m`. */
  duration: string;
  /** For `time`: a `datetime-local` value. */
  time: string;
  exclusive: boolean;
  reason: string;
}

export type GrantOutcome =
  | { kind: "idle" }
  | { kind: "sending" }
  /** A form value that the dialog does not send. */
  | { kind: "invalid"; message: string }
  /** `409 lease_held`: another agent holds the target. */
  | { kind: "held"; message: string; held: LeaseHeldDetails }
  | { kind: "error"; error: unknown }
  | { kind: "done"; lease: LeaseGranted };

/** The body of `POST /api/targets/{name}/leases` from the form, or the reason the form is not complete. */
export function grantBody(f: GrantForm, now = Date.now()): { body: LeaseGrant } | { error: string } {
  if (!f.target) return { error: "Choose a target." };
  if (!f.agent) return { error: "Choose an agent." };
  const u = untilValue(f.untilMode, f.untilMode === "duration" ? f.duration : f.time, now);
  if ("error" in u) return u;
  const body: LeaseGrant = { agent: f.agent, exclusive: f.exclusive };
  if (u.until !== null) body.until = u.until;
  const reason = f.reason.trim();
  if (reason) body.reason = reason;
  return { body };
}

/**
 * The agents that may get a lease on the target: the agents of its project,
 * or all agents for a global target (docs/api.md §17.7).
 */
export const eligibleAgents = (agents: Agent[], target: Pick<Target, "project"> | undefined): Agent[] =>
  agents.filter((a) => !target?.project || a.project === target.project).sort((a, b) => `${a.project}/${a.name}`.localeCompare(`${b.project}/${b.name}`));

/** The targets that may go to the agent: the global ones and those of its project. */
export const eligibleTargets = (targets: Target[], agent: Pick<Agent, "project"> | undefined): Target[] =>
  targets.filter((t) => !t.project || !agent || t.project === agent.project).sort((a, b) => a.name.localeCompare(b.name));

export const emptyGrantForm = (init: Partial<GrantForm> = {}): GrantForm => ({
  target: "",
  agent: "",
  untilMode: "duration",
  duration: "30m",
  time: "",
  exclusive: false,
  reason: "",
  ...init,
});

export function createGrantDialog(api: Pick<Api, "targets">, init: Partial<GrantForm> = {}, now: () => number = Date.now) {
  const [form, setForm] = createSignal<GrantForm>(emptyGrantForm(init));
  const [outcome, setOutcome] = createSignal<GrantOutcome>({ kind: "idle" });
  return {
    form,
    outcome,
    /** Change one field. A change clears a refusal: the next send is a new try. */
    set<K extends keyof GrantForm>(key: K, value: GrantForm[K]) {
      setForm((f) => ({ ...f, [key]: value }));
      if (outcome().kind !== "sending" && outcome().kind !== "done") setOutcome({ kind: "idle" });
    },
    /** Send the grant. Answers the lease, or null on a refusal (see `outcome`). */
    async submit(): Promise<LeaseGranted | null> {
      if (outcome().kind === "sending") return null;
      const b = grantBody(form(), now());
      if ("error" in b) {
        setOutcome({ kind: "invalid", message: b.error });
        return null;
      }
      setOutcome({ kind: "sending" });
      try {
        const lease = await api.targets.grant(form().target, b.body);
        setOutcome({ kind: "done", lease });
        return lease;
      } catch (e) {
        const held = leaseHeld(e);
        setOutcome(held ? { kind: "held", message: held.message, held } : { kind: "error", error: e });
        return null;
      }
    },
  };
}

export type GrantDialogState = ReturnType<typeof createGrantDialog>;
