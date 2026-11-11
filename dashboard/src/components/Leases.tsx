import { useInstance } from "../instance";
// Target leases (docs/api.md §17.7): the lease list with Revoke and Extend,
// the Grant dialog, and the "holds <target>" chips. The Targets page and the
// agent page use them.

import { A } from "../instance/navigation";
import { createEffect, createMemo, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { Agent, Lease, LeaseEventData, LeaseGranted, Target } from "../api/types";
import { scopeLabel, targetScope } from "../lib/target-scope";
import { actorLabel, applyLeaseEvent, countdown, holderLabel, shownState, untilValue, type UntilMode } from "../lib/leases";

import { createGrantDialog, eligibleAgents, eligibleTargets, grantTitle, NO_EXCLUSIVE_REASON } from "../features/targets/grant";
import { ConfirmButton, Empty, ErrorBanner, Modal, useNow } from "./ui";

const STATE_CHIP: Record<string, string> = { active: "ok", revoked: "", expired: "warn" };

/** A name for an `agent:<ulid>` actor from the agents store. */
const agentName = (id: string) => {
  const { agents } = useInstance();
  const a = agents.byId(id);
  return a ? `${a.project}/${a.name}` : undefined;
};

/** The expiry as a local time and a live countdown. */
export function LeaseExpiry(props: { lease: Pick<Lease, "expires_at" | "state">; now: number }) {
  return (
    <Show when={props.lease.expires_at} fallback={<span class="muted">until revoked</span>}>
      {(at) => (
        <span title={at()}>
          {new Date(at()).toLocaleString()}
          <Show when={props.lease.state === "active"}>
            {" "}
            <span class="mono small lease-countdown">({countdown(at(), props.now)})</span>
          </Show>
        </span>
      )}
    </Show>
  );
}

/** "holds <target>" chips for an agent, from the leases store. */
export function LeaseChips(props: { agentId: string; now?: number }) {
  const { leases: holds } = useInstance();
  const tick = useNow(15_000);
  const list = () => holds.holdsFor(props.agentId, props.now ?? tick());
  return (
    <For each={list()}>
      {(h) => (
        <span class={`chip ${h.exclusive ? "info" : ""} lease-chip`} title={`Lease on ${h.target}${h.exclusive ? " (exclusive)" : ""}, ${h.expires_at ? `ends ${new Date(h.expires_at).toLocaleString()}` : "until revoked"}`}>
          holds {h.target}
        </span>
      )}
    </For>
  );
}

/**
 * One lease list, kept live from `lease` events. `fetch` reads it with or
 * without the history (`?all=true`). `match` says which events are about it.
 */
export function useLeaseList(opts: {
  key: () => string | null | undefined;
  fetch: (key: string, all: boolean) => Promise<Lease[]>;
  match: (d: LeaseEventData, agentId: string | undefined) => boolean;
  /** Seed the chips from a fresh list. */
  onLoaded?: (key: string, list: Lease[]) => void;
}) {
  const { events } = useInstance();
  const [all, setAll] = createSignal(false);
  const [list, setList] = createSignal<Lease[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  // A memo: a fresh target object with the same name must not refetch.
  const key = createMemo(() => opts.key());
  let gen = 0;
  let soon: ReturnType<typeof setTimeout> | undefined;

  const load = async () => {
    const k = key();
    if (!k) return;
    const mine = ++gen;
    setLoading(true);
    try {
      const rows = await opts.fetch(k, all());
      if (mine !== gen) return;
      setList(rows);
      setError(null);
      opts.onLoaded?.(k, rows);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on([key, all], () => void load()));
  onMount(() => {
    const off = events.on((e) => {
      if (e.kind !== "lease") return;
      const r = applyLeaseEvent(list(), e, { match: opts.match, keepEnded: all() });
      if (r.list !== list()) setList(r.list);
      if (r.refetch) {
        clearTimeout(soon);
        soon = setTimeout(() => void load(), 300);
      }
    });
    onCleanup(() => {
      off();
      clearTimeout(soon);
    });
  });

  /** Put the daemon's answer for one lease into the list. */
  const put = (l: Lease) =>
    setList((xs) => {
      const i = xs.findIndex((x) => x.id === l.id);
      if (l.state !== "active" && !all()) return xs.filter((x) => x.id !== l.id);
      if (i < 0) return [l, ...xs];
      const next = xs.slice();
      next[i] = l;
      return next;
    });

  return { all, setAll, list, loading, error, reload: load, put };
}

/** The lease table with Revoke and Extend. The API refuses a caller without the right; the banner shows its message. */
export function LeaseTable(props: { leases: Lease[]; show: "agent" | "target"; onChange: (l: Lease) => void; history?: boolean; loading?: boolean }) {
  const { api } = useInstance();
  const now = useNow(1000);
  const [error, setError] = createSignal<unknown>(null);
  const [extending, setExtending] = createSignal<string | null>(null);

  const revoke = async (l: Lease) => {
    try {
      props.onChange(await api.targets.revokeLease(l.target, l.agent));
      setError(null);
    } catch (e) {
      setError(e);
    }
  };

  return (
    <div class="stack" style={{ gap: "6px" }}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show
        when={props.leases.length}
        fallback={<Empty>{props.loading ? "Loading…" : props.history ? "No leases." : "No active leases."}</Empty>}
      >
        <div class="table-wrap">
          <table class="lease-table">
            <thead>
              <tr>
                <th>{props.show === "agent" ? "Agent" : "Target"}</th>
                <th>Exclusive</th>
                <th>Granted by</th>
                <th>Expires</th>
                <th>Reason</th>
                <Show when={props.history}>
                  <th>State</th>
                </Show>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={props.leases}>
                {(l) => {
                  const state = () => shownState(l, now());
                  return (
                    <>
                      <tr>
                        <td>
                          <Show when={props.show === "agent"} fallback={<strong>{l.target}</strong>}>
                            <A href={`/agents/${encodeURIComponent(l.agent)}`}>{holderLabel(l)}</A>
                          </Show>
                        </td>
                        <td>{l.exclusive ? <span class="chip info">exclusive</span> : <span class="muted">shared</span>}</td>
                        <td class="small" title={l.granted_at}>
                          {actorLabel(l.granted_by, agentName)}
                        </td>
                        <td class="small">
                          <LeaseExpiry lease={{ ...l, state: state() }} now={now()} />
                        </td>
                        <td class="small">{l.reason ?? <span class="muted">—</span>}</td>
                        <Show when={props.history}>
                          <td class="small">
                            <span class={`chip ${STATE_CHIP[state()] ?? ""}`}>{state()}</span>
                            <Show when={l.revoked_by}>
                              {(by) => <span class="muted"> by {actorLabel(by(), agentName)}</span>}
                            </Show>
                          </td>
                        </Show>
                        <td>
                          <Show when={state() === "active"}>
                            <span class="row" style={{ "flex-wrap": "wrap", gap: "4px" }}>
                              <button class="small" onClick={() => setExtending(extending() === l.id ? null : l.id)}>
                                Extend
                              </button>
                              <ConfirmButton class="small danger" label="Revoke" onConfirm={() => revoke(l)} />
                            </span>
                          </Show>
                        </td>
                      </tr>
                      <Show when={extending() === l.id}>
                        <tr>
                          <td colSpan={props.history ? 7 : 6}>
                            <ExtendForm
                              lease={l}
                              onDone={(next) => {
                                setExtending(null);
                                props.onChange(next);
                              }}
                              onCancel={() => setExtending(null)}
                            />
                          </td>
                        </tr>
                      </Show>
                    </>
                  );
                }}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
    </div>
  );
}

/** The `until` choice: a duration, a time, or until a revoke. */
function UntilInput(props: { mode: UntilMode; duration: string; time: string; onMode: (m: UntilMode) => void; onDuration: (v: string) => void; onTime: (v: string) => void; name: string }) {
  return (
    <div class="row" style={{ "flex-wrap": "wrap", gap: "8px" }} role="radiogroup" aria-label="Until">
      <label class="row small">
        <input type="radio" name={props.name} checked={props.mode === "duration"} onChange={() => props.onMode("duration")} />
        for
      </label>
      <input aria-label="Duration" style={{ width: "6em" }} value={props.duration} disabled={props.mode !== "duration"} placeholder="30m" onInput={(e) => props.onDuration(e.currentTarget.value)} />
      <label class="row small">
        <input type="radio" name={props.name} checked={props.mode === "time"} onChange={() => props.onMode("time")} />
        until
      </label>
      <input aria-label="Until time" type="datetime-local" value={props.time} disabled={props.mode !== "time"} onInput={(e) => props.onTime(e.currentTarget.value)} />
      <label class="row small">
        <input type="radio" name={props.name} checked={props.mode === "none"} onChange={() => props.onMode("none")} />
        until revoked
      </label>
    </div>
  );
}

/** `PATCH /api/targets/{name}/leases/{agent}` with a new `until`. */
function ExtendForm(props: { lease: Lease; onDone: (l: Lease) => void; onCancel: () => void }) {
  const { api } = useInstance();
  const [mode, setMode] = createSignal<UntilMode>("duration");
  const [dur, setDur] = createSignal("30m");
  const [time, setTime] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const save = async (e: Event) => {
    e.preventDefault();
    const u = untilValue(mode(), mode() === "duration" ? dur() : time());
    if ("error" in u) return setError(u.error);
    setBusy(true);
    try {
      props.onDone(await api.targets.extendLease(props.lease.target, props.lease.agent, u.until));
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <form class="stack" style={{ gap: "6px" }} onSubmit={save} aria-label={`Extend the lease of ${holderLabel(props.lease)} on ${props.lease.target}`}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="row" style={{ "flex-wrap": "wrap" }}>
        <span class="small muted">New end, from now:</span>
        <UntilInput name={`extend-${props.lease.id}`} mode={mode()} duration={dur()} time={time()} onMode={setMode} onDuration={setDur} onTime={setTime} />
        <button class="small primary" type="submit" disabled={busy()}>
          Save
        </button>
        <button class="small" type="button" onClick={() => props.onCancel()}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/**
 * Grant a lease. From a target: pick the agent (the target's project, or
 * all for a global target). From an agent: pick the target.
 */
export function GrantDialog(props: { target?: Target; agent?: Agent; targets?: Target[]; onClose: () => void; onGranted: (l: LeaseGranted) => void }) {
  const { api, agents } = useInstance();
  const now = useNow(1000);
  const targetOf = (name: string) => (props.target?.name === name ? props.target : props.targets?.find((t) => t.name === name));
  const dlg = createGrantDialog(api, { target: props.target?.name ?? "", agent: props.agent?.id ?? "" }, Date.now, targetOf);
  const f = dlg.form;
  const target = () => targetOf(f().target);
  const agentChoices = () => eligibleAgents(agents.state.agents, target());
  const targetChoices = () => eligibleTargets(props.targets ?? [], props.agent);
  onMount(() => {
    if (!agents.state.agents.length) void agents.refresh();
  });
  const submit = async (e: Event) => {
    e.preventDefault();
    const l = await dlg.submit();
    if (l) props.onGranted(l);
  };
  return (
    <Modal title={props.target ? grantTitle(props.target) : `Grant a target to ${props.agent?.name ?? "an agent"}`} onClose={() => props.onClose()}>
      <form class="stack" onSubmit={submit} aria-label="Grant a lease">
        <Show
          when={props.target}
          fallback={
            <label>
              Target
              <select value={f().target} onChange={(e) => dlg.set("target", e.currentTarget.value)} required>
                <option value="" disabled>
                  Pick a target
                </option>
                <For each={targetChoices()}>
                  {(t) => (
                    <option value={t.name} selected={t.name === f().target}>
                      {t.name} ({scopeLabel(t)})
                    </option>
                  )}
                </For>
              </select>
            </label>
          }
        >
          <label>
            Agent <span class="small">{target() && targetScope(target()!) === "project" ? `(agents of ${target()!.project})` : "(a global target: all agents)"}</span>
            <select value={f().agent} onChange={(e) => dlg.set("agent", e.currentTarget.value)} required>
              <option value="" disabled>
                Pick an agent
              </option>
              <For each={agentChoices()}>
                {(a) => (
                  <option value={a.id} selected={a.id === f().agent}>
                    {a.project}/{a.name} · {a.status}
                  </option>
                )}
              </For>
            </select>
          </label>
        </Show>
        <div class="stack" style={{ gap: "4px" }}>
          <span class="small muted">Until</span>
          <UntilInput
            name="grant-until"
            mode={f().untilMode}
            duration={f().duration}
            time={f().time}
            onMode={(m) => dlg.set("untilMode", m)}
            onDuration={(v) => dlg.set("duration", v)}
            onTime={(v) => dlg.set("time", v)}
          />
        </div>
        <Show
          when={dlg.exclusiveAllowed()}
          fallback={
            <p class="small muted" aria-label="No exclusive lease" style={{ margin: 0 }}>
              {NO_EXCLUSIVE_REASON}
            </p>
          }
        >
          <label class="row small" style={{ "flex-wrap": "nowrap", "align-items": "flex-start" }}>
            <input type="checkbox" checked={f().exclusive} onChange={(e) => dlg.set("exclusive", e.currentTarget.checked)} />
            <span>Exclusive: no other agent may hold a lease on the target at the same time</span>
          </label>
        </Show>
        <label>
          Reason
          <input value={f().reason} placeholder="TER-3" onInput={(e) => dlg.set("reason", e.currentTarget.value)} />
        </label>
        <Show when={dlg.outcome().kind === "held" ? (dlg.outcome() as Extract<ReturnType<typeof dlg.outcome>, { kind: "held" }>) : null}>
          {(o) => (
            <div class="error-banner lease-held" role="alert">
              <div class="stack" style={{ gap: "2px" }}>
                <strong>
                  Refused: {o().held.holder} holds {o().held.target || f().target}
                  {o().held.exclusive ? " exclusively" : ""}{" "}
                  {o().held.expires_at ? `until ${new Date(o().held.expires_at!).toLocaleString()} (${countdown(o().held.expires_at, now())} left)` : "until revoked"}.
                </strong>
                <span class="small">{o().message}</span>
                <span class="small muted">Revoke that lease first, or wait until it ends.</span>
              </div>
            </div>
          )}
        </Show>
        <Show when={dlg.outcome().kind === "invalid" ? (dlg.outcome() as { message: string }).message : null}>{(m) => <p class="error-banner small" role="alert">{m()}</p>}</Show>
        <ErrorBanner error={dlg.outcome().kind === "error" ? (dlg.outcome() as { error: unknown }).error : null} />
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button class="primary" type="submit" disabled={dlg.outcome().kind === "sending"}>
            {dlg.outcome().kind === "sending" ? "Granting…" : "Grant"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** A note on a grant that ran a lifecycle verb, or tried to. */
export const grantNotice = (l: LeaseGranted): string =>
  `Granted ${l.target} to ${holderLabel(l)}${l.expires_at ? ` until ${new Date(l.expires_at).toLocaleString()}` : " until revoked"}.` +
  (l.lifecycle_error ? ` The target did not come up: ${l.lifecycle_error}` : l.lifecycle ? " The target is coming up." : "");
