// The login of a target (PLAN §16b.6a, docs/api.md §17.2): a select of the
// secrets that the target can use. An ssh target takes an `ssh_key`; a vnc
// or rdp target takes a `login`. A project target takes its project's
// secrets and the global ones; a global target, the global ones only. A
// global secret that the project holds no grant of gets a hint: the
// operator can set it, a project_admin needs a grant. An ssh target has
// "New key…": it generates a key in the store and selects it.

import { createResource, createSignal, For, Show } from "solid-js";
import type { Secret, TargetKind } from "../../api/types";
import { api } from "../../stores";
import { credentialChoices, credentialKind, GLOBAL, NEEDS_GRANT_HINT } from "./model";
import { GenerateKeyForm } from "./GenerateKey";
import { PublicKey } from "./PublicKey";

/** Not a secret name: a name has no `+`. */
const NEW_KEY = "+new-key";

export function CredentialSelect(props: {
  kind: TargetKind;
  /** A project name or `global`; "" when not chosen yet. */
  scope: string;
  value: string;
  onChange: (name: string) => void;
  /** The name of the target, for the name of a new key. */
  targetName?: string;
}) {
  const [secrets, { refetch }] = createResource(
    () => (props.scope && credentialKind(props.kind) ? { scope: props.scope } : null),
    async (v): Promise<Secret[]> => {
      const reads = [api.secrets.list({ scope: GLOBAL }).catch(() => [] as Secret[])];
      if (v.scope !== GLOBAL) reads.push(api.secrets.list({ scope: v.scope }).catch(() => [] as Secret[]));
      return (await Promise.all(reads)).flat();
    },
  );
  // A project target: the global secrets of the kind that the project holds
  // a grant of (PLAN §16b.6a). The others stay in the list with a hint: the
  // operator needs no grant, a project_admin does.
  const [granted] = createResource(
    () => {
      const want = credentialKind(props.kind);
      if (!props.scope || props.scope === GLOBAL || !want || !secrets.latest) return null;
      return { project: props.scope, names: secrets.latest.filter((s) => s.scope === GLOBAL && s.kind === want).map((s) => s.name) };
    },
    async (v): Promise<Set<string>> => {
      const who = `project:${v.project}`;
      const rows = await Promise.all(v.names.map((n) => api.secrets.grants(GLOBAL, n).then((gs) => (gs.some((g) => g.grantee === who) ? n : null)).catch(() => null)));
      return new Set(rows.filter((n): n is string => !!n));
    },
  );
  const [generating, setGenerating] = createSignal(false);
  const [made, setMade] = createSignal<Secret | null>(null);
  // The new key is in the list at once, before the refetch ends.
  const choices = () => credentialChoices([...(secrets.latest ?? []), ...(made() ? [made()!] : [])], props.kind, props.scope || GLOBAL, granted.latest);
  const picked = () => choices().find((c) => c.name === props.value);
  /** A value that is not in the list: an old name, or a secret this viewer cannot read. */
  const unknown = () => !!props.value && !choices().some((c) => c.name === props.value);
  const ssh = () => props.kind === "ssh";
  const what = () => (ssh() ? "an ssh_key" : "a login");

  return (
    <div class="stack" style={{ gap: "6px" }}>
      <select
        aria-label="Credential"
        value={props.value}
        disabled={!props.scope}
        onChange={(e) => {
          const v = e.currentTarget.value;
          if (v === NEW_KEY) {
            setGenerating(true);
            e.currentTarget.value = props.value;
          } else props.onChange(v);
        }}
      >
        <option value="" selected={!props.value}>
          {!props.scope ? "Choose a scope first" : secrets.loading ? "Loading…" : `None (${ssh() ? "no key" : "no login"})`}
        </option>
        <Show when={unknown()}>
          <option value={props.value} selected>
            {props.value} (not found)
          </option>
        </Show>
        <For each={choices()}>
          {(c) => (
            <option value={c.name} selected={c.name === props.value}>
              {c.label}
            </option>
          )}
        </For>
        <Show when={ssh() && props.scope}>
          <option value={NEW_KEY}>New key…</option>
        </Show>
      </select>
      <Show when={picked()?.needsGrant}>
        <span class="small muted">
          {NEEDS_GRANT_HINT}: project <code>{props.scope}</code> holds no grant of the global secret <code>{props.value}</code>. You can save it as the operator. To let a project_admin use it, run{" "}
          <code>
            terrarium secret grant {props.value} project:{props.scope}
          </code>
          .
        </span>
      </Show>
      <Show when={props.scope && !secrets.loading && !choices().length && !generating()}>
        <span class="small muted">
          No {what()} secret that this target can use{props.scope === GLOBAL ? " (a global target uses a global secret only)" : ""}.{" "}
          {ssh() ? "Pick New key…, or store one on the Secrets page." : "Store one on the Secrets page."}
        </span>
      </Show>
      <Show when={generating()}>
        <div class="card-box" style={{ padding: "8px" }}>
          <GenerateKeyForm
            scope={props.scope || GLOBAL}
            scopes={[props.scope || GLOBAL]}
            name={props.targetName ? `${props.targetName}-key` : ""}
            onCancel={() => setGenerating(false)}
            onGenerated={(s) => {
              setGenerating(false);
              setMade(s);
              void refetch();
              props.onChange(s.name);
            }}
          />
        </div>
      </Show>
      <Show when={made() && made()!.name === props.value ? made() : null}>{(s) => <PublicKey secret={s()} />}</Show>
    </div>
  );
}
