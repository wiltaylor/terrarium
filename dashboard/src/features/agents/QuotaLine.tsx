import { useInstance } from "../../instance";
// Under the composers of the agent page (issue #22): the quota hold as a
// banner (it changes whether a turn runs), then one short muted line with
// the plan, the windows, and today's use (`GET /api/usage/me?agent=`,
// docs/api.md §16.3). It takes the place of the old Quota panel.

import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { Agent } from "../../api/types";
import { useNow } from "../../components/ui";
import { timeUntil } from "../../lib/format";

import { describeCause } from "../../stores/quota";
import { quotaLine } from "./quotaText";

export function QuotaLine(props: { agent: Agent }) {
  const { api, quota, events } = useInstance();
  const now = useNow(30_000);
  const [tick, setTick] = createSignal(0);
  const [me] = createResource(
    () => (props.agent.subscription ? ([props.agent.id, tick()] as const) : null),
    async ([id]) => {
      const m = await api.usage.me(id).catch(() => null);
      // Holds older than the event backlog come from here.
      if (m) quota.seedHolds(id, m.holds, props.agent.name);
      return m;
    },
  );
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      const mine = e.agent_id === props.agent.id || e.data?.agent_id === props.agent.id;
      if (!(mine && (e.kind.startsWith("quota.") || e.kind === "usage.sample")) && e.kind !== "quota.sample") return;
      clearTimeout(t);
      t = setTimeout(() => setTick((n) => n + 1), 2000);
    });
    onCleanup(() => {
      off();
      clearTimeout(t);
    });
  });
  const holds = () => quota.holdsFor(props.agent.id, now());
  return (
    <>
      <For each={holds()}>
        {(h) => (
          <div class="error-banner" role="status">
            <span>
              <strong>Blocked by quota until {h.until ? `${new Date(h.until).toLocaleString()} (in ${timeUntil(h.until, now())})` : "further notice"}.</strong> {describeCause(h.cause)}: {h.reason}
              {h.enforced ? "" : " The supervisor does not hold turns yet, so this is advice only."}
            </span>
          </div>
        )}
      </For>
      <Show when={props.agent.subscription}>
        {(plan) => (
          <p class="muted small quota-line" data-testid="quota-line" style={{ margin: 0 }}>
            {quotaLine(me(), plan(), now())}
          </p>
        )}
      </Show>
    </>
  );
}
