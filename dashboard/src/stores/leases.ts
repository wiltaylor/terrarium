// Who holds which target now (docs/api.md §17.7), for the "holds <target>"
// chips next to an agent's status badge. Kept live from `lease` events, and
// seeded from the lease lists that the Targets and agent pages read.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { AgentEvent, Lease } from "../api/types";
import { leaseActive, leaseEventAgent, leaseEventData } from "../lib/leases";
import type { EventsStore } from "./events";

/** One active lease, as a chip shows it. */
export interface Hold {
  lease_id: string;
  target: string;
  expires_at: string | null;
  exclusive: boolean;
}

/** Holds by agent ULID, then by target name. */
export type Holds = Record<string, Record<string, Hold>>;

const hold = (l: Pick<Lease, "id" | "target" | "expires_at" | "exclusive">): Hold => ({ lease_id: l.id, target: l.target, expires_at: l.expires_at, exclusive: l.exclusive });

const without = (h: Holds, agent: string, target: string): Holds => {
  if (!h[agent]?.[target]) return h;
  const { [target]: _gone, ...rest } = h[agent];
  return { ...h, [agent]: rest };
};

/** Apply one `lease` event. Pure; returns the same object when nothing changed. */
export function applyHoldEvent(h: Holds, e: AgentEvent): Holds {
  const d = leaseEventData(e);
  const agent = leaseEventAgent(e);
  if (!d || !agent) return h;
  if (d.change === "ended" || d.state !== "active") {
    // Only the end of the lease that the chip shows removes it.
    return h[agent]?.[d.target]?.lease_id === d.lease_id ? without(h, agent, d.target) : h;
  }
  return { ...h, [agent]: { ...h[agent], [d.target]: hold({ id: d.lease_id, target: d.target, expires_at: d.expires_at, exclusive: d.exclusive }) } };
}

/** Put the active leases of one agent in place of what the store had for it. Pure. */
export function seedAgentHolds(h: Holds, agent: string, leases: Lease[], now = Date.now()): Holds {
  const mine = leases.filter((l) => l.agent === agent && leaseActive(l, now));
  return { ...h, [agent]: Object.fromEntries(mine.map((l) => [l.target, hold(l)])) };
}

/** Put the active leases on one target in place of what the store had for it. Pure. */
export function seedTargetHolds(h: Holds, target: string, leases: Lease[], now = Date.now()): Holds {
  let out: Holds = {};
  for (const [agent, byTarget] of Object.entries(h)) out[agent] = Object.fromEntries(Object.entries(byTarget).filter(([t]) => t !== target));
  for (const l of leases) if (l.target === target && leaseActive(l, now)) out = { ...out, [l.agent]: { ...out[l.agent], [target]: hold(l) } };
  return out;
}

/** The holds of an agent that have not passed their time, by target name. */
export const holdsOf = (h: Holds, agent: string, now = Date.now()): Hold[] =>
  Object.values(h[agent] ?? {})
    .filter((x) => !x.expires_at || Date.parse(x.expires_at) > now)
    .sort((a, b) => a.target.localeCompare(b.target));

export function createLeasesStore(api: Pick<Api, "targets">, events?: Pick<EventsStore, "on">) {
  const [holds, setHolds] = createSignal<Holds>({});
  let loading: Promise<void> | null = null;
  const off = events?.on((e) => {
    if (e.kind !== "lease") return;
    const next = applyHoldEvent(holds(), e);
    if (next !== holds()) setHolds(next);
  });
  return {
    holds,
    holdsFor: (agent: string, now?: number) => holdsOf(holds(), agent, now),
    seedAgent: (agent: string, leases: Lease[]) => setHolds((h) => seedAgentHolds(h, agent, leases)),
    seedTarget: (target: string, leases: Lease[]) => setHolds((h) => seedTargetHolds(h, target, leases)),
    /**
     * Read the active leases of every target once, for the agents grid.
     * There is no list of all leases; a daemon without leases answers 404,
     * and the chips then come from the events only.
     */
    loadAll(): Promise<void> {
      loading ??= api.targets
        .list()
        .then(async (targets) => {
          const lists = await Promise.all(targets.map((t) => api.targets.leases(t.name).catch(() => null)));
          targets.forEach((t, i) => {
            const l = lists[i];
            if (l) setHolds((h) => seedTargetHolds(h, t.name, l));
          });
        })
        .catch(() => {
          loading = null; // signed out, or a daemon without targets: try again next time
        });
      return loading;
    },
    dispose: () => off?.(),
  };
}

export type LeasesStore = ReturnType<typeof createLeasesStore>;
